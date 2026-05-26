# Changelog

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
