use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use zstd::stream::raw::{Decoder as ZstdDecoder, Operation};

pub struct Decoder<'a> {
    mmap: Mmap,
    in_pos: usize,
    retire_pos: usize,
    zstd: ZstdDecoder<'static>,
    overflow: Vec<u8>,
    overflow_start: usize,
    _marker: std::marker::PhantomData<&'a ()>,
}

impl Decoder<'static> {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the file is opened read-only and we do not mutate it.
        let mmap = unsafe { MmapOptions::new().map(&file)? };
        Self::from_mmap(mmap)
    }

    pub fn from_mmap(mmap: Mmap) -> io::Result<Self> {
        apply_madvise(&mmap)?;
        let zstd = ZstdDecoder::new()?;
        Ok(Decoder {
            mmap,
            in_pos: 0,
            retire_pos: 0,
            zstd,
            overflow: Vec::new(),
            overflow_start: 0,
            _marker: std::marker::PhantomData,
        })
    }
}

const BLOCK_SIZE: usize = 128 * 1024; // zstd output blocks are at most ~128 KiB
const RETIRE_WINDOW: usize = 4 * 1024 * 1024; // 4 MiB trailing window before retirement

impl<'a> Decoder<'a> {
    #[cfg(target_os = "linux")]
    fn maybe_retire(&mut self) {
        let new_frontier = self.in_pos.saturating_sub(RETIRE_WINDOW);
        let new_frontier = new_frontier & !(4096 - 1); // align down to page boundary
        if new_frontier > self.retire_pos {
            // SAFETY: We only retire pages we have already consumed (before
            // in_pos - RETIRE_WINDOW). The zstd compressed-input cursor is
            // strictly monotonically increasing, so no future read will touch
            // these bytes again. MADV_DONTNEED on a read-only file mapping
            // simply lets the kernel reclaim pages; re-faults would reload
            // from the file, but they won't happen here.
            let _ = unsafe {
                self.mmap.unchecked_advise_range(
                    memmap2::UncheckedAdvice::DontNeed,
                    self.retire_pos,
                    new_frontier - self.retire_pos,
                )
            };
            self.retire_pos = new_frontier;
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn maybe_retire(&mut self) {}
}

impl<'a> io::Read for Decoder<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // Drain spill buffer from a previous oversized block before decoding more.
        if self.overflow_start < self.overflow.len() {
            let available = self.overflow.len() - self.overflow_start;
            let n = available.min(buf.len());
            buf[..n].copy_from_slice(&self.overflow[self.overflow_start..self.overflow_start + n]);
            self.overflow_start += n;
            if self.overflow_start == self.overflow.len() {
                self.overflow.clear();
                self.overflow_start = 0;
            }
            return Ok(n);
        }

        // Decode one block into the overflow buffer (reused as a staging area).
        // Passing an empty slice when in_pos == mmap.len() flushes any trailing
        // frame bytes; the decoder returns bytes_written == 0 when truly done.
        self.overflow.resize(BLOCK_SIZE, 0);

        let input = &self.mmap[self.in_pos..];
        let status = self
            .zstd
            .run_on_buffers(input, &mut self.overflow)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.in_pos += status.bytes_read;
        self.maybe_retire();
        self.overflow.truncate(status.bytes_written);
        self.overflow_start = 0;

        if status.bytes_written == 0 {
            return Ok(0);
        }

        let n = status.bytes_written.min(buf.len());
        buf[..n].copy_from_slice(&self.overflow[..n]);
        self.overflow_start = n;

        if self.overflow_start == self.overflow.len() {
            self.overflow.clear();
            self.overflow_start = 0;
        }

        Ok(n)
    }
}

fn apply_madvise(mmap: &Mmap) -> io::Result<()> {
    #[cfg(unix)]
    {
        use memmap2::Advice;
        mmap.advise(Advice::Sequential)?;
        #[cfg(target_os = "linux")]
        mmap.advise(Advice::HugePage)?;
    }
    #[cfg(not(unix))]
    let _ = mmap;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn compressed_tempfile(data: &[u8]) -> tempfile::NamedTempFile {
        let compressed = zstd::encode_all(data, 0).expect("encode_all");
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        f.write_all(&compressed).expect("write");
        f
    }

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0u16..1024).map(|i| (i % 251) as u8).collect();
        let f = compressed_tempfile(&data);

        let mut dec = Decoder::open(f.path()).expect("open");
        let mut got = Vec::new();
        dec.read_to_end(&mut got).expect("read_to_end");

        assert_eq!(got, data);
    }

    #[test]
    fn retire_pos_advances_after_large_read() {
        // Use poorly-compressible (LCG pseudo-random) data so the compressed
        // file is > RETIRE_WINDOW (4 MiB), ensuring madvise is actually called.
        let data: Vec<u8> = (0u32..10 * 1024 * 1024 / 4)
            .flat_map(|i| {
                i.wrapping_mul(1664525)
                    .wrapping_add(1013904223)
                    .to_le_bytes()
            })
            .collect();
        let compressed = zstd::encode_all(data.as_slice(), 3).expect("encode_all");
        let compressed_len = compressed.len();

        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        use std::io::Write;
        tmp.write_all(&compressed).expect("write");

        let mut dec = Decoder::open(tmp.path()).expect("open");
        let mut got = Vec::with_capacity(data.len());
        dec.read_to_end(&mut got).expect("read_to_end");
        assert_eq!(got, data);

        // Retirement only fires when the compressed input exceeds RETIRE_WINDOW.
        if compressed_len > RETIRE_WINDOW {
            #[cfg(target_os = "linux")]
            assert!(dec.retire_pos > 0, "retire_pos should advance on linux");
        }
    }

    #[test]
    fn small_buf_exercises_overflow() {
        let data: Vec<u8> = (0u16..4096).map(|i| (i % 199) as u8).collect();
        let f = compressed_tempfile(&data);

        let mut dec = Decoder::open(f.path()).expect("open");
        let mut got = Vec::with_capacity(data.len());
        let mut one = [0u8; 1];
        loop {
            match dec.read(&mut one).expect("read") {
                0 => break,
                n => got.extend_from_slice(&one[..n]),
            }
        }

        assert_eq!(got, data);
    }
}
