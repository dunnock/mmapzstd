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
| naive file mmap | `Decoder::open` (cycle 01) | 6,597 | 9 MB | −24%; overflow-copy overhead + TLB pressure |
| file mmap H1+H2 | `Decoder::open` (cycle 02) | 8,215 | 9 MB | −5%; eliminated overflow copy, MAP_POPULATE |
| file mmap + MADV_HUGEPAGE | `Decoder::open` (cycle 03) | 8,076 | 9 MB | no change; THPeligible=0 for file VMA |
| hugepage-anon (H3b) | `Decoder::open_hugepage` | **9,910** | ~133 MB | **+15%**; MAP_HUGETLB anon copy-in |
| hugepage-memfd (H3c) | `Decoder::open_hugepage_memfd` | **9,944** | ~133 MB | **+15%**; memfd direct read, fewer faults |

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

**Naive file mmap** (`Decoder::open`) — low RSS, all platforms.  
Memory-maps the compressed file read-only with `MAP_POPULATE` (batch pre-fault at open time) and `MADV_SEQUENTIAL + MADV_HUGEPAGE`. A 4 MiB sliding `MADV_DONTNEED` window retires pages behind the decode cursor, keeping RSS at ~9 MB regardless of file size. Best for memory-pressure workloads or files that exceed available RAM. On warm cache, ~5% slower than BufReader after two cycles of optimisation.

**File mmap + MAP_POPULATE/MADV_HUGEPAGE** (cycle 02 winner before hugepage reservation) — same API as `Decoder::open`.  
`MAP_POPULATE` moves 2,043 minor faults from the decode loop to open time. `MADV_HUGEPAGE` on a read-only file-backed `MAP_PRIVATE` VMA is silently ignored in Linux 6.17 (`THPeligible: 0`). TLB pressure during the decode scan remains unchanged.

**Hugepage anon copy-in (H3b)** (`Decoder::open_hugepage`) — maximum throughput, requires hugepages.  
Reads the compressed file into a `Vec`, then copies it into a `MAP_HUGETLB | MAP_HUGE_2MB` anonymous buffer. Decodes from the hugepage region. Falls back transparently to `Decoder::open` if `mmap(MAP_HUGETLB)` returns `ENOMEM`. The copy-in incurs ~55k minor faults per open (one per 4 KiB write into the fresh anonymous mapping).

**Hugepage memfd copy-in (H3c)** (`Decoder::open_hugepage_memfd`) — maximum throughput, fewer faults.  
Allocates a `memfd_create(MFD_HUGETLB | MFD_HUGE_2MB)` file descriptor, maps it, and reads the compressed file directly into the mapped region—no intermediate `Vec`. Fault count drops to ~107 per open (vs ~55k for H3b). Marginally faster than H3b; preferred when hugepages are available.

## Library usage

```toml
# Cargo.toml
[dependencies]
mmapzstd = { git = "https://github.com/dunnock/mmapzstd" }
```

```rust
use std::io::{self, Read};
use mmapzstd::decoder::Decoder;

fn decompress(path: &std::path::Path) -> io::Result<Vec<u8>> {
    // Maximum throughput on Linux with vm.nr_hugepages reserved.
    // Falls back to Decoder::open automatically if hugepages are unavailable.
    #[cfg(target_os = "linux")]
    let mut dec = Decoder::open_hugepage_memfd(path)?;

    // Portable: mmap with MAP_POPULATE + MADV_SEQUENTIAL. Low RSS.
    #[cfg(not(target_os = "linux"))]
    let mut dec = Decoder::open(path)?;

    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}
```

Constructor guide:
- **`open_hugepage_memfd(path)`** — Linux, hugepages reserved: maximum throughput, direct read, ~107 faults/open.
- **`open_hugepage(path)`** — Linux, hugepages reserved: same throughput, ~55k faults/open (Vec copy-in).
- **`open(path)`** — all platforms: lowest RSS (~9 MB); auto-fallback target for both hugepage variants.
- **`from_mmap(mmap)`** — caller provides a pre-built `memmap2::Mmap`.
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
| Linux ≥ 2.6.17 | `MAP_HUGETLB` (`open_hugepage`) |
| Linux ≥ 4.14 | `memfd_create(MFD_HUGETLB)` (`open_hugepage_memfd`) |
| `vm.nr_hugepages ≥ ⌈compressed_size_MiB / 2⌉` | Both hugepage variants need pre-reserved 2 MiB pages |
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

160 pages covers up to 320 MiB of compressed input. If hugepages are not reserved, both `open_hugepage` and `open_hugepage_memfd` fall back to `Decoder::open` transparently—no error, no panic, no API breakage.

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
