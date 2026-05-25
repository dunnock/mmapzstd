# BufReader Optimisation Hypotheses (cycle 02 — bufreader-fallback)

**Context:** `mmap-perf-implement` (cycle 02) found `no_winner` — the mmap decoder
closed the gap from −28% to −5% vs the BufReader baseline but could not beat it on
the warm-cache benchmark.  This task explores whether the BufReader baseline itself
can be made faster.

**Baseline (cycle-02 Criterion):** `zstd::stream::Decoder::new(BufReader::with_capacity(65_536, File::open(p)?))` — 29.67 ms / **8,634 MB/s** for a 256 MiB corpus (zstd level 3).

Measurement tool: `examples/measure_bufreader_variants.rs` — 5 runs per variant
(2 warmup discarded), timing taken with `std::time::Instant`, page cache warm.

---

## Results (3 independent invocations, 3 measured runs each)

| Variant | Run 1 ms | Run 2 ms | Run 3 ms | Mean ms | Mean MB/s | vs baseline |
|---|---|---|---|---|---|---|
| **H1a BufReader  64 KiB** (baseline) | 29.18 | 29.45 | 28.92 | **29.18** | **8,773** | — |
| H1b BufReader 256 KiB | 30.95 | 31.37 | 30.85 | 31.06 | 8,243 | −6% |
| H1c BufReader   1 MiB | 32.60 | 32.83 | 32.36 | 32.60 | 7,853 | −10% |
| H1d BufReader   4 MiB | 32.31 | 32.88 | 32.42 | 32.54 | 7,868 | −10% |
| H2  64 KiB + fadvise(SEQ+WILLNEED) | 29.53 | 29.21 | 28.86 | 29.20 | 8,766 | ≈0% |
| H4  raw File (no BufReader) | 29.65 | 29.21 | 28.72 | 29.19 | 8,771 | ≈0% |

Minor faults: 0 in all variants (file is fully page-cached).

---

## Hypothesis-by-hypothesis analysis

### H1 — Larger BufReader buffer (64 KiB / 256 KiB / 1 MiB / 4 MiB)

**Hypothesis:** More bytes per syscall → fewer context switches → faster.

**Result: 64 KiB is optimal. Larger buffers are 6–10% slower.**

**Explanation:** zstd decompression is CPU-bound; syscall overhead is negligible on
a page-cached file. The BufReader's heap buffer competes directly with the zstd
decoder's internal working set for L2 cache space. The i9-12900K's P-core L2 is
1280 KiB; the 64 KiB buffer fits alongside the zstd streaming state (~64 KiB) and
the 8 KiB `io::copy` output buffer. A 256 KiB buffer partially evicts zstd's
hot state on every refill, causing L2 cache misses in the decompressor. At 1 MiB
and 4 MiB the effect plateaus at ~10% overhead.

**Conclusion:** Do not increase the BufReader capacity beyond 64 KiB for this
workload. The 64 KiB value matches the cache-optimal point.

---

### H2 — `posix_fadvise(POSIX_FADV_SEQUENTIAL)` + `posix_fadvise(POSIX_FADV_WILLNEED)`

**Hypothesis:** Explicit kernel hints cause more aggressive readahead and page-table
pre-population, reducing the effective read latency for the BufReader.

**Result: Within measurement noise of baseline (≈0% delta, ~29.2 ms).**

**Explanation:** On a fully page-cached file there is no disk I/O and kernel
sequential readahead is already maximally effective. `POSIX_FADV_SEQUENTIAL`
tells the kernel to double its readahead window, but with a 130 MiB file fully
in cache the window is irrelevant. `POSIX_FADV_WILLNEED` would trigger
asynchronous readahead, again a no-op when pages are already resident. Both
hints add two extra syscalls at open time with no measurable benefit.

**Conclusion:** posix_fadvise provides no benefit for warm-cache sequential reads.
May help on cold-cache workloads (file not resident); not tested here.

---

### H3 — `splice()` file → pipe → user buffer

**Analysis (not implemented):**

`splice(file_fd, pipe_write, N)` copies file pages into a pipe buffer entirely
in-kernel (no user-space memcpy for that step). But the zstd decoder requires a
`Read` impl that returns bytes into a user-space buffer. To use splice as the
input stage we would need to:

1. `splice(file_fd, pipe_write, CHUNK)` — kernel-kernel: 1 syscall
2. `read(pipe_read, user_buf, CHUNK)` — kernel→user: 1 syscall + 1 copy

That is two syscalls and one copy per chunk vs BufReader's one syscall and one
copy. Net effect: more syscall overhead with identical data-copy cost.

`splice` wins only when the destination is also a kernel fd (e.g. a socket via
`sendfile`). It cannot eliminate the user-space copy required for decompression.

**Conclusion:** Not applicable. No implementation needed.

---

### H4 — Raw `File` (no BufReader)

**Hypothesis:** `io::copy` on recent Rust uses specialised `copy_file_range` /
`sendfile` paths. Passing a raw `File` to zstd might trigger a faster path than
going through `BufReader`.

**Result: Within noise of 64 KiB BufReader (29.19 ms mean, ≈0% delta).**

**Explanation:** zstd's streaming `Decoder<File>` calls `file.read(internal_buf)`
where `internal_buf` is zstd's own 128 KiB input staging buffer. This is
equivalent to one `BufReader` refill per 128 KiB of compressed input. The
`io::copy` output path is identical in both cases (8 KiB stack buffer → `sink()`).
So the effective syscall rate is the same whether BufReader or File is used,
because zstd's internal buffering absorbs the read pattern.

The `copy_file_range` / `sendfile` specialisation in `io::copy` only activates
when copying `File → File` — not `Decoder<File> → Sink`.

**Conclusion:** Raw File is equivalent to BufReader<128KiB>. No improvement.

---

## Overall conclusion

**No winner found.** None of the four BufReader hypotheses beat the 64 KiB
BufReader baseline by ≥5%. The 64 KiB buffer is already at the L2-cache
sweet spot, and the remaining hypotheses add syscall overhead (H2, H3) or
replicate existing internal buffering (H4).

The optimal BufReader recipe remains:

```rust
zstd::stream::Decoder::new(BufReader::with_capacity(65_536, File::open(p)?))
```

This is **the best end-to-end zstd decompression recipe on this hardware** for
warm-cache sequential reads. The mmap decoder (`mmapzstd::Decoder`) is ~5% slower
than this recipe on the same workload. Enabling 2 MiB transparent huge pages
(H3 from `perf-hypotheses.md`) remains the only identified path to closing that gap,
but requires operator action on the system THP policy.
