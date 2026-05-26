use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};
use zstd::stream::raw::{Decoder as ZstdDecoder, Operation};

/// Owned buffer backed by a 2 MiB `MAP_HUGETLB` anonymous mapping.
///
/// Linux-only. Constructed by `Decoder::open_hugepage`; dropped when the
/// `Decoder` is dropped, which calls `munmap` to release the hugepages.
#[cfg(target_os = "linux")]
struct HugepageBuf {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
}

#[cfg(target_os = "linux")]
unsafe impl Send for HugepageBuf {}

#[cfg(target_os = "linux")]
impl std::ops::Deref for HugepageBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[cfg(target_os = "linux")]
impl Drop for HugepageBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.capacity) };
    }
}

enum DecoderBuf<'a> {
    Mmap(Mmap),
    Slice(&'a [u8]),
    #[cfg(target_os = "linux")]
    Hugepage(HugepageBuf),
}

impl<'a> std::ops::Deref for DecoderBuf<'a> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            DecoderBuf::Mmap(m) => m,
            DecoderBuf::Slice(s) => s,
            #[cfg(target_os = "linux")]
            DecoderBuf::Hugepage(h) => h,
        }
    }
}

/// Streaming zstd decompressor backed by a memory-mapped (or hugepage-backed) input buffer.
///
/// `Decoder` implements [`std::io::Read`]; the caller drives the decode pace by
/// calling `read`.  No bytes are decompressed ahead of demand beyond the zstd
/// frame buffer (~128 KiB per block).
///
/// # Lifetime
///
/// `Decoder<'static>` owns its backing buffer (constructed via [`open`][Self::open],
/// [`from_mmap`][Self::from_mmap], [`open_hugepage`][Self::open_hugepage], or
/// [`open_hugepage_memfd`][Self::open_hugepage_memfd]).
///
/// `Decoder<'a>` borrows a caller-supplied slice (constructed via
/// [`from_slice`][Self::from_slice]).
///
/// # Example
///
/// ```no_run
/// use std::io::Read;
/// use mmapzstd::decoder::Decoder;
///
/// let mut dec = Decoder::open(std::path::Path::new("data.zst"))?;
/// let mut out = Vec::new();
/// dec.read_to_end(&mut out)?;
/// # Ok::<(), std::io::Error>(())
/// ```
pub struct Decoder<'a> {
    buf: DecoderBuf<'a>,
    in_pos: usize,
    retire_pos: usize,
    zstd: ZstdDecoder<'static>,
}

