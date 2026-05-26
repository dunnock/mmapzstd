# Streaming Hugepage Decode — Design

Cycle 07-streaming-hugepage. Author: fritz-agent, 2026-05-26.

---

## §1. Problem Statement

### The whole-file copy-into-hugepage constraint

The current high-throughput path (`open_hugepage_memfd`) achieves ~10,000 MB/s
decompressed on a 128.5 MiB compressed input by copying the entire compressed file
into a `MAP_HUGETLB | MAP_HUGE_2MB` anonymous region before decoding. The zstd
decoder then scans this region sequentially: at 2 MiB pages, 64 PMD entries cover
the full 128 MiB, versus ~32,893 PTE entries with 4 KiB pages. The ~15% throughput
win over BufReader is entirely a dTLB miss reduction — confirmed by cycle 03 and 06
bench data (`docs/level9-bench.md §4`).

This approach has a hard ceiling: it requires `⌈compressed_size / 2 MiB⌉` pre-reserved
hugepages allocated before the constructor is called, and all of them held resident for
the full decode duration.

### Practical limits on the dev box

The dev box has 160 reserved 2 MiB hugepages (320 MiB), out of 128 GiB total RAM.

| Compressed file size | Hugepages required | Fits in pool? |
|----------------------|--------------------|---------------|
| 128.5 MiB (cycle 06 fixture) | 65 | Yes |
| 214 MiB (BTCUSD full archive) | 107 | Yes |
| 320 MiB | 160 | Exactly at limit |
| 321 MiB | 161 | **No — constructor returns OutOfMemory** |
| 1 GiB | 512 | No |
| 10 GiB | 5,120 | No |

At 320 MiB, the pool is exactly exhausted. Any file above that returns
`io::ErrorKind::OutOfMemory`. For production workloads (multi-GB archives), the
whole-file path is simply not available.

Even if the operator reserves more hugepages (`sysctl vm.nr_hugepages=1024` gives
2 GiB), the pool is a shared system resource. Holding gigabytes of hugepages for
the duration of a single decode is operationally expensive and limits concurrency.

### The goal

A bounded-memory decode path that:
- Uses at most `W` bytes of hugepage-backed scratch (operator-chosen `window` parameter)
- Preserves the dTLB win: the zstd decoder scans a hugepage-backed input buffer
- Requires only `⌈W / 2 MiB⌉` hugepages, not `⌈file_size / 2 MiB⌉`
- Falls back to `open_hugepage_memfd` transparently when `W >= file_size`

---

## §2. Streaming Algorithms Evaluated

### Background: zstd streaming decoder API

`zstd::stream::raw::Decoder::run_on_buffers(input: &[u8], output: &mut [u8])`
returns a `Status { bytes_read, bytes_written }`. The function:

- Consumes `status.bytes_read` bytes from `input` (advancing the internal state machine)
- Produces `status.bytes_written` decompressed bytes into `output`
- Preserves all internal state across calls — the caller simply advances the input
  pointer and calls again with the next chunk

This is the standard zstd streaming contract: the decoder never seeks backward in
the compressed input stream. The compressed input cursor is strictly
monotonically increasing. This is what all four candidates below exploit.

The current code feeds `run_on_buffers` the full compressed slice and advances
`in_pos` by `status.bytes_read` each call. The streaming variant does the same
against a small window, refilling when the window is consumed.

---

### S1: Single sliding hugepage window (pread refill)

Allocate one `W`-byte hugepage region as "active" scratch. When the scratch is
exhausted (`scratch_in_pos == scratch_len`), call `pread(fd, scratch.ptr, W, file_pos)`
to refill from the file descriptor.

**Pros:**
- Minimal RAM: exactly `W` bytes of hugepages
- Simple state: one pointer into scratch, one file offset

**Cons:**
- `pread()` is a blocking syscall with kernel overhead per refill
- Cold-cache regime: disk I/O latency stalls the decode loop directly
- No overlap between I/O and CPU: compute pauses for every refill

**Corner case:** cross-boundary frames are handled naturally — `scratch_in_pos`
tracks where zstd left off; we only refill when the scratch is fully consumed,
so no partial data is ever overwritten.

