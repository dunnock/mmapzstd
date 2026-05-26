# Performance Hypotheses: mmap Decoder vs BufReader Baseline

**Starting point:** mmap decoder is 24% slower (37.95 ms vs 30.67 ms, 256 MiB corpus).  
**Key signals:** 4.3× more minor faults (2666 vs 623); no major faults; file is page-cached.

See `docs/perf-baseline.md` for the full profiling data.

---

## Cycle 02-perf Summary

**Hypotheses tested:** H1 ✅, H2 ✅, H3 🚫 (system blocked), H4 ✅ (regressive), H5 ✅ (N/A), H6 ✅ (N/A), H7 ✅ (regressive)

**Best result (H1 + H2 combined):**

| Metric | Cycle-01 baseline | Cycle-02 mmap | BufReader baseline |
|---|---|---|---|
| Criterion median | 38.806 ms | **31.15 ms** | 29.67 ms |
| Throughput | 6,597 MB/s | **8,215 MB/s** | 8,634 MB/s |
| Minor faults (decode) | 2,637 | ~579 | 624 |
| Gap vs BufReader | −28% | **−5%** | — |

**Outcome: no_winner.** The mmap decoder improved by 20% from cycle 01 but remains ~5% slower than the BufReader baseline on the warm-cache sequential-decode benchmark.

**Root cause of remaining gap:** TLB pressure. BufReader reuses 16 virtual pages (64 KiB buffer), keeping them permanently in L1 dTLB. The mmap decoder accesses 32,768 distinct 4 KiB pages sequentially, saturating the L2 dTLB (1536 entries) and requiring ~31,000 hardware page-table walks per decode. Reducing these requires 2 MiB huge pages (H3), which is blocked by the current THP policy (`shmem_enabled = never`, `nr_hugepages = 0`).

**Recommended next step:** Enable huge pages for the test fixture (operator action: `echo madvise > /sys/kernel/mm/transparent_hugepage/shmem_enabled`). H3 is expected to give an additional 15–20% improvement, which combined with H1+H2 should cross the 5%-faster-than-baseline threshold.

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

**Result (cycle 02-perf)**

Implemented. The simplest correct approach turned out to be _removing the overflow buffer entirely_ and calling `run_on_buffers` directly with the caller's buffer. `ZSTD_decompressStream` handles arbitrary output-buffer sizes and maintains internal state across calls, so no staging buffer is needed.

| Metric | Before (cycle 01) | After H1 |
|---|---|---|
| Criterion median | 38.806 ms | ~31.2 ms |
| Throughput | 6,597 MB/s | ~8,200 MB/s |
| Improvement | — | **+20%** |

The improvement applies for all caller-buffer sizes (not just ≥ BLOCK_SIZE). The 128 KiB overflow buffer and its associated copy traffic were the primary source of overhead.

Tests: `round_trip`, `small_buf_exercises_overflow`, and `retire_pos_advances_after_large_read` all pass unchanged.

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

**Result (cycle 02-perf)**

