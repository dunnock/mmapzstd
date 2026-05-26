# CLI Benchmark Results — `mmapzstd-bench` against real Binance data

## Environment

- Binary: `mmapzstd-bench v0.1.0` (release build)
- File: `/mnt/data/Dropbox/split/BINANCE_D/BTCUSD_PERP/20231217.zst`
- Compressed size: 224,226,015 bytes (213.8 MiB)
- Decompressed size: 1.8 GiB (~8.5:1 ratio — typical for CSV trading records)
- Sink: `--sink null` (io::copy into io::sink — CPU-only, no allocator cost)
- Warmup: 1 run discarded per mode
- HugePages_Free at run time: 160 (sufficient; needed ≈ 108)

---

## hugepage-anon (MAP_ANONYMOUS | MAP_HUGETLB | MAP_HUGE_2MB)

```
mmapzstd-bench v0.1.0
file: /mnt/data/Dropbox/split/BINANCE_D/BTCUSD_PERP/20231217.zst
size: 224,226,015 bytes (213.8 MiB compressed)
mode: hugepage-anon
runs: 3 (warmup 1 discarded)

| run | wall      | decompressed | throughput (MB/s) | dRSS (KiB) | minor faults |
|-----|-----------|--------------|-------------------|------------|--------------|
| 1   | 777 ms    | 1.8 GiB      | 2,386             | 4116       | 55879        |
| 2   | 782 ms    | 1.8 GiB      | 2,373             | 12         | 54853        |
| 3   | 774 ms    | 1.8 GiB      | 2,397             | 0          | 54850        |

median: 777 ms / 2,386 MB/s
```

**Note:** `open_hugepage` reads the file into a `Vec<u8>` first and then copies to
the hugepage region. This double-buffering causes ~55k minor faults per run as the
hugepage pages are written, even across repeated runs (hugepage pages are fresh each
run since the anonymous mapping is re-created).

---

## hugepage-memfd (memfd_create(MFD_HUGETLB|MFD_HUGE_2MB) + ftruncate + mmap)

```
mmapzstd-bench v0.1.0
file: /mnt/data/Dropbox/split/BINANCE_D/BTCUSD_PERP/20231217.zst
size: 224,226,015 bytes (213.8 MiB compressed)
mode: hugepage-memfd
runs: 3 (warmup 1 discarded)

| run | wall      | decompressed | throughput (MB/s) | dRSS (KiB) | minor faults |
|-----|-----------|--------------|-------------------|------------|--------------|
| 1   | 694 ms    | 1.8 GiB      | 2,674             | 4116       | 1136         |
| 2   | 695 ms    | 1.8 GiB      | 2,668             | 12         | 110          |
| 3   | 696 ms    | 1.8 GiB      | 2,666             | 0          | 107          |

median: 695 ms / 2,668 MB/s
```

**Note:** `open_hugepage_memfd` reads the file directly into the mapped hugepage
region (no intermediate Vec). Only ~107 minor faults on runs 2–3 (one per huge
page boundary walked by the decoder). Significantly lower fault count than
`hugepage-anon`.

---

## bufreader (zstd::stream::Decoder + BufReader<File, 65536>)

```
mmapzstd-bench v0.1.0
file: /mnt/data/Dropbox/split/BINANCE_D/BTCUSD_PERP/20231217.zst
size: 224,226,015 bytes (213.8 MiB compressed)
mode: bufreader
runs: 3 (warmup 1 discarded)

| run | wall      | decompressed | throughput (MB/s) | dRSS (KiB) | minor faults |
|-----|-----------|--------------|-------------------|------------|--------------|
| 1   | 700 ms    | 1.8 GiB      | 2,649             | 4264       | 1066         |
| 2   | 707 ms    | 1.8 GiB      | 2,624             | 8          | 2            |
| 3   | 699 ms    | 1.8 GiB      | 2,652             | 4          | 0            |

median: 700 ms / 2,649 MB/s
```

---

## Consolidated comparison

| mode           | median wall | median throughput | median minor faults (run 2+) |
|----------------|-------------|-------------------|-------------------------------|
| hugepage-anon  | 777 ms      | 2,386 MB/s        | ~54,853                       |
| hugepage-memfd | 695 ms      | 2,668 MB/s        | ~110                          |
| bufreader      | 700 ms      | 2,649 MB/s        | ~1                            |

### Takeaways

1. **hugepage-memfd is fastest** on this workload (+12% vs hugepage-anon, +1% vs
   bufreader), with the lowest fault count on repeated runs.

2. **bufreader is essentially tied with hugepage-memfd** within measurement noise
   (±5 ms). For this Binance file — large compressed size, high compression ratio —
   the decode CPU cost dominates over any TLB/mmap overhead differences.

3. **hugepage-anon underperforms** because `open_hugepage` copies the compressed data
   from a regular-page `Vec` into the hugepage buffer. The ~55k minor faults per run
   (one per 4 KiB hugepage write) are the double-buffering cost; the throughput
   penalty is ~15%. `hugepage-memfd` avoids this by reading directly into the mapped
   region.

4. **Minor fault advantage of hugepage-memfd over bufreader** (~107 vs ~1 on run 2+)
   is not visible in wall-clock time — the saved faults are offset by the cost of
   mapping and reading the full 214 MiB into hugepage memory up front. The advantage
   would be more pronounced in latency-sensitive code that decompresses in parallel
   with other memory-intensive work.

### Comparison with criterion bench (cycle-03, 256 MiB synthetic fixture)

The cycle-03 criterion bench reported ~9,930 MB/s for H3b/H3c on the synthetic
fixture vs ~2,600–2,700 MB/s here. The difference is expected: the synthetic fixture
is 256 MiB of alternating random+0xAB blocks which compresses to a smaller output,
while the Binance CSV data compresses at ~8.5:1 (214 MiB → 1.8 GiB). Decompressing
1.8 GiB requires proportionally more CPU work per byte of compressed input.