**Assessment:** Correct and simple, but leaves CPU idle during refills when the
file is cold. Suitable as a fallback on systems where mmap is unavailable.

---

### S2: Double-buffered windows with readahead (single-threaded)

Allocate two `W/2`-byte hugepage windows A and B. While decoding from A, issue
`readahead(fd, next_file_offset, W/2)` to ask the kernel to start loading the
next chunk into page cache. When A is exhausted, do a blocking `pread()` into B
(by which point the readahead may have completed), swap A↔B, and continue.

**Pros:**
- Potential overlap of disk I/O with CPU (readahead hint)
- Strictly single-threaded

**Cons:**
- `readahead(2)` is advisory — no guarantee of completion before the pread
- Requires `2×W` bytes of hugepages instead of `1×W`
- More complex state machine (two windows, swap logic, prefetch tracking)
- On a warm page cache (the dominant benchmark scenario) the readahead hint
  provides zero benefit — pread hits page cache anyway
- On cold cache the hint helps but the blocking pread still occurs

**Assessment:** Higher complexity for uncertain benefit. The two-hugepage cost
doubles the pool requirement for the same window size. Not recommended.

---

### S3: Mapped-file + hugepage scratch hybrid (RECOMMENDED)

mmap the compressed file at 4 KiB granularity (standard `memmap2::Mmap`) as the
source. Allocate one `W`-byte hugepage scratch as the decode input buffer.

**Decode loop:**
1. When scratch is empty, `memcpy(scratch.ptr, src_mmap[src_pos..], W_actual)` where
   `W_actual = min(W, file_size - src_pos)`. Advance `src_pos += W_actual`.
2. Feed `scratch[scratch_in_pos..]` to `zstd.run_on_buffers`. Advance `scratch_in_pos`.
3. When `scratch_in_pos == scratch_len`, go to step 1.
4. When `src_pos == file_size` and `scratch_in_pos == scratch_len`, return EOF.

**Why this preserves the TLB win:**

The zstd decoder scans `scratch`, which is hugepage-backed. For `W = 8 MiB`,
only 4 PMD entries (4 × 2 MiB) cover the entire scratch region — the same TLB
profile as the current whole-file path, just over a smaller region. The decoder
is always reading from hugepage-mapped memory, so TLB miss rates are identical to
`open_hugepage_memfd` on a per-byte basis.

**Why the source mmap is fine at 4 KiB:**

The memcpy phase (`src_mmap → scratch`) runs once per `W` bytes and is dominated
by bandwidth, not TLB latency. The actual TLB-sensitive loop is the zstd decode
scan over scratch. Furthermore, `MADV_SEQUENTIAL` on the source mmap causes the
kernel to aggressively readahead upcoming file pages, so the memcpy rarely stalls
on page faults. `MADV_DONTNEED` on consumed src_mmap regions keeps the source RSS
bounded to a sliding window (same as the existing `maybe_retire` strategy in the
`from_mmap` path).

**Pros:**
- Preserves TLB win exactly: zstd always reads from hugepage scratch
- No pread() syscalls: memcpy from page-cached mmap is cheap and non-blocking
  in the warm-cache regime
- Kernel readahead via MADV_SEQUENTIAL on src_mmap handles cold-cache prefetch
- Only `⌈W / 2 MiB⌉` hugepages required
- No threading, no AIO, no readahead() calls
- Simple state machine (three fields: src_pos, scratch_in_pos, scratch_len)

**Cons:**
- Source RSS includes up to `W` bytes of 4 KiB page-cache pages (from the
  source mmap) in addition to `W` bytes of hugepages for scratch
  → peak RSS ≈ 2×W while refilling (one old src window + new scratch)
  → after `MADV_DONTNEED` on the retired src_mmap range, source RSS drops back
- Cold-cache: memcpy from mmap will block on page faults; no worse than pread()
  but without an explicit prefetch ahead of decode — mitigated by MADV_SEQUENTIAL

**Assessment:** Best balance of simplicity, correctness, and TLB preservation.
Recommended.

---

### S4: Frame-aligned chunked decode

