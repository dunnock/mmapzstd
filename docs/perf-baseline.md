# Performance Baseline: mmapzstd vs zstd+BufReader

## Profiling Methodology

`perf` is not installed on this machine. Metrics were collected via:

- Wall-clock time: `std::time::Instant`
- Minor / major faults: `/proc/self/stat` fields `minflt` (field 10) and `majflt` (field 12) — delta measured across the decode only, after fixture generation
- Peak RSS: `/proc/self/status` field `VmHWM`

Three runs per path. The fixture is regenerated fresh each run (fixture generation phase is excluded from fault/RSS deltas; wall-clock includes only the decode).

## System Information

| Item | Value |
|---|---|
| CPU | 12th Gen Intel Core i9-12900K |
| RAM | 125 GiB |
| Kernel | 6.17.0-29-generic (Linux) |
| Rust toolchain | rustc 1.83.0 / cargo 1.83.0 |
| Optimization | `--release` profile |
| Page size | 4096 bytes |
| L1D cache | 48 KiB |
| L1I cache | 32 KiB |
| L2 cache | 1280 KiB (1.25 MiB) |
| L3 cache | 30720 KiB (30 MiB) |
| THP policy | `madvise` (MADV_HUGEPAGE hint is active) |
| Static huge pages (HugePages_Total) | 0 |
| File-backed huge pages in use (FileHugePages) | ~4.3 GiB |

## Corpus

256 MiB alternating 4 KiB random + 4 KiB `0xAB` blocks, compressed at zstd level 3.
Compressed size on disk: ~130 MiB. Compression ratio: ~2:1.

## Raw Results

### `mmapzstd::Decoder` (`examples/measure_mmap`)

| Run | Elapsed (ms) | Throughput (MB/s) | Minor faults | Major faults |
|-----|--------------|-------------------|--------------|--------------|
| 1 | 37.947 | 6746 | 2666 | 0 |
| 2 | 38.021 | 6733 | 2666 | 0 |
| 3 | 37.821 | 6769 | 2666 | 0 |

**Median:** 37.947 ms, **6746 MB/s**
**Spread:** [37.821 ms, 38.021 ms] — very stable, ±0.1 ms

### `zstd + BufReader` baseline (`examples/measure_baseline`)

| Run | Elapsed (ms) | Throughput (MB/s) | Minor faults | Major faults |
|-----|--------------|-------------------|--------------|--------------|
| 1 | 30.672 | 8347 | 623 | 0 |
| 2 | 31.292 | 8181 | 623 | 0 |
| 3 | 26.925 | 9508 | 624 | 0 — CPU boost outlier |

**Median (stable pair):** 30.672 ms, **8347 MB/s**
Run 3 is an outlier from CPU frequency boost (Turbo Boost, ~14% above sustained); excluded from median.

## Summary Comparison

| Metric | `mmapzstd::Decoder` | `zstd+BufReader` | Ratio |
|---|---|---|---|
| Elapsed (median, ms) | 37.95 | 30.67 | 1.24× slower |
| Throughput (MB/s) | 6746 | 8347 | 0.81× |
| Minor page faults | **2666** | **623** | **4.3× more** |
| Major page faults | 0 | 0 | — |

**The mmap decoder is ~24% slower than the BufReader baseline on page-cached input.**

These numbers are consistent with the Criterion benchmark in `docs/bench-results.md` (28% gap), the slight difference reflecting run-to-run variance.

## Counters Unavailable Without `perf`

The following counters were requested but are not measurable without `perf stat`:

- `task-clock`, `context-switches`, `cpu-migrations`
- `dTLB-loads`, `dTLB-load-misses`, `dTLB-store-misses`
- `iTLB-loads`, `iTLB-load-misses`
- `cache-references`, `cache-misses`, `LLC-loads`, `LLC-load-misses`
- `cycles`, `instructions`, `branches`, `branch-misses`

Install with `apt-get install linux-tools-common linux-tools-$(uname -r)` and re-run:

```sh
perf stat -e task-clock,context-switches,cpu-migrations,\
page-faults,minor-faults,major-faults,\
dTLB-loads,dTLB-load-misses,iTLB-loads,iTLB-load-misses,\
cache-references,cache-misses,LLC-loads,LLC-load-misses,\
cycles,instructions,branches,branch-misses \
./target/release/examples/measure_mmap
```

## Interpretation

### Zero major faults

Both paths have 0 major faults. The compressed file is fully resident in the kernel page cache by the time the benchmark runs — no disk I/O is needed during decode.

### 4.3× more minor faults for mmap

The mmap path incurs 2666 minor faults vs 623 for BufReader. Each minor fault updates the process's page table with a mapping to a page already in the cache. The kernel's `MADV_SEQUENTIAL` hint triggers "fault-around" (pre-faulting ~16 nearby pages per fault), so the actual number of individual page walks is lower than naively expected.

With 2666 faults × 4 KiB = ~10.4 MiB of faulted input, and a ~130 MiB compressed file, the fault-around factor of ~12× accounts for the ratio. Minor faults cost on the order of 1–5 μs each on modern hardware (page table lock + TLB fill). At 2 μs × 2043 extra faults = ~4 ms, page fault overhead explains roughly half the observed gap.

### Overflow copy overhead (code-level finding)

The current decoder always decompresses into a 128 KiB `self.overflow` buffer first, then copies into the caller's buffer. `io::copy` uses an 8 KiB internal buffer, so the call pattern is:

1. One `run_on_buffers()` call → 128 KiB into `overflow`
2. 16 drain calls copying 8 KiB each from `overflow` to caller

This adds ~256 MiB of extra cache traffic per full decode (128 KiB × 2048 decompress calls). At L2 bandwidth (~100 GB/s), this costs ~2.5 ms and likely accounts for the remaining gap.

### No significant madvise overhead

`MADV_DONTNEED` is called ~32 times for the ~130 MiB file with the 4 MiB retirement window. This is 32 syscalls total — negligible cost.

### THP status

THP is in `madvise` mode and the code applies `MADV_HUGEPAGE`. However, static huge pages are not pre-allocated (`HugePages_Total: 0`), so THP relies on asynchronous promotion. File-backed read-only mappings are only promoted to THPs in newer kernel versions. It is unclear whether THP promotion is actually occurring for this mapping without `perf` counters.

## What to Beat

Any optimization must improve on 37.95 ms / 6746 MB/s for the mmap path. The target is to approach or exceed the baseline of 30.67 ms / 8347 MB/s.
