# Performance Hypotheses: mmap Decoder vs BufReader Baseline

**Starting point:** mmap decoder is 24% slower (37.95 ms vs 30.67 ms, 256 MiB corpus).  
**Key signals:** 4.3× more minor faults (2666 vs 623); no major faults; file is page-cached.

See `docs/perf-baseline.md` for the full profiling data.

---

## Hypothesis 1: Eliminate the Overflow Buffer Copy

**Predicted mechanism**

The current decoder unconditionally decompresses into a 128 KiB `self.overflow` staging buffer and then copies into the caller's buffer. `std::io::copy` uses an 8 KiB internal buffer, so for every 128 KiB of output, 1 `run_on_buffers()` call is followed by 16 drain calls copying 8 KiB each from `overflow`. This produces ~256 MiB of redundant cache traffic per full decode (128 KiB × 2048 decompression calls × 1 copy out). At ~100 GB/s L2 bandwidth (i9-12900K), those 256 MiB cost roughly 2.5 ms — matching about one-third of the observed gap.

The fix: when `buf.len() >= BLOCK_SIZE`, pass `buf` directly to `run_on_buffers()` as the output; bypass `overflow` entirely. Fall back to the overflow path only when `buf` is smaller than `BLOCK_SIZE`.

**Expected delta**

Up to 20% if the copy is cache-bandwidth-bound, less if L2 is fast enough that the copy hides behind decompression latency.

**How to test**

1. Add a fast path in `Decoder::read`: if `buf.len() >= BLOCK_SIZE` AND `overflow` is empty, call `self.zstd.run_on_buffers(&self.mmap[self.in_pos..], buf)` and return immediately after updating `in_pos`.
2. Wrap the benchmark with the same `io::copy` + `sink()` harness (8 KiB buffer → fast path not triggered, tests fallback). Also test with a 256 KiB caller buffer (fast path triggered). Watch wall-clock delta and check `cache-misses` with `perf` if available.

**Risk**

- Correctness: `run_on_buffers` may write fewer bytes than `buf.len()`, leaving the tail uninitialised. Must use only `status.bytes_written`. Unit tests (`small_buf_exercises_overflow`) will cover this.
- Regression for very-small-buffer callers: no change in that path.

**Cost**

No new dependencies. No `unsafe`. Pure Rust, ~10 lines of change in `read()`.

---

## Hypothesis 2: `MAP_POPULATE` / `MADV_POPULATE_READ` — Batch All Minor Faults at Open Time

**Predicted mechanism**

The mmap decoder incurs 2666 minor faults scattered throughout the decode loop. Each fault interrupts the decompressor to update the process's page table. With `MADV_POPULATE_READ` (Linux ≥ 5.14) or `MAP_POPULATE` at mmap time, all page-table entries are populated in one kernel call before the first byte is decoded. The kernel can service the faults in batch, use larger TLB shootdowns more efficiently, and enable the hardware prefetcher to see a fully-mapped region immediately.

The extra faults incurred by mmap vs BufReader: 2666 − 623 = 2043 faults. At ~2 μs each = ~4 ms — roughly half the observed gap.

**Expected delta**

Up to 15–20% if minor-fault latency is the primary bottleneck. On a page-cached file the kernel does not need disk I/O, so POPULATE is cheap. Overhead at open time: ~10 ms for 130 MiB / 4 KiB pages, partially hiding in the fixture-generation phase if the caller warm-starts the decoder.

**How to test**

In `apply_madvise()` (or `from_mmap()`), add:
```rust
#[cfg(target_os = "linux")]
mmap.advise(Advice::PopulateRead)?; // requires memmap2 ≥ 0.9.3
```
Alternatively gate on a builder flag `prefault: bool`. Compare minor-fault count (should drop to near zero during decode) and wall-clock time. Also try `MAP_POPULATE` via `MmapOptions::populate()`.

**Risk**

- First-open latency increases proportionally to file size.
- Thrashes the page cache on large files that don't fit in RAM (the primary reason the mmap path exists).
- On cold cache (major faults expected), POPULATE forces all disk I/O before the decoder starts, blocking backpressure. This is a workload regression.

**Cost**