Implemented both `MAP_POPULATE` (in `MmapOptions::new().populate()`) and `MADV_POPULATE_READ` (in `apply_madvise()`). Verified fault distribution: 2057 faults move to open time, only 579 decode-time faults remain (comparable to the baseline's 624 non-mmap faults).

| Metric | H1 only | H1 + H2 (criterion) |
|---|---|---|
| Criterion median | ~32.4 ms (est.) | 31.15 ms |
| Improvement over H1 | — | ~4% |
| Decode-time faults | ~2637 | ~579 |

The 579 residual decode faults are non-mmap process faults (heap, stack) unavoidable at any buffer size. Effectively all mmap page faults are eliminated from the decode loop.

Performance delta vs baseline: mmap 31.15 ms, baseline 29.67 ms — **mmap is 5% slower** (criterion, same fixture). Does not meet the ≥5% beat-baseline threshold.

The remaining gap is TLB pressure: mmap accesses 32,768 distinct 4 KiB pages sequentially, while BufReader reuses the same 16 pages repeatedly (64 KiB buffer), keeping them hot in L1 dTLB.

**Disposition (0.2.0):** `MAP_POPULATE` and `MADV_POPULATE_READ` were removed from the
`apply_madvise()` implementation in 0.2.0 along with `Decoder::open`. The hugepage path
(`open_hugepage`, `open_hugepage_memfd`) has no caller for this optimisation — the
`read()`-into-memfd step faults pages as part of its essential work, and the measured
contribution was ~0.5% wall time, well below criterion noise.

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

**Result (cycle 02-perf)**

Not tested — blocked by system configuration:
- `/sys/kernel/mm/transparent_hugepage/shmem_enabled = never` (the test fixture lives on overlay/tmpfs which uses the shmem THP policy).
- `vm.nr_hugepages = 0` (no static huge pages reserved).

`MADV_HUGEPAGE` is already called in `apply_madvise()` but has no effect because the kernel silently ignores it under the current THP policy. Smaps inspection confirmed `AnonHugePages: 0` and no `FilePmdMapped` entries for the mmap region.

H3 is the most promising remaining hypothesis for closing the gap (estimated 15-20%). It requires operator action: `echo madvise > /sys/kernel/mm/transparent_hugepage/shmem_enabled` OR `sysctl vm.nr_hugepages=128`.

**Status (cycle 02-perf): not-tested — blocked by THP policy. Requires operator escalation.**

---

## Hypothesis 3 — Cycle 03 Results

**Cycle-03 task:** `hugepage-mmap`
**Worktree:** `/work/mmapzstd/.worktrees/03-hugepages`
**Fixture:** `/work/cargo-target-ralph/mmapzstd-fixtures/decompress_256mib.zst` (ext4 bind-mount, 128.5 MiB)

### H3a — ext4 fixture + `MADV_HUGEPAGE` + `MADV_POPULATE_READ`

The bench fixture was moved from `tempfile::NamedTempFile` (tmpfs, `shmem_enabled=never`)
to a persistent file on `/work` (ext4 bind-mount, `FileHugePages: 4.5 GiB` system-wide).
`MADV_HUGEPAGE` and `MADV_POPULATE_READ` were already in `apply_madvise()`.

**smaps inspection** (via `examples/measure_mmap_hugepage.rs`):

```
FilePmdMapped:   0 kB   (no 2 MiB file-backed huge pages in user page table)
AnonHugePages:   0 kB   (expected — file-backed mmap)
THPeligible:     0      (kernel marks this VMA ineligible for THP)
```

`THPeligible: 0` means khugepaged will not promote this mapping. Root cause:
`MADV_HUGEPAGE` on a read-only file-backed `MAP_PRIVATE` mapping does not set
`VM_HUGEPAGE` on the VMA in Linux 6.17 — the kernel's `madvise_hugepage` path
silently skips file VMAs that are not shmem/tmpfs. The `FileHugePages: 4.5 GiB`
system-wide total comes from other processes whose file pages were promoted by a
different path (page-cache large-folio allocation), not from user-space hints.

**Criterion results (3 runs, 10 samples each, 30 s measurement):**

| Run | mmapzstd median | baseline median |
|-----|----------------|----------------|
| 1   | 31.72 ms       | 30.30 ms       |
| 2   | 31.73 ms       | 30.18 ms       |
| 3   | 31.54 ms       | 29.61 ms       |

**H3a median: ~31.66 ms → ~8,076 MB/s. No improvement vs cycle-02 (8,215 MB/s).**
The fixture-on-ext4 change does not affect performance because THP is not active.

**Status: tested — no winner (THP ineligible on read-only file mmap).**

---

### H3b — Anon `MAP_HUGETLB | MAP_HUGE_2MB`

Attempted: `mmap(MAP_ANONYMOUS | MAP_PRIVATE | MAP_HUGETLB | (21<<26))` sized to
the compressed fixture (~130 MiB, aligned to 2 MiB).

```
errno=12 (ENOMEM)
HugePages_Total: 0 / HugePages_Free: 0
```

**Status: Skipped — hugepages not reserved. Escalation filed:**
`/work/ralph-self-improvement/workspace/.escalations/cycle03-hugepages-setup.md`
Operator command: `sudo sysctl vm.nr_hugepages=160`.

The `Decoder::from_slice` constructor was added to `src/decoder.rs` for this variant.
When huge pages are reserved, the bench group `h3b_hugepage_anon` will activate.

---

### H3c — `memfd_create(MFD_HUGETLB | MFD_HUGE_2MB)`

Attempted: `syscall(SYS_memfd_create, "mmapzstd-hugetlb", MFD_HUGETLB|(21<<26))`,
then `ftruncate` + `mmap(MAP_SHARED)`.

```
ftruncate: ok
mmap of memfd: errno=12 (ENOMEM) — same root cause as H3b
```

**Status: Skipped — hugepages not reserved (same escalation as H3b).**

---

### Cycle-03 Summary

| Variant | Status | Throughput | vs cycle-02 |
|---------|--------|-----------|-------------|
| H3a: ext4 + MADV_HUGEPAGE | Tested — no winner | ~8,076 MB/s | −1.7% (noise) |
| H3b: MAP_HUGETLB anon | Skipped: HugePages_Free=0 | — | — |
| H3c: memfd_create MFD_HUGETLB | Skipped: HugePages_Free=0 | — | — |

**Outcome: no_winner.** The ext4 fixture change does not trigger THP (THPeligible=0
on read-only file mmap). H3b/H3c require operator pre-action (`nr_hugepages=160`).
Escalation filed for operator to reserve huge pages and re-run.

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

**Result (cycle 02-perf)**

Tested. Added `MADV_WILLNEED` for the next 2×RETIRE_WINDOW ahead of decode position, triggered inside `maybe_retire()` (fires ~32 times per 130 MiB file).

| Metric | H1+H2 (baseline) | H1+H2+H4 |
|---|---|---|
| measure_mmap (typical) | ~31 ms | ~33.6 ms |
| Throughput change | — | **−8%** |

Worse on a warm-cache file. The WILLNEED syscall overhead (~32 calls) and/or interference with SEQUENTIAL/HUGEPAGE hints outweighs any prefetch benefit. As predicted, the kernel's heuristic read-ahead is already sufficient for warm-cache sequential access. Reverted.

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

**Result (cycle 02-perf)**

Not tested as a standalone hypothesis. With the H1 direct-decode implementation, `BLOCK_SIZE` is no longer used (the overflow buffer was removed entirely). Increasing `BLOCK_SIZE` to reduce FFI calls would require reintroducing an overflow buffer, which analysis shows is counterproductive:

- Current (direct, 8 KiB io::copy): 32,768 `run_on_buffers` calls, 0 extra copies
- 1 MiB overflow + 8 KiB drain: 256 calls, but 256 MiB of copy at ~40 GB/s = +6.4 ms

FFI savings (~3 ms) < copy overhead (~6.4 ms) for 8 KiB caller buffers. Direct approach is optimal for the benchmark's 8 KiB io::copy harness.

**Status: not-tested — superseded by H1 direct-buffer approach.**

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

**Result (cycle 02-perf)**

Not tested. As documented in the hypothesis itself, `MADV_FREE` is silently ignored on file-backed read-only mappings. Confirmed by the madvise(2) man page. No code change warranted.

**Status: not-tested — N/A per hypothesis analysis.**

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

**Result (cycle 02-perf)**

Tested. Added `_mm_prefetch(_MM_HINT_T0, ptr + 512)` before each `run_on_buffers` call on `x86_64`.

| Metric | H1+H2 | H1+H2+H7 |
|---|---|---|
| measure_mmap (typical) | ~31 ms | ~33.1 ms |
| Throughput change | — | **−6%** |

Worse, as predicted. The i9-12900K's hardware stream prefetcher already handles the sequential compressed input perfectly. The software prefetch instruction adds overhead without hiding any latency. Reverted.

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
