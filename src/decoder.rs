use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use zstd::stream::raw::{Decoder as ZstdDecoder, Operation};

pub struct Decoder<'a> {
    mmap: Mmap,
    in_pos: usize,
    #[allow(dead_code)] // used by page-retire task
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

// Zstd output blocks are at most ~128 KiB.
const BLOCK_SIZE: usize = 128 * 1024;

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
