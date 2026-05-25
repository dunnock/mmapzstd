//! mmap-backed zstd decompressor.
//!
//! Decompresses `.zst` files by memory-mapping the compressed input, then
//! streaming decompressed bytes to a caller-provided sink. The caller drives
//! the read; the library does not buffer ahead of demand beyond the zstd
//! frame buffer. See [`decoder::Decoder`] for the public API.

pub mod decoder;
