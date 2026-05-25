# Benchmark Results: mmapzstd vs zstd+BufReader

## Environment

| Item | Value |
|---|---|
| CPU | 12th Gen Intel Core i9-12900K |
| RAM | 125 GiB |
| Kernel | 6.17.0-29-generic (Linux) |
| Rust toolchain | rustc 1.83.0 / cargo 1.83.0 |
| Optimization | `--release` profile |
| Corpus | 256 MiB alternating random + 0xAB blocks, zstd level 3 |

## Benchmark Results (Criterion)

Harness: Criterion 0.3, 10 samples, 30 s measurement window, warm-up 3 s.
Throughput computed as 256 MiB ÷ median wall time.

| Implementation | Median time | 95% CI | Throughput (median) | Throughput CI |
|---|---|---|---|---|
| `mmapzstd::Decoder` | 38.692 ms | [38.578 ms – 38.904 ms] | **6,614 MB/s** | 6,581–6,633 MB/s |
| `zstd+BufReader` (baseline) | 30.194 ms | [29.912 ms – 30.655 ms] | **8,478 MB/s** | 8,350–8,558 MB/s |

The baseline is ~22 % faster than the mmap decoder on this machine.

## Page-Fault and RSS Comparison

Measured by reading `/proc/self/stat` and `/proc/self/status` before and after
one full decode of the same 256 MiB corpus. (`/usr/bin/time -v` and `perf` are
not installed on this machine; shell `time` does not report fault counts.)

| Metric | `mmapzstd::Decoder` | `zstd+BufReader` |
|---|---|---|
| Minor page faults | 2,667 | 623 |
| Major page faults | 0 | 0 |
| VmRSS after decode | 9.0 MB | 5.0 MB |

Zero major faults in both cases: the compressed file (~130 MB on disk) is fully
resident in the kernel page cache by the time the measurement runs (criterion
warms up first).

## Interpretation

The baseline `zstd+BufReader` path is faster because zstd decompression is
CPU-bound on modern hardware: the `read()` + kernel-copy overhead from a 64 KiB
`BufReader` is negligible relative to codec work, and the kernel's sequential
read-ahead keeps the pipe full. The mmap decoder pays a higher soft-fault cost
(2,667 vs 623 minor faults) because each 4 KiB compressed page must be faulted
into the TLB on first access, even with `MADV_SEQUENTIAL` hinting. The
`MADV_DONTNEED` sliding-window retirement is working correctly — the mmap
decoder's post-decode RSS is only 9 MB despite mapping a ~130 MB compressed
file, staying in the same ballpark as the BufReader path's 5 MB. On workloads
where the compressed input is larger than available RAM or where re-reading the
same file repeatedly would fill the page cache, the mmap path's tighter memory
footprint and elimination of double-buffering may tip the balance.