No new dependencies. `memmap2::Advice::PopulateRead` is already in the dependency. Feature-flag it if the cold-cache use case matters.

---

## Hypothesis 3: `MAP_HUGETLB` / Transparent Huge Pages — Collapse TLB Entries

**Predicted mechanism**

The compressed file (~130 MiB) requires ~33,280 page-table entries at 4 KiB pages. The i9-12900K dTLB has 64 entries (L1) and ~1536 entries (L2). For a sequential scan of 130 MiB, the TLB must be refilled ~512 times per full pass. With 2 MiB huge pages the same file needs only 65 entries — fitting entirely in the L2 dTLB. TLB misses would drop from potentially thousands per decode to near zero.

The code already calls `MADV_HUGEPAGE`. THP policy is `madvise` on this machine, so the hint is registered. However, `HugePages_Total: 0` means no static huge pages are pre-allocated. File-backed read-only mmaps may or may not receive THP promotion in the running kernel (support was added in Linux 6.6 for read-only file mappings). Without `perf dTLB-load-misses` it is unknown whether THP is actually active for this mapping.

Confirmation step: read `/proc/<pid>/smaps` and check `AnonHugePages:` vs `ShmemPmdMapped:` for the mmap region.

**Expected delta**

If THP is NOT currently active: up to 20% from eliminating dTLB refills. If THP is already active (likely given `FileHugePages: 4.3 GiB` on the system): marginal.

**How to test**

1. Check `/proc/self/smaps` during decode to confirm whether huge pages are in use for the mapping.
2. If not: try `MAP_HUGETLB | MAP_HUGE_2MB` via a custom `MmapOptions` (requires pre-allocated huge pages: `sysctl vm.nr_hugepages=128`).
3. Alternatively, use `madvise(MADV_HUGEPAGE)` + `madvise(MADV_POPULATE_READ)` to force THP promotion at open time.
4. Watch `dTLB-load-misses` counter with `perf stat`.

**Risk**

- `MAP_HUGETLB` fails if `vm.nr_hugepages` is 0; must gracefully fall back to 4 KiB pages.
- Huge pages consume physically contiguous 2 MiB blocks — may cause OOM under memory pressure.
- On small files (< 2 MiB) the overhead of reserving a huge page outweighs the TLB benefit.

**Cost**

No new dependencies. Need `unsafe` only if bypassing `memmap2`'s safe API. `MmapOptions` exposes `huge(usize)` for huge-page sizes. Feature-flag recommended for fallback.

---

## Hypothesis 4: `MADV_WILLNEED` Rolling Window — Explicit Read-Ahead Ahead of Decode Position

**Predicted mechanism**

`MADV_SEQUENTIAL` tells the kernel to apply its own heuristic read-ahead on sequential access. `MADV_WILLNEED` is a more explicit directive: it tells the kernel to immediately start I/O for a specific range. By issuing `madvise(MADV_WILLNEED, in_pos + RETIRE_WINDOW, N_MiB)` before each decode call, we kick the kernel's read-ahead harder and can tune the lookahead independently of the kernel's heuristic.

On a page-cached file this has minimal I/O benefit (no disk needed), but it can cause the kernel to populate page-table entries earlier and may improve TLB pre-warming by triggering the hardware prefetcher on the mmap region.

**Expected delta**

Marginal (1–5%) on page-cached input where the kernel's sequential read-ahead already keeps pages hot. Higher benefit expected on a cold-cache workload where the file is not resident.

**How to test**

In `maybe_retire()`, add a forward `MADV_WILLNEED` advisory on `[in_pos + RETIRE_WINDOW, in_pos + 2 * RETIRE_WINDOW)` after the DONTNEED retirement. Use `memmap2::Advice::WillNeed`. Measure wall-clock on warm-cache runs; also test on a cold cache (use `echo 3 > /proc/sys/vm/drop_caches` between runs).

**Risk**

- On small files or files smaller than the WILLNEED window, this becomes a no-op.
- On cold cache with compressed input larger than RAM, WILLNEED may pull in too much too eagerly, competing with the DONTNEED window's retirement.
- Can cause latency spikes if the kernel blocks on the hint.

**Cost**

