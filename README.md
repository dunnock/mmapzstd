# mmapzstd

**mmap-backed zstd decompressor with hugepage-accelerated sequential reads**

## Why

The naive intuition—that memory-mapping a compressed file should be as fast as or faster than `BufReader`—turns out to be wrong for sequential zstd decompression on a warm page cache. A 256 MiB test corpus decompressed **24% slower** through a plain `mmap` (6,597 MB/s) than through `zstd::stream::Decoder::new(BufReader::with_capacity(65_536, File::open(p)?))` (8,634 MB/s). The culprit is TLB pressure: a 128 MiB compressed file maps to ~32,768 four-kilobyte PTEs, saturating the 1,536-entry L2 dTLB of an Intel Core i9-12900K and requiring ~31,000 hardware page-table walks per full decode pass.

The fix is to place the compressed input on 2 MiB huge pages before decoding. `MAP_HUGETLB` and `memfd_create(MFD_HUGETLB)` collapse 32,768 PTEs into 64 PMD entries, eliminating essentially all TLB misses for the input scan. A one-time copy of the compressed file into the hugepage region pays for itself immediately: both variants run at **~9,930 MB/s (+15% over the BufReader baseline)** on the same hardware (i9-12900K, Linux 6.17, warm cache). Note that `MADV_HUGEPAGE` on a read-only file-backed `MAP_PRIVATE` VMA is a no-op in Linux 6.17 (`THPeligible: 0`); the copy-into-hugepage approach is required to get the TLB reduction.

## Benchmark results

### Synthetic 256 MiB corpus (zstd level 3, ~128 MiB compressed, warm cache, Criterion medians)

| Mode | Constructor | Throughput (MB/s) | dRSS | Notes |
|------|-------------|------------------:|-----:|-------|
| naive BufReader | `zstd+BufReader<64 KiB>` | **8,634** | 5 MB | portable baseline |
| naive file mmap | `(removed — see CHANGELOG)` (cycle 01) | 6,597 | 9 MB | −24%; overflow-copy overhead + TLB pressure |
| file mmap H1+H2 | `(removed — see CHANGELOG)` (cycle 02) | 8,215 | 9 MB | −5%; eliminated overflow copy, MAP_POPULATE |
| file mmap + MADV_HUGEPAGE | `(removed — see CHANGELOG)` (cycle 03) | 8,076 | 9 MB | no change; THPeligible=0 for file VMA |
| hugepage-anon (H3b) | `Decoder::open_hugepage` | **9,910** | ~133 MB | **+15%**; MAP_HUGETLB anon copy-in |
| hugepage-memfd (H3c) | `Decoder::open_hugepage_memfd` | **9,944** | ~133 MB | **+15%**; memfd direct read, fewer faults |
| hugepage-streaming-8m (S3) | `Decoder::open_hugepage_streaming(8 MiB)` | 7,791 | ~8 MB | −13% vs BufReader; bounded hugepage RSS; supports arbitrarily large files |

### Synthetic 1 GiB compressed corpus (zstd level 3, ~1,028 MiB compressed → 2,048 MiB, warm cache)

Only modes that can handle files exceeding the 320 MiB hugepage pool are shown.
`hugepage-anon` and `hugepage-memfd` require 514 pages (1 GiB) — pool exhausted.

| Mode | Constructor | Throughput (MB/s) | dRSS | Notes |
|------|-------------|------------------:|-----:|-------|
| naive BufReader | `zstd+BufReader<64 KiB>` | **12,128** | ~0 | reuses 64 KiB buffer; hot cache |
| file mmap | `Decoder::from_mmap` | 12,843 | ~4 MB | MADV_SEQUENTIAL; zero-copy at scale |
| **hugepage-streaming-8m** | `Decoder::open_hugepage_streaming(8 MiB)` | **10,761** | **~8 MB** | **only hugepage path for large files; bounded RSS** |

### Real Binance BTCUSD file (214 MiB compressed → 1.8 GiB, warm cache, `mmapzstd-bench`)

| Mode | Constructor | Throughput (MB/s) | dRSS | Notes |
|------|-------------|------------------:|-----:|-------|
| hugepage-anon | `Decoder::open_hugepage` | 2,386 | ~220 MB | penalised by Vec→hugepage double-copy |
| hugepage-memfd | `Decoder::open_hugepage_memfd` | **2,668** | ~220 MB | direct read into hugepage region |
| bufreader | `zstd+BufReader<64 KiB>` | 2,649 | ~4 MB | within noise of hugepage-memfd |