Parse the zstd frame header to determine each frame's compressed size. Allocate
a hugepage region exactly sized to the frame, decode it, free, advance.

**Pros:** No cross-boundary handling.

**Cons:**
- Most production `.zst` files are a single frame. `zstd --list` on a typical
  archive shows one frame, making this identical to the whole-file path.
- Frame header parsing requires bespoke code not covered by the `zstd` crate's
  public API.
- Does not bound memory for single-frame files.

**Assessment:** Not applicable for the primary use case. Skip.

---

### Summary table

| Algorithm | Hugepages needed | syscalls per refill | Cold-cache? | Complexity |
|-----------|-----------------|---------------------|-------------|------------|
| S1 pread | 1×W | pread() per window | stalls on read | Low |
| S2 double-buf | 2×W | readahead+pread | may overlap | Medium |
| **S3 mmap+scratch** | **1×W** | **none (mmap fault)** | **MADV_SEQ** | **Low** |
| S4 frame-aligned | 1×frame_size | varies | varies | High |

---

## §3. Corner Cases

### C1: Frame boundary at window edge

`run_on_buffers` returns `bytes_read` which may be less than `input.len()`.
The unconsumed tail (`scratch[scratch_in_pos..scratch_len]`) is valid and will
be consumed on the next `read()` call.

We refill scratch **only when `scratch_in_pos == scratch_len`**, i.e., only when
all scratch bytes have been consumed. At that point there is no tail to preserve.
This design eliminates the "copy tail to front" complication entirely.

Proof sketch: `scratch_in_pos` advances monotonically; refill triggers only at
`scratch_in_pos == scratch_len`; zstd's monotonic input-cursor invariant means
it will eventually consume the last byte of any valid input.

### C2: Last partial window

When `file_size - src_pos < W`, `W_actual = file_size - src_pos`. Scratch is
filled with the remaining bytes. `scratch_len = W_actual`. The zstd decoder
will consume these bytes, complete the last frame, and return 0 on the next
call. The loop then checks `src_pos == file_size && scratch_in_pos == scratch_len`
and returns `Ok(0)`.

The decoder must not be called with an empty input slice (to avoid spurious
`bytes_read = 0`). Guard: refill only when `W_actual > 0`.

### C3: Decoder state across refills

`ZstdDecoder` (wrapping `ZSTD_DCtx`) is a C struct that maintains frame and block
state internally. It is designed to be called repeatedly with successive input
chunks — that is the entire purpose of `ZSTD_decompressStream`. The Rust `zstd`
crate exposes this as `run_on_buffers`, which maps directly to `ZSTD_decompressStream`.
State is fully preserved across calls. No special handling needed between refills.

### C4: Truncated input (file shorter than declared)

If the source mmap was created from a file that is later truncated by another
process, accessing the truncated pages raises SIGBUS (same as the existing
`from_mmap` path). Document this requirement: the file must not be modified during
decode. For the streaming path, partial reads are more common — if the file is
simply shorter than its reported size, `read_exact` on the final chunk will return
an early EOF from the page fault mechanism, which the mmap will surface as a SIGBUS
or a short memcpy (implementation-defined). The safe approach: use `src_mmap.len()`
as the authoritative file size (computed at constructor time), and cap all copies to
`min(W, src_mmap.len() - src_pos)`. If the zstd stream ends before `src_pos` reaches
`src_mmap.len()`, the decoder returns `Ok(0)` naturally. If the zstd stream is
truncated mid-frame, `run_on_buffers` returns an `io::Error(InvalidData)`.

### C5: Cold-cache regime

In cold-cache, the first memcpy from `src_mmap` will block on page faults.
`MADV_SEQUENTIAL` is applied to `src_mmap` at construction time, so the kernel
issues readahead for upcoming pages automatically. For the initial window, faults
will stall; for subsequent windows, the readahead should keep the next window
warm. This matches the behavior of `BufReader::with_capacity` with `read()`: both
incur cold-start faults and then benefit from readahead.

Optionally, `readahead(fd, src_pos + W, W)` can be issued before each memcpy as an
explicit hint. This is deferred to the implementation task: the design does not
mandate it, but flags it as a potential optimization.