No new dependencies, no `unsafe`. ~5 lines in `maybe_retire()`.

---

## Hypothesis 5: Larger Output Block Size to Reduce `run_on_buffers` Call Count

**Predicted mechanism**

The current `BLOCK_SIZE = 128 KiB` (zstd's maximum uncompressed block size). Every call to `run_on_buffers()` has a fixed overhead: FFI boundary crossing into libzstd, stack frame setup, and the internal zstd streaming state check. For 256 MiB output at 128 KiB per call, we make 2048 calls. Increasing `BLOCK_SIZE` to 1 MiB or 4 MiB would reduce calls to 256 or 64.

However, zstd's streaming decompressor is bounded by its internal block size (max 128 KiB per compressed block for standard frames). Passing a larger output buffer simply causes zstd to loop internally and fill as much as it can. The FFI overhead saving is real but small. The larger benefit is that a bigger output buffer means fewer read-overflow-drain cycles: with a 1 MiB overflow and an 8 KiB caller buffer, we still do 128 drains per decompress call, but the total number of `run_on_buffers` FFI calls drops to 256.

Combined with Hypothesis 1 (direct-to-caller writing), a larger `BLOCK_SIZE` helps when the caller provides a large buffer (e.g., in a custom sink using 1 MiB reads).

**Expected delta**

5–10% from reduced FFI overhead alone. Up to 20% when combined with the direct-buffer fast path (Hypothesis 1) and a large caller buffer.

**How to test**

Change `BLOCK_SIZE` from `128 * 1024` to `1024 * 1024`. Verify `round_trip` and `small_buf_exercises_overflow` still pass. Benchmark with both the existing 8 KiB `io::copy` harness and a custom sink using `read_exact(buf)` with `buf.len() = BLOCK_SIZE`. Watch wall-clock and minor-fault count.

**Risk**

- Larger `overflow` allocation. With `BLOCK_SIZE = 1 MiB`, the decoder holds a 1 MiB Vec permanently. May be undesirable for callers embedding many decoder instances.
- If zstd outputs less than BLOCK_SIZE per call (common when compressed blocks are small), the allocation is wasted. Consider using a dynamically-sized output target.

**Cost**

One constant change. No new dependencies. Zero `unsafe`.

---

## Hypothesis 6: `MADV_FREE` Instead of `MADV_DONTNEED` for the Trailing Window

**Predicted mechanism**

`MADV_DONTNEED` on a file-backed read-only mapping immediately drops pages from the process's resident set, forcing re-faults from the page cache on any future access. `MADV_FREE` delays reclaim until memory pressure occurs. For a strictly-forward sequential scan, pages behind the decode cursor are never re-accessed, so the choice between DONTNEED and FREE should make no difference to correctness or to the number of faults. The difference is overhead at the syscall site: DONTNEED forces immediate TLB shootdowns for the retired range, while FREE defers them.

**Important constraint:** `MADV_FREE` only applies to private anonymous pages (MAP_PRIVATE | MAP_ANONYMOUS). The mmap decoder uses a file-backed read-only mapping (`Mmap::map(&file)`). The Linux kernel silently ignores `MADV_FREE` on file-backed mappings; it does not return an error. Therefore MADV_FREE provides NO benefit for this use case.

**Expected delta**

Zero. Not applicable to file-backed mappings.

**How to test**

Swap `UncheckedAdvice::DontNeed` for `UncheckedAdvice::Free` in `maybe_retire()`. Confirm via `/proc/<pid>/smaps` that RSS drops at the same rate as with DONTNEED. Benchmark should show no difference.

**Risk**

None (no-op). The risk of *shipping* this change is confusion — future maintainers may not know MADV_FREE is silently ignored on file-backed mmaps. Add a comment if this is tried.

**Cost**

Trivial code change, but expected to have no effect. Low priority to test.

---

## Hypothesis 7: Software Prefetch (`_mm_prefetch`) Inside the Decode Loop

**Predicted mechanism**

A software prefetch instruction emitted just before `run_on_buffers()` could bring the next segment of compressed input into L1/L2 cache before the CPU reaches it. For a sequential stream the hardware stream prefetcher (HSP) already detects and prefetches ahead autonomously, so this is likely redundant. However, if the mmap accesses have irregular strides within a 128 KiB compressed block (zstd's internal offset tables, match tables), the HSP may stall. A targeted `_mm_prefetch` 256–512 bytes ahead of `self.in_pos` could cover these gaps.

**Expected delta**

Marginal or slightly negative. The i9-12900K's HSP is highly effective for sequential streams. Software prefetch may conflict with the HSP and reduce bandwidth. Worth a 30-minute experiment but low confidence.

**How to test**

In `read()`, before calling `run_on_buffers`, add:
```rust
#[cfg(target_arch = "x86_64")]
unsafe {
    use std::arch::x86_64::_mm_prefetch;
    let ptr = self.mmap.as_ptr().add(self.in_pos + 256) as *const i8;
    _mm_prefetch(ptr, std::arch::x86_64::_MM_HINT_T0);
}
```
Watch `cache-misses` and `cycles` under `perf stat`. If `cache-misses` drop but wall-clock doesn't improve, the HSP was already handling it.

**Risk**

May interfere with the hardware prefetcher and regress streaming throughput. Only safe on `x86_64`; must be feature-gated.

**Cost**

~5 lines, `unsafe` block required, `target_arch = "x86_64"` guard required. No new dependencies.

---

## Priority Order

| Priority | Hypothesis | Expected delta | Confidence |
|---|---|---|---|
| 1 | H1: Eliminate overflow copy | up to 20% | High — addresses root cause identified in code |
| 2 | H2: MAP_POPULATE / MADV_POPULATE_READ | up to 15% | Medium — fault overhead is measurable |
| 3 | H3: MAP_HUGETLB / THP confirmation | up to 20% | Medium — THP may already be active |
| 4 | H5: Larger BLOCK_SIZE | 5–10% | Medium — reduces FFI overhead |
| 5 | H4: MADV_WILLNEED rolling window | 1–5% | Low on warm cache |
| 6 | H7: Software prefetch | marginal | Low — HSP handles sequential |
| 7 | H6: MADV_FREE | 0% | Not applicable to file-backed mappings |

H1 and H2 can be combined in one patch. H3 requires a smaps inspection first. H5 synergizes with H1.

---

## Literature References

- [Latency Implications of Virtual Memory — Erik Rigtorp](https://rigtorp.se/virtual-memory/): Minor faults on file-backed mmaps trigger "fault-around" (pre-faulting ~16 nearby pages), explaining why 2666 faults cover far more than 2666 × 4 KiB of input.
- [madvise(2) Linux manual](https://man7.org/linux/man-pages/man2/madvise.2.html): MADV_FREE applies only to private anonymous pages; silently ignored on file-backed mappings.
- [Using Huge Pages on Linux — Erik Rigtorp](https://rigtorp.se/hugepages/): 200× fewer TLB entries with 2 MiB pages; MAP_HUGETLB requires pre-reserved huge pages.
- [MAP_HUGETLB vs MADV_HUGEPAGE — LinuxVox](https://linuxvox.com/blog/using-mmap-and-madvise-for-huge-pages/): MAP_HUGETLB fails if no huge pages reserved; MADV_HUGEPAGE is a hint with no guarantee.
- [Skip TLB flushes for reused pages within mmaps — arXiv 2409.10946](https://arxiv.org/html/2409.10946v1): TLB shootdowns from mmap cycles cost up to 30% compute throughput; eliminating them yields up to 92% microbenchmark improvement.
- [mmap() vs read() — Daniel Lemire](https://lemire.me/blog/2012/06/26/which-is-fastest-read-fread-ifstream-or-mmap/): Sequential reads with read() consistently match or beat mmap() on Linux due to readahead and copy avoidance.
- [LWN — Relaxed TLB flushes](https://lwn.net/Articles/901751/): TLB miss cost is ~20–60 cycles per page walk; shootdowns are the primary scalability bottleneck for mmap-heavy workloads.
- [zstd manual — streaming API](https://facebook.github.io/zstd/zstd_manual.html): ZSTD_decompressStream processes input in compressed-block units (max 128 KiB); passing a larger output buffer causes internal looping with no correctness issue.
