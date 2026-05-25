use std::io;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};

#[allow(dead_code)]
pub struct Decoder<'a> {
    mmap: Mmap,
    in_pos: usize,
    retire_pos: usize,
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
        Ok(Decoder {
            mmap,
            in_pos: 0,
            retire_pos: 0,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<'a> io::Read for Decoder<'a> {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        todo!("zstd decode not yet implemented")
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