On the highly-compressible (8.5:1) BTCUSD data, decode CPU dominates over TLB effects; hugepage-memfd and BufReader are statistically tied.

## Option overview

**Naive BufReader** — the portable baseline.  
Reads compressed input in 64 KiB chunks; the kernel's sequential read-ahead keeps the pipe full. The 64 KiB size is L2-cache-optimal on the i9-12900K: it fits alongside the zstd streaming state inside the 1.25 MiB P-core L2. Larger buffers (256 KiB – 4 MiB) evict zstd's hot working set and lose 6–10%.

**Portable file mmap** (`Decoder::from_mmap`) — low RSS, all platforms.  
Build a `memmap2::Mmap` yourself and pass it to `Decoder::from_mmap`. Applies `MADV_SEQUENTIAL + MADV_HUGEPAGE`. A 4 MiB sliding `MADV_DONTNEED` window retires pages behind the decode cursor, keeping RSS at ~9 MB regardless of file size. Best for memory-pressure workloads or files that exceed available RAM. On warm cache, ~5% slower than BufReader.

**Hugepage anon copy-in (H3b)** (`Decoder::open_hugepage`) — maximum throughput, requires hugepages.  
Reads the compressed file into a `Vec`, then copies it into a `MAP_HUGETLB | MAP_HUGE_2MB` anonymous buffer. Decodes from the hugepage region. Returns `io::ErrorKind::OutOfMemory` if `mmap(MAP_HUGETLB)` fails — no silent fallback. The copy-in incurs ~55k minor faults per open (one per 4 KiB write into the fresh anonymous mapping).

**Hugepage memfd copy-in (H3c)** (`Decoder::open_hugepage_memfd`) — maximum throughput, fewer faults.  
Allocates a `memfd_create(MFD_HUGETLB | MFD_HUGE_2MB)` file descriptor, maps it, and reads the compressed file directly into the mapped region—no intermediate `Vec`. Fault count drops to ~107 per open (vs ~55k for H3b). Marginally faster than H3b; preferred when hugepages are available. Returns `io::ErrorKind::OutOfMemory` if hugepage allocation fails.

**Hugepage streaming scratch (S3)** (`Decoder::open_hugepage_streaming`) — bounded hugepage RSS, arbitrarily large files.  
Allocates a fixed-size `MAP_HUGETLB` scratch (default 8 MiB = 4 hugepages) and sequentially refills it from the source mmap as the decoder consumes input. The source mmap remains 4 KiB-backed; only the scratch uses huge pages. Hugepage RSS is bounded to the scratch size regardless of corpus size—making this the only hugepage path that handles files larger than the hugepage pool. The refill memcpy from 4 KiB source pages to the hugepage scratch costs ~23% of total decode time (8 ms on 256 MiB, 22 ms on 1 GiB), leaving throughput 30% below `open_hugepage_memfd` and 13% below BufReader on the 256 MiB benchmark. Use `open_hugepage_memfd` for small files; use this constructor when the compressed file exceeds `vm.nr_hugepages × 2 MiB`. Returns `io::ErrorKind::OutOfMemory` if hugepage scratch allocation fails.

## Library usage

```toml
# Cargo.toml
[dependencies]
mmapzstd = { git = "https://github.com/dunnock/mmapzstd" }
memmap2 = "0.9"   # for from_mmap on non-Linux
```

```rust
use std::io::{self, Read};
use mmapzstd::decoder::Decoder;

fn decompress(path: &std::path::Path) -> io::Result<Vec<u8>> {
    // Maximum throughput on Linux with vm.nr_hugepages reserved.
    // Returns io::ErrorKind::OutOfMemory if hugepages are unavailable —
    // reserve with `sudo sysctl vm.nr_hugepages=N` before calling.
    #[cfg(target_os = "linux")]
    let mut dec = Decoder::open_hugepage_memfd(path)?;

    // Non-Linux: build a Mmap yourself and pass it through from_mmap.
    // Applies MADV_SEQUENTIAL; keeps RSS at ~9 MB via DONTNEED retirement.
    #[cfg(not(target_os = "linux"))]
    let mut dec = {
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };
        Decoder::from_mmap(mmap)?
    };

    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}
```

