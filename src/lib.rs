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
//! | [`decoder::Decoder::open_hugepage_memfd`] | Linux ≥ 4.14 | max throughput, hugepages reserved |
//! | [`decoder::Decoder::open_hugepage`] | Linux ≥ 2.6.17 | max throughput, hugepages reserved |
//! | [`decoder::Decoder::from_mmap`] | all | caller controls mmap options; portable file-mmap path |
//! | [`decoder::Decoder::from_slice`] | all | caller controls backing memory |
//!
//! Hugepage constructors require pre-reserved 2 MiB pages (`vm.nr_hugepages`).
//! If unavailable they return `io::ErrorKind::OutOfMemory` — there is no silent
//! fallback.  Reserve with `sudo sysctl vm.nr_hugepages=N` before calling them.
//!
//! Non-Linux callers or callers that want a portable low-RSS path should build
//! a [`memmap2::Mmap`] and pass it through [`decoder::Decoder::from_mmap`].
//!
//! # Quick start
//!
//! ```no_run
//! use std::io::Read;
//! use memmap2::MmapOptions;
//! use mmapzstd::decoder::Decoder;
//!
//! // Linux — maximum throughput (requires vm.nr_hugepages > 0).
//! #[cfg(target_os = "linux")]
//! let mut dec = Decoder::open_hugepage_memfd(std::path::Path::new("data.zst"))?;
//!
//! // All platforms — portable file-mmap, low RSS (~9 MB sliding window).
//! #[cfg(not(target_os = "linux"))]
//! let mut dec = {
//!     let file = std::fs::File::open("data.zst")?;
//!     let mmap = unsafe { MmapOptions::new().map(&file)? };
//!     Decoder::from_mmap(mmap)?
//! };
//!
//! let mut out = Vec::new();
//! dec.read_to_end(&mut out)?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # System requirements for hugepage variants
//!
//! Hugepage reservation is **mandatory** for `open_hugepage` and
//! `open_hugepage_memfd` — failure returns a clear `OutOfMemory` error.
//!
//! ```sh
//! # Reserve at least ⌈compressed_size_MiB / 2⌉ huge pages
//! sudo sysctl vm.nr_hugepages=160
//! grep HugePages_Total /proc/meminfo
//! ```

/// Streaming zstd [`Decoder`][decoder::Decoder] backed by mmap or hugepage memory.
pub mod decoder;