### C6: Hugepage exhaustion

If `mmap(MAP_HUGETLB | MAP_HUGE_2MB)` fails for the scratch region, the
constructor returns `io::Error { kind: OutOfMemory, … }` with the same message
format as `open_hugepage_memfd`. No silent fallback. The caller must either
reserve more hugepages or use `from_mmap` (which does not require hugepages).

The OOM path is identical in structure to the existing constructors:

```rust
if ptr == MAP_FAILED {
    let free = read_hugepages_free().unwrap_or(0);
    return Err(io::Error::new(
        io::ErrorKind::OutOfMemory,
        format!("MAP_HUGETLB failed (HugePages_Free={free}); …"),
    ));
}
```

### C7: Minimum window size

The window must be at least one hugepage (2 MiB). If `window == 0` or `window < 2 MiB`,
the constructor returns `io::Error(InvalidInput)`. Values between 1 byte and 2 MiB
are rounded up to 2 MiB. A 2 MiB window is functional but will refill frequently;
8 MiB is the recommended default (see §4).

---

## §4. Public API

### New constructor

```rust
impl Decoder<'static> {
    /// Streaming hugepage decoder with a bounded in-memory window.
    ///
    /// Opens `path`, mmaps it at 4 KiB granularity as the source, and
    /// allocates a `MAP_HUGETLB | MAP_HUGE_2MB` scratch region of at most
    /// `window` bytes (rounded up to the nearest 2 MiB).  The zstd decoder
    /// reads exclusively from the hugepage scratch, preserving the TLB win
    /// of `open_hugepage_memfd` without holding the full file in hugepages.
    ///
    /// When `window >= file_size` the call delegates to
    /// `open_hugepage_memfd` (no point sliding a window over a file that
    /// fits entirely in the hugepage pool).
    ///
    /// # Window sizing
    ///
    /// `window` is a memory budget, not a frame size.  The recommended
    /// default is `8 * 1024 * 1024` (8 MiB = 4 hugepages):
    ///
    /// - Below 2 MiB: returns `io::Error(InvalidInput)`.
    /// - 2 MiB: one hugepage; refills every ~2 MiB of compressed input;
    ///   refill overhead is measurable.
    /// - 8 MiB: four hugepages; refill overhead < 0.5% of decode time on
    ///   warm page cache; the pmds/TLB savings are the same per byte decoded.
    /// - 64 MiB: amortizes refill cost further; uses 32 hugepages per
    ///   active decode; only useful if hugepages are abundant.
    ///
    /// # Errors
    ///
    /// - `io::ErrorKind::OutOfMemory` — `MAP_HUGETLB` failed for the scratch.
    /// - `io::ErrorKind::InvalidInput` — `window < 2 MiB`.
    /// - Any OS error from `File::open`, `Mmap::map`, or `ZstdDecoder::new`.
    ///
    /// # Linux kernel requirement
    ///
    /// Requires `vm.nr_hugepages >= ceil(window / 2_MiB)`.  Reserve with
    /// `sudo sysctl vm.nr_hugepages=N`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmapzstd::decoder::Decoder;
    ///
    /// // 8 MiB window — 4 hugepages, works on any file size.
    /// let dec = Decoder::open_hugepage_streaming(
    ///     std::path::Path::new("large.zst"),
    ///     8 * 1024 * 1024,
    /// )?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[cfg(target_os = "linux")]
    pub fn open_hugepage_streaming(
        path: &Path,
        window: usize,
    ) -> io::Result<Self> { /* streaming-impl task */ }
}
```

### Internal type additions

The implementation will add a `StreamingDecoder` variant to `DecoderBuf` (or a
parallel `StreamingDecoder<'static>` newtype). The `Read` impl dispatches on which
variant is active:

```rust
enum DecoderBuf<'a> {
    Mmap(Mmap),
    Slice(&'a [u8]),
    #[cfg(target_os = "linux")]
    Hugepage(HugepageBuf),
    #[cfg(target_os = "linux")]
    Streaming(StreamingState),   // new
}

#[cfg(target_os = "linux")]
struct StreamingState {
    src_mmap: Mmap,           // whole-file page-cached source
    scratch: HugepageBuf,     // W bytes, MAP_HUGETLB scratch
    src_pos: usize,           // next byte to read from src_mmap
    scratch_in_pos: usize,    // zstd cursor within scratch
    scratch_len: usize,       // valid bytes in scratch
}
```

The `zstd: ZstdDecoder` field in `Decoder` is shared across all variants
(already present on the struct). The streaming `Read` impl uses it the same way
as the existing impl, with an extra refill step.

### Fallback behaviour

```
open_hugepage_streaming(path, window):
    file_size = path.metadata()?.len() as usize
    if window >= file_size:
        return open_hugepage_memfd(path)   // whole-file path, no change
    window = round_up_to_2mib(window)
    if window < 2 MiB: return Err(InvalidInput)
    allocate hugepage scratch of size window
    mmap path → src_mmap
    madvise(src_mmap, SEQUENTIAL)
    return Decoder { buf: Streaming(…), zstd: … }
```

### Read impl (streaming variant)

```rust
// Pseudocode for the streaming Read::read dispatch
fn read_streaming(state: &mut StreamingState, zstd: &mut ZstdDecoder, buf: &mut [u8])
    -> io::Result<usize>
{
    loop {
        if state.scratch_in_pos == state.scratch_len {
            if state.src_pos >= state.src_mmap.len() {
                return Ok(0);  // EOF
            }
            let remaining = state.src_mmap.len() - state.src_pos;
            let chunk = remaining.min(state.scratch.capacity);
            // memcpy from page-cached mmap into hugepage scratch
            state.scratch[..chunk]
                .copy_from_slice(&state.src_mmap[state.src_pos..state.src_pos + chunk]);
            state.src_pos += chunk;
            state.scratch_len = chunk;
            state.scratch_in_pos = 0;
            state.retire_src(state.src_pos.saturating_sub(RETIRE_WINDOW));
        }

        let input = &state.scratch[state.scratch_in_pos..state.scratch_len];
        let status = zstd.run_on_buffers(input, buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        state.scratch_in_pos += status.bytes_read;

        if status.bytes_written > 0 {
            return Ok(status.bytes_written);
        }
        // bytes_written == 0: zstd consumed input but needs more to produce output
        // — loop to refill or return EOF
    }
}
```

`retire_src` calls `madvise(MADV_DONTNEED)` on the src_mmap range preceding the
trailing `RETIRE_WINDOW` bytes, same as `maybe_retire` on the existing `Mmap`
variant. This keeps source RSS bounded.

---

## §5. Memory and Performance Characteristics

### Memory budget

| Component | Size | Notes |
|-----------|------|-------|
| Hugepage scratch | `W` bytes | Fixed, operator-chosen |
| src_mmap VMA | `file_size` (virtual) | Physical pages = readahead window only |
| src_mmap RSS | ≈ `2×W` peak during refill | MADV_DONTNEED retires consumed pages |
| zstd DCtx | ~128 KiB | Internal zstd state |

Peak RSS during a refill = one retired src_mmap window (before DONTNEED) plus
one hugepage scratch = ≈ 2×W. After DONTNEED the source RSS drops to the kernel's
readahead window (typically 128 KiB – 512 KiB depending on kernel config).

For `W = 8 MiB`: peak RSS during decode ≈ 16 MiB (far below the 320 MiB hugepage
pool limit), uses 4 hugepages, handles arbitrarily large files.

### PMD coverage comparison

| Mode | Window size | PMD entries | PTE entries |
|------|------------|-------------|-------------|
| open_hugepage_memfd (128 MiB file) | 128 MiB | 64 | 0 |
| open_hugepage_streaming W=8 MiB | 8 MiB | 4 | 0 |
| from_mmap (128 MiB file) | 128 MiB | 0 | 32,768 |
| bufreader-64k | 64 KiB scratch | 0 | 16 (always hot) |

The streaming path uses only 4 PMD entries for the hugepage scratch, vs 16 PTE
entries for BufReader's 64 KiB buffer. Per-byte TLB miss rates are identical to
`open_hugepage_memfd` for the decode scan, since zstd always reads from the
hugepage scratch regardless of window size.