Constructor guide:
- **`open_hugepage_memfd(path)`** — Linux ≥ 4.14, file fits in hugepage pool: maximum throughput (~10 GB/s), direct read, ~107 faults/open.
- **`open_hugepage(path)`** — Linux ≥ 2.6.17, file fits in hugepage pool: same throughput, ~55k faults/open (Vec copy-in).
- **`open_hugepage_streaming(path, window)`** — Linux ≥ 2.6.17, file exceeds hugepage pool: bounded `window`-byte hugepage RSS; use 8 MiB default (`DEFAULT_STREAMING_WINDOW`). Only 4 pages reserved required regardless of file size.
- **`from_mmap(mmap)`** — all platforms: caller provides a pre-built `memmap2::Mmap`; portable low-RSS path.
- **`from_slice(data)`** — caller manages backing memory (custom allocators, test stubs).

All constructors return `io::Result<Decoder>` and implement `std::io::Read`.

## CLI usage

```sh
# Build
cargo build --release

# Benchmark against a real .zst file (3 runs, 1 warmup discarded, null sink)
./target/release/mmapzstd-bench /path/to/file.zst --mode hugepage-memfd --runs 3
./target/release/mmapzstd-bench /path/to/file.zst --mode hugepage-anon  --runs 3
./target/release/mmapzstd-bench /path/to/file.zst --mode bufreader      --runs 3
```

Sample output (BTCUSD 214 MiB compressed, warm cache):

```
mode: hugepage-memfd
| run | wall   | decompressed | throughput (MB/s) | dRSS (KiB) | minor faults |
|-----|--------|--------------|-------------------|------------|--------------|
| 1   | 694 ms | 1.8 GiB      | 2,674             | 4116       | 1136         |
| 2   | 695 ms | 1.8 GiB      | 2,668             | 12         | 110          |
| 3   | 696 ms | 1.8 GiB      | 2,666             | 0          | 107          |
median: 695 ms / 2,668 MB/s
```

## System requirements

| Requirement | Details |
|-------------|---------|
| Linux ≥ 2.6.17 | `MAP_HUGETLB` (`open_hugepage`, `open_hugepage_streaming`) |
| Linux ≥ 4.14 | `memfd_create(MFD_HUGETLB)` (`open_hugepage_memfd`) |
| `vm.nr_hugepages ≥ ⌈compressed_size_MiB / 2⌉` | `open_hugepage` / `open_hugepage_memfd` — full file in hugepages |
| `vm.nr_hugepages ≥ ⌈window_MiB / 2⌉` (≥ 4 for 8 MiB default) | `open_hugepage_streaming` — scratch only; independent of file size |
| Rust ≥ 1.75 | MSRV |

### Reserving huge pages

```sh
# Persistent (survives reboot)
echo 'vm.nr_hugepages = 160' | sudo tee /etc/sysctl.d/40-hugepages.conf
sudo sysctl --system

# One-shot (lost on reboot)
sudo sysctl vm.nr_hugepages=160

# Verify
grep HugePages /proc/meminfo
# HugePages_Total:     160
# HugePages_Free:      160
# Hugepagesize:        2048 kB
```

160 pages covers up to 320 MiB of compressed input for `open_hugepage` / `open_hugepage_memfd`.
`open_hugepage_streaming` needs only 4 pages (8 MiB) regardless of file size.
If hugepages are not reserved, all three constructors return `io::ErrorKind::OutOfMemory`
with a message explaining how to reserve hugepages. There is no silent fallback.

## Read the paper

See [RESEARCH.md](RESEARCH.md) for the full write-up (also available as [RESEARCH.pdf](RESEARCH.pdf)). The PDF is regenerated from RESEARCH.md via:

```sh
# The lmodern TeX package must be removed from the pandoc default template on
# Debian-based TeX Live 2022 installs where it is missing from texlive-fonts-recommended.
# Generate a patched template first:
pandoc --print-default-template=latex | \
  sed 's/\\usepackage{lmodern}//' > pandoc-nolm.tex

pandoc RESEARCH.md -o RESEARCH.pdf \
  --pdf-engine=xelatex \
  --template=pandoc-nolm.tex \
  -V geometry:margin=1in \
  -V mainfont="DejaVu Serif" \
  -V monofont="DejaVu Sans Mono" \
  -V fontsize=12pt \
  -V documentclass=article \
  -V title="Closing the TLB Gap" \
  --toc
```

## Build and test

```sh
cargo build --release
cargo test
CARGO_TARGET_DIR=/work/cargo-target-ralph cargo bench
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
