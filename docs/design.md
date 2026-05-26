# mmapzstd — Design

## Public API

```rust
use std::io::{self, Read};
use std::path::Path;
use memmap2::Mmap;

/// Decompresses a zstd-compressed file via a memory-mapped region.
///
/// `'a` is the lifetime of the backing byte slice. The owned constructors
/// (`from_mmap`, `open_hugepage`, `open_hugepage_memfd`) store the buffer
/// inside the struct; they return `Decoder<'static>`. The `'a` parameter
/// enables `from_slice(data: &'a [u8])` for caller-managed backing.
pub struct Decoder<'a> { /* private */ }

impl<'a> Read for Decoder<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

impl Decoder<'static> {
    /// Take ownership of `mmap` and prepare the zstd stream.
    pub fn from_mmap(mmap: Mmap) -> io::Result<Self>;

    /// Load compressed data into a MAP_HUGETLB anon buffer (Linux only).
    #[cfg(target_os = "linux")]
    pub fn open_hugepage(path: &Path) -> io::Result<Self>;

    /// Load compressed data via memfd_create(MFD_HUGETLB) (Linux ≥ 4.14 only).
    #[cfg(target_os = "linux")]
    pub fn open_hugepage_memfd(path: &Path) -> io::Result<Self>;
}

impl<'a> Decoder<'a> {
    /// Borrow compressed data from a caller-supplied slice.
    pub fn from_slice(data: &'a [u8]) -> io::Result<Self>;
}
```

Note: `Decoder::open(path)` was removed in 0.2.0. Callers who want a portable
file-mmap path should build a `memmap2::Mmap` and pass it through `from_mmap`.

**Lifetime story.** Owned constructors store the backing buffer inside `Decoder`.
The inner zstd decoder holds a raw `*const [u8]` into that buffer; because the
buffer is owned inside `Decoder` and never exposed by `&mut self`, those bytes
are stable for the struct's lifetime. `Decoder` is `!Send` and `!Sync` (raw
pointer; add explicit `unsafe impl` only if benchmarked on a multi-core
harness, which is out of scope for this crate).

**Error contract.** Constructors return `io::Error` for OS-level failures
(file not found, mmap permission error, zstd header parse error).
`Read::read` returns `io::Error` for zstd decode errors
(`io::ErrorKind::InvalidData`) and for SIGBUS due to file truncation
(see Risks).

---

## mmap Configuration

Applied in the constructor after `MmapOptions::new().map(&file)?`:

| Step | Call | Reason |
|------|------|--------|
| 1 | `mmap.advise(Advice::Sequential)?` | Tells the kernel we will access pages
in order; triggers aggressive readahead. |
| 2 | `mmap.advise(Advice::HugePage)?` | Opts in to transparent huge pages (THP)
on Linux. The kernel promotes 4 KiB pages to 2 MiB pages opportunistically,
reducing TLB pressure. No-op on kernels without THP or on non-Linux. |

**MAP_HUGETLB vs THP.** `MAP_HUGETLB` requires pre-allocated huge pages in the
kernel pool (`/proc/sys/vm/nr_hugepages`). We do not use it: the library has
no control over system configuration and the `mmap(MAP_HUGETLB)` call silently
falls back (or errors) if the pool is empty. `MADV_HUGEPAGE` (THP) is always
safe — the kernel chooses whether to promote.

**MAP_POPULATE vs lazy faults.** We do *not* use `MAP_POPULATE` or
`MADV_POPULATE_READ`. Measurements showed ~0.5% wall-time contribution on the
hugepage path — below criterion noise. Both were removed in 0.2.0 along with
`Decoder::open`. With `MADV_SEQUENTIAL`, the kernel's readahead covers the
upcoming window; lazy faulting keeps RSS bounded.

---

## Page-Retirement Strategy

**Window size: 4 MiB** (`RETIRE_WINDOW = 4 * 1024 * 1024`).

After each `read()` call, the decoder checks whether the zstd cursor has
advanced past `retire_frontier + RETIRE_WINDOW`. If so:

```
let new_frontier = cursor_pos.saturating_sub(RETIRE_WINDOW);
// align down to page boundary (4096)
let new_frontier = new_frontier & !(4096 - 1);
if new_frontier > retire_frontier {
    madvise(mmap_ptr + retire_frontier,
            new_frontier - retire_frontier,
            MADV_DONTNEED);
    retire_frontier = new_frontier;
}
```

**Trade-offs.**
- 4 MiB >> zstd's default window size (default 8 MiB for level 3, but
  back-references within a frame are bounded by the window; 4 MiB trailing
  buffer is usually sufficient). If the zstd file was compressed with a
  window size > 4 MiB, the decoder may refault a retired page. In practice,
  `zstd -3` defaults to an 8 MiB window, so we will not retire pages
  actively used by back-references. See Risk 2 below.
- Larger window (e.g. 16 MiB) → safer for large window sizes, more RSS.
- Smaller window (e.g. 512 KiB) → lower RSS, risk of re-faulting for large
  window files.
- A future `DecoderOptions::retire_window(usize)` builder can expose tuning.

`MADV_DONTNEED` is used (not `MADV_FREE`): it immediately marks pages
reclaimable on Linux, which is what we want for RSS benchmarking.

---

## Backpressure

The zstd streaming decoder (`zstd::stream::Decoder<R: Read>`) pulls bytes from
its inner `R` only when it needs more compressed input to fill an output block.
We feed it a `Cursor<&[u8]>` over the mmap region.

```
caller calls Decoder::read(&mut buf)
  └─> zstd::Decoder::read(&mut buf)
        └─> when compressed input exhausted:
              calls Cursor::read() → copies next bytes from mmap slice
              (kernel faults in the page if not resident)
        └─> decompresses into buf directly; returns bytes_written
```

No internal output queue beyond the zstd frame buffer (≤ 128 KiB per block).
The caller drives the pace: if it stops calling `read`, the zstd decoder stops
pulling from the mmap and the kernel stops faulting pages. This is the
backpressure contract.

The retirement step runs at the end of each `Decoder::read` call (not inside
the zstd inner read), to avoid issuing madvise for every small cursor advance.

---

## Risks

| # | Risk | Mitigation |
|---|------|------------|
| 1 | **File shorter than hugepage size (< 2 MiB).** `MADV_HUGEPAGE` on a tiny
mapping is a kernel no-op; THP requires at least one naturally-aligned 2 MiB
region. | No action needed — the kernel silently ignores the hint. No error is
raised. Small files decompress correctly with 4 KiB pages. |
| 2 | **zstd frame compressed with window size > `RETIRE_WINDOW` (4 MiB).**
A back-reference in the zstd stream could require reading bytes before the
retirement frontier. `zstd` the library handles this internally (its
decompressed output window, not the compressed input window). Retiring
compressed-input pages is safe as long as the zstd *input* cursor never
goes backward — and it never does in streaming mode. The compressed bytes
are consumed once; only the *decompressed* output window needs to persist
(held by libzstd internally). | Confirm: zstd streaming never seeks backward
in the compressed input. The compressed input cursor is monotonically
increasing. Retire pages safely. |
| 3 | **mmap fault on truncated input (SIGBUS).** If the file is truncated by
another process after the mmap is created, a page fault within the now-absent
region raises `SIGBUS`, which by default terminates the process. We cannot
install a `SIGBUS` handler inside a library without polluting the caller's
signal table. | Document the requirement: the file must not be modified or
truncated during decoding. The constructor records `mmap.len()` and the zstd
decoder will return `InvalidData` for a cleanly-truncated stream; SIGBUS is
only triggered by a torn write. Users who need protection should pass a
snapshot copy. |