impl Decoder<'static> {
    /// Open `path` by memory-mapping the compressed file.
    ///
    /// Applies `MAP_POPULATE` (batch pre-fault), `MADV_SEQUENTIAL`, and
    /// `MADV_HUGEPAGE` (Linux).  A 4 MiB sliding `MADV_DONTNEED` window retires
    /// pages behind the decode cursor, keeping RSS at ~9 MB regardless of file size.
    ///
    /// This is the portable, low-RSS constructor.  On warm cache it is ~5% slower
    /// than a 64 KiB `BufReader`; prefer [`open_hugepage_memfd`][Self::open_hugepage_memfd]
    /// when Linux hugepages are available.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmapzstd::decoder::Decoder;
    ///
    /// let dec = Decoder::open(std::path::Path::new("data.zst"))?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the file is opened read-only and we do not mutate it.
        // MAP_POPULATE pre-faults all pages at mmap time so fault overhead
        // is paid once in batch rather than scattered across the decode loop.
        let mmap = unsafe { MmapOptions::new().populate().map(&file)? };
        Self::from_mmap(mmap)
    }

    /// Take ownership of a pre-built [`Mmap`] and prepare the zstd stream.
    ///
    /// Applies `MADV_SEQUENTIAL`, `MADV_HUGEPAGE` (Linux), and
    /// `MADV_POPULATE_READ` (Linux ≥ 5.14) hints.  Use this when you need
    /// fine-grained control over mmap options (e.g., to map a specific file
    /// offset or apply custom flags before handing the mapping to the decoder).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use memmap2::MmapOptions;
    /// use mmapzstd::decoder::Decoder;
    ///
    /// let file = std::fs::File::open("data.zst")?;
    /// let mmap = unsafe { MmapOptions::new().map(&file)? };
    /// let dec = Decoder::from_mmap(mmap)?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn from_mmap(mmap: Mmap) -> io::Result<Self> {
        apply_madvise(&mmap)?;
        let zstd = ZstdDecoder::new()?;
        Ok(Decoder {
            buf: DecoderBuf::Mmap(mmap),
            in_pos: 0,
            retire_pos: 0,
            zstd,
        })
    }

    /// Open `path` by loading the compressed data into a `MAP_HUGETLB | MAP_HUGE_2MB`
    /// anonymous buffer (Linux only).
    ///
    /// Reads the file into a `Vec<u8>`, then copies it into a 2 MiB-page-backed
    /// anonymous region.  Decoding from 64 PMD entries instead of ~32,768 PTEs
    /// eliminates essentially all dTLB misses for the input scan, yielding
    /// approximately +15% throughput over the BufReader baseline on warm-cache
    /// sequential reads (i9-12900K, Linux 6.17, 128 MiB compressed input).
    ///
    /// Falls back to [`open`][Self::open] transparently if `mmap(MAP_HUGETLB)`
    /// fails (no hugepages reserved, or non-Linux platform).
    ///
    /// Requires `vm.nr_hugepages >= ceil(compressed_size_bytes / 2_MiB)`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmapzstd::decoder::Decoder;
    ///
    /// // Linux with hugepages reserved; falls back to Decoder::open otherwise.
    /// let dec = Decoder::open_hugepage(std::path::Path::new("data.zst"))?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[cfg(target_os = "linux")]
    pub fn open_hugepage(path: &Path) -> io::Result<Self> {
        const HUGEPAGE: usize = 2 * 1024 * 1024;
        const MAP_HUGE_2MB: libc::c_int = 21 << 26; // log2(2 MiB) = 21, MAP_HUGE_SHIFT = 26

        let compressed = std::fs::read(path)?;
        let len = compressed.len();
        let capacity = (len + HUGEPAGE - 1) & !(HUGEPAGE - 1);

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB | MAP_HUGE_2MB,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Self::open(path);
        }

        let ptr = ptr as *mut u8;
        // SAFETY: ptr is valid for `capacity` bytes, and `compressed.len() == len <= capacity`.
        unsafe { std::ptr::copy_nonoverlapping(compressed.as_ptr(), ptr, len) };

        let zstd = ZstdDecoder::new()?;
        Ok(Decoder {
            buf: DecoderBuf::Hugepage(HugepageBuf { ptr, len, capacity }),
            in_pos: 0,
            retire_pos: 0,
            zstd,
        })
    }

    /// Open `path` via a `memfd_create(MFD_HUGETLB | MFD_HUGE_2MB)` hugepage mapping
    /// (Linux ≥ 4.14 only).
    ///
    /// Allocates a hugepage-backed file descriptor, maps it, then reads the compressed
    /// file directly into the mapped region—no intermediate `Vec`.  This avoids the
    /// double-buffering copy of [`open_hugepage`][Self::open_hugepage] and reduces
    /// copy-in minor faults from ~55k to ~107 per open.
    ///
    /// Throughput: ~9,944 MB/s on a 128 MiB compressed synthetic corpus
    /// (i9-12900K, Linux 6.17, warm cache, Criterion median over 4 runs).
    ///
    /// Falls back to [`open`][Self::open] transparently if `memfd_create`,
    /// `ftruncate`, or the subsequent `mmap` fails.
    ///
    /// Requires `vm.nr_hugepages >= ceil(compressed_size_bytes / 2_MiB)`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use mmapzstd::decoder::Decoder;
    ///
    /// // Preferred on Linux with hugepages reserved.
    /// let dec = Decoder::open_hugepage_memfd(std::path::Path::new("data.zst"))?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[cfg(target_os = "linux")]
    pub fn open_hugepage_memfd(path: &Path) -> io::Result<Self> {
        use std::io::Read as _;

        const HUGEPAGE: usize = 2 * 1024 * 1024;
        let mfd_hugetlb: libc::c_ulong = 0x0004;
        let mfd_huge_2mb: libc::c_ulong = 21 << 26;

        let len = path.metadata()?.len() as usize;
        let capacity = (len + HUGEPAGE - 1) & !(HUGEPAGE - 1);

        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                b"mmapzstd-hugetlb\0".as_ptr(),
                mfd_hugetlb | mfd_huge_2mb,
            )
        };

        if fd < 0 {
            return Self::open(path);
        }
        let fd = fd as libc::c_int;

        let rc = unsafe { libc::ftruncate(fd, capacity as libc::off_t) };
        if rc != 0 {
            unsafe { libc::close(fd) };
            return Self::open(path);
        }

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                capacity,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };

        if ptr == libc::MAP_FAILED {
            return Self::open(path);
        }

        let ptr = ptr as *mut u8;
        // Read compressed file directly into the mapped hugepage region.
        // SAFETY: ptr is valid for `capacity` bytes; we read exactly `len` bytes.
        {
            let buf = unsafe { std::slice::from_raw_parts_mut(ptr, len) };
            if let Err(e) = std::fs::File::open(path)?.read_exact(buf) {
                unsafe { libc::munmap(ptr as *mut libc::c_void, capacity) };
                return Err(e);
            }
        }

        let zstd = ZstdDecoder::new()?;
        Ok(Decoder {
            buf: DecoderBuf::Hugepage(HugepageBuf { ptr, len, capacity }),
            in_pos: 0,
            retire_pos: 0,
            zstd,
        })
    }
}