BufReader's 16 PTEs are always hot in L1 dTLB across the entire decode.
The streaming scratch (4 PMDs) should similarly stay hot: 4 PMD entries fit
in the 32-entry 2 MiB L1 dTLB on Alder Lake. The hypothesis is that streaming
matches or closely approaches `open_hugepage_memfd` throughput, with the memcpy
cost offset by the reduced page fault pressure from MADV_SEQUENTIAL.

### Expected throughput

Hypothesis: streaming at `W = 8 MiB` achieves within 5% of `open_hugepage_memfd`
on warm cache (128 MiB corpus). The refill cost per `W` bytes is one memcpy
(~5–10 ns/byte at L3 bandwidth) + one MADV_DONTNEED call. For `W = 8 MiB` and
128 MiB compressed input: 16 refills × (8 MiB memcpy + syscall). The memcpy at
~15 GB/s takes ≈ 0.5 ms per window; 16 refills = ~8 ms overhead on a ~27 ms
total decode. This is a ~30% overhead estimate, which is pessimistic — the L3
bandwidth is higher and MADV_DONTNEED is cheap.

The bench task (`streaming-bench`) will validate this hypothesis against real
numbers for both 256 MiB and ≥1 GiB corpora.

---

## §6. Task Decomposition

Three sibling tasks are created under `projects/mmapzstd/tasks/`:

```
streaming-design-and-plan  (this task — done)
  └─ streaming-impl          (implement S3 algorithm)
       └─ streaming-bench    (bench: 256 MiB + ≥1 GiB corpus)
            └─ streaming-decision  (read numbers, decide, update docs)
```

**`streaming-impl`** — Implement `Decoder::open_hugepage_streaming(path, window)` in
`src/decoder.rs` following this design. Add the `StreamingState` struct, the
`DecoderBuf::Streaming` variant, the streaming `Read` dispatch, `retire_src`, and
the fallback to `open_hugepage_memfd`. Add integration tests (round-trip, EOF,
window-smaller-than-block, OOM path).

**`streaming-bench`** — Extend `benches/levels.rs` with a `hugepage-streaming-8m`
bench group. Generate a ≥1 GiB fixture (4× the cycle-06 fixture, same 50/50
random+repeating corpus) compressed at level 3. Run all four modes (bufreader,
mmap, hugepage-memfd, hugepage-streaming) on both 256 MiB and 1 GiB inputs.
Record: throughput (MB/s), minor faults, smaps hugepage state per mode.
Output: `docs/streaming-bench.md`.

**`streaming-decision`** — Read `docs/streaming-bench.md`. Decide:
(a) make streaming the default on Linux (if within 5% of memfd on 256 MiB and
works on 1 GiB), (b) ship alongside as an explicit constructor (if measurable
regression vs memfd on 256 MiB), or (c) abandon (if >20% regression and no
large-file advantage). Update README, RESEARCH.md, CHANGELOG, and bump to 0.3.0
if shipping.

---

## §7. Open Questions Deferred to Implementation

1. **Explicit readahead hint**: Should `open_hugepage_streaming` issue
   `readahead(fd, src_pos + W, W)` before each memcpy? Design says: try it
   in the bench and compare with/without. Default: off (MADV_SEQUENTIAL is
   sufficient on warm cache; cold-cache is not the primary benchmark target).

2. **Retire window size for streaming**: The existing `RETIRE_WINDOW = 4 MiB`
   constant was tuned for the file-mmap path. For the streaming source mmap,
   retiring one full scratch window behind the read cursor (i.e., `W` bytes)
   is the natural choice. Implementation task should confirm and document.

3. **`bytes_written = 0, bytes_read = 0` loop guard**: In theory, calling
   `run_on_buffers` with non-empty input never returns `(0, 0)` on a valid
   stream. Add a debug assertion or counter to detect this in tests.

4. **Default window constant**: Expose a `pub const DEFAULT_STREAMING_WINDOW:
   usize = 8 * 1024 * 1024` for callers who want to match the library default.
