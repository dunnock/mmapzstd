# Changelog

## 0.3.0 — 2026-05-26

New constructor:

- `Decoder::open_hugepage_streaming(path, window)` — S3 streaming algorithm.
  Decodes arbitrarily large `.zst` files through an 8 MiB (configurable)
  `MAP_HUGETLB` scratch buffer; hugepage RSS is bounded to the scratch size
  regardless of corpus size. This is the only hugepage-backed path that works
  when the compressed file exceeds the hugepage pool (`vm.nr_hugepages`).
  Requires Linux ≥ 2.6.17 and ≥ 4 pre-reserved 2 MiB pages (8 MiB scratch).
  Returns `io::ErrorKind::OutOfMemory` if hugepage allocation fails.

**Guidance:**

- For files that fit in the hugepage pool: prefer `open_hugepage_memfd` — it
  loads the full compressed file onto huge pages and achieves ~30% higher
  throughput (10,152 MB/s vs 7,791 MB/s on the 256 MiB benchmark).
- For files larger than the hugepage pool: use `open_hugepage_streaming` — it
  is the only hugepage path available, maintaining bounded 8 MiB hugepage RSS
  while achieving 10,761 MB/s on a 2 GiB decompressed / 1 GiB compressed file.

## 0.2.0 — 2026-05-26

Breaking changes:

- Removed `Decoder::open(path)`. Use `Decoder::open_hugepage_memfd`,
  `Decoder::open_hugepage`, or build your own `memmap2::Mmap` and
  pass it through `Decoder::from_mmap`.
- Removed `MAP_POPULATE` and `MADV_POPULATE_READ` (had no measurable
  effect on the surviving hugepage paths).
- `open_hugepage*` now return `io::Error::OutOfMemory` if
  `MAP_HUGETLB` fails (no more silent fallback to file mmap).
  Reserve hugepages with `sudo sysctl vm.nr_hugepages=N` first.