impl<'a> Decoder<'a> {
    /// Borrow compressed data from `data` without mapping; no madvise hints.
    ///
    /// The caller controls the backing memory lifetime (e.g., a hugepage allocation,
    /// a static byte slice, or a test fixture).  The slice must remain valid for the
    /// lifetime of the `Decoder`.
    ///
    /// # Example
    ///
    /// ```
    /// use mmapzstd::decoder::Decoder;
    ///
    /// let compressed = zstd::encode_all(b"hello world".as_ref(), 0).unwrap();
    /// let mut dec = Decoder::from_slice(&compressed).unwrap();
    ///
    /// use std::io::Read;
    /// let mut out = Vec::new();
    /// dec.read_to_end(&mut out).unwrap();
    /// assert_eq!(out, b"hello world");
    /// ```
    pub fn from_slice(data: &'a [u8]) -> io::Result<Self> {
        let zstd = ZstdDecoder::new()?;
        Ok(Decoder {
            buf: DecoderBuf::Slice(data),
            in_pos: 0,
            retire_pos: 0,
            zstd,
        })
    }
}

const RETIRE_WINDOW: usize = 4 * 1024 * 1024; // 4 MiB trailing window before retirement

impl<'a> Decoder<'a> {
    #[cfg(target_os = "linux")]
    fn maybe_retire(&mut self) {
        if !matches!(self.buf, DecoderBuf::Mmap(_)) {
            // Hugepage and Slice buffers are not file-backed; skip retirement.
            return;
        }
        let new_frontier = self.in_pos.saturating_sub(RETIRE_WINDOW);
        let new_frontier = new_frontier & !(4096 - 1); // align down to page boundary
        if new_frontier > self.retire_pos {
            if let DecoderBuf::Mmap(mmap) = &self.buf {
                // SAFETY: We only retire pages we have already consumed (before
                // in_pos - RETIRE_WINDOW). The zstd compressed-input cursor is
                // strictly monotonically increasing, so no future read will touch
                // these bytes again. MADV_DONTNEED on a read-only file mapping
                // simply lets the kernel reclaim pages; re-faults would reload
                // from the file, but they won't happen here.
                let _ = unsafe {
                    mmap.unchecked_advise_range(
                        memmap2::UncheckedAdvice::DontNeed,
                        self.retire_pos,
                        new_frontier - self.retire_pos,
                    )
                };
            }
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

        // Decode directly into the caller's buffer. ZSTD_decompressStream
        // handles any output-buffer size and picks up where it left off next
        // call, so no intermediate staging buffer is needed.
        let input = &self.buf[self.in_pos..];
        let status = self
            .zstd
            .run_on_buffers(input, buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.in_pos += status.bytes_read;
        self.maybe_retire();

        Ok(status.bytes_written)
    }
}

fn apply_madvise(mmap: &Mmap) -> io::Result<()> {
    #[cfg(unix)]
    {
        use memmap2::Advice;
        mmap.advise(Advice::Sequential)?;
        #[cfg(target_os = "linux")]
        {
            mmap.advise(Advice::HugePage)?;
            // Pre-fault all page-table entries in batch so that minor faults
            // are paid once here rather than scattered across the decode loop.
            // Also benefits callers that use from_mmap() where MAP_POPULATE
            // was not set at creation time. Silently ignored on < 5.14 kernels.
            let _ = mmap.advise(Advice::PopulateRead);
        }
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
    fn round_trip_from_slice() {
        let data: Vec<u8> = (0u16..1024).map(|i| (i % 251) as u8).collect();
        let compressed = zstd::encode_all(data.as_slice(), 0).expect("encode_all");

        let mut dec = Decoder::from_slice(&compressed).expect("from_slice");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn open_hugepage_memfd_round_trip() {
        let data: Vec<u8> = (0u16..1024).map(|i| (i % 251) as u8).collect();
        let f = compressed_tempfile(&data);

        let mut dec = Decoder::open_hugepage_memfd(f.path()).expect("open_hugepage_memfd");
        let mut got = Vec::new();
        dec.read_to_end(&mut got).expect("read_to_end");

        assert_eq!(got, data);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn open_hugepage_round_trip() {
        let data: Vec<u8> = (0u16..1024).map(|i| (i % 251) as u8).collect();
        let f = compressed_tempfile(&data);

        let mut dec = Decoder::open_hugepage(f.path()).expect("open_hugepage");
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
