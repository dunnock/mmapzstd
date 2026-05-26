#![deny(missing_docs)]
//! mmap-backed zstd decompressor.
//!
//! Decompresses `.zst` files by memory-mapping the compressed input, then
//! streaming decompressed bytes to a caller-provided sink.  The caller drives
//! the read; the library does not buffer ahead of demand beyond the zstd
//! frame buffer (~128 KiB per block).
//!
//! # Choosing a constructor
//!
//! | Constructor | Platform | When to use |
//! |-------------|----------|-------------|
//! | [`decoder::Decoder::open`] | all | default; lowest RSS (~9 MB) |
//! | [`decoder::Decoder::open_hugepage_memfd`] | Linux ≥ 4.14 | max throughput, hugepages available |
//! | [`decoder::Decoder::open_hugepage`] | Linux ≥ 2.6.17 | max throughput, hugepages available |
//! | [`decoder::Decoder::from_mmap`] | all | caller controls mmap options |
//! | [`decoder::Decoder::from_slice`] | all | caller controls backing memory |
//!
//! Both hugepage constructors fall back to [`decoder::Decoder::open`] transparently
//! if `MAP_HUGETLB` is unavailable (no hugepages reserved or non-Linux platform).
//!
//! # Quick start
//!
//! ```no_run
//! use std::io::Read;
//! use mmapzstd::decoder::Decoder;
//!
//! let mut dec = Decoder::open(std::path::Path::new("data.zst"))?;
//! let mut out = Vec::new();
//! dec.read_to_end(&mut out)?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # System requirements for hugepage variants
//!
//! ```sh
//! # Reserve at least ⌈compressed_size_MiB / 2⌉ huge pages
//! sudo sysctl vm.nr_hugepages=160
//! grep HugePages_Total /proc/meminfo
//! ```

/// Streaming zstd [`Decoder`][decoder::Decoder] backed by mmap or hugepage memory.
pub mod decoder;
