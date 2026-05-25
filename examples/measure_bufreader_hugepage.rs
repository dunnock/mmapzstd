/// Benchmark hugepage-scratch BufReader variants for zstd decompression.
///
/// H4a  — 2 MiB MAP_HUGETLB anon scratch buffer, 64 KiB read chunks
/// H4b  — 2 MiB MAP_HUGETLB anon scratch buffer, 256 KiB read chunks
/// H4c  — 2 MiB MAP_HUGETLB anon scratch buffer, 64 KiB pread64 chunks
/// H4-ctrl — normal BufReader 64 KiB on 4 KiB pages (cycle-02 baseline)
///
/// Hypothesis: if the scratch buffer lives on a single 2 MiB huge page the
/// user-side TLB needs only one entry for the entire buffer, reducing TLB
/// pressure vs the standard allocator which may scatter 16+ 4 KiB entries.
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rand::RngCore;
use tempfile::NamedTempFile;

const DECOMPRESSED_BYTES: u64 = 256 * 1024 * 1024;
const SCRATCH_LEN: usize = 2 * 1024 * 1024; // 2 MiB
const MAP_HUGE_2MB: libc::c_int = 21 << 26; // MAP_HUGE_SHIFT=26, 21=log2(2MiB)

// ── hugepage allocator ────────────────────────────────────────────────────────

struct HugepageAlloc {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: we never share across threads in this single-threaded example.
unsafe impl Send for HugepageAlloc {}

impl HugepageAlloc {
    /// Try `MAP_HUGETLB`; return `None` if unavailable.
    fn try_hugepage(len: usize) -> Option<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB | MAP_HUGE_2MB,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            None
        } else {
            Some(HugepageAlloc { ptr, len })
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr as *mut u8
    }
}

impl Drop for HugepageAlloc {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

// ── scratch-backed reader ─────────────────────────────────────────────────────

/// A BufReader equivalent that uses a caller-supplied hugepage region as its
/// internal buffer.  On each refill it reads `chunk_size` bytes from `inner`
/// into the first `chunk_size` bytes of `alloc`, which always belong to the
/// same 2 MiB TLB entry.
struct ScratchReader {
    inner: File,
    alloc: HugepageAlloc,
    chunk_size: usize,
    start: usize,
    end: usize,
    /// For pread variant: explicit file offset (avoids kernel f_pos update).
    file_offset: i64,
    use_pread: bool,
}

impl ScratchReader {
    fn new(file: File, alloc: HugepageAlloc, chunk_size: usize, use_pread: bool) -> Self {
        ScratchReader {
            inner: file,
            alloc,
            chunk_size,
            start: 0,
            end: 0,
            file_offset: 0,
            use_pread,
        }
    }

    fn refill(&mut self) -> io::Result<()> {
        let to_read = self.chunk_size.min(self.alloc.len);
        let buf_ptr = self.alloc.as_mut_ptr();
        let n = if self.use_pread {
            let rc = unsafe {
                libc::pread(
                    self.inner.as_raw_fd(),
                    buf_ptr as *mut libc::c_void,
                    to_read,
                    self.file_offset,
                )
            };
            if rc < 0 {
                return Err(io::Error::last_os_error());
            }
            self.file_offset += rc as i64;
            rc as usize
        } else {
            use io::Read;
            let buf = unsafe { std::slice::from_raw_parts_mut(buf_ptr, to_read) };
            self.inner.read(buf)?
        };
        self.start = 0;
        self.end = n;
        Ok(())
    }
}

impl io::Read for ScratchReader {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if self.start >= self.end {
            self.refill()?;
        }
        if self.start >= self.end {
            return Ok(0);
        }
        let available = self.end - self.start;
        let to_copy = available.min(dst.len());
        let src = unsafe {
            std::slice::from_raw_parts(self.alloc.as_mut_ptr().add(self.start), to_copy)
        };
        dst[..to_copy].copy_from_slice(src);
        self.start += to_copy;
        Ok(to_copy)
    }
}

// ── /proc helpers ─────────────────────────────────────────────────────────────

fn read_proc_stat() -> (u64, u64) {
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let minflt: u64 = fields.get(9).and_then(|s| s.parse().ok()).unwrap_or(0);
    let majflt: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    (minflt, majflt)
}

fn read_vm_pte_kb() -> u64 {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmPTE:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn read_hugepages_free() -> u64 {
    fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("HugePages_Free:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

// ── fixture ───────────────────────────────────────────────────────────────────

fn get_or_create_fixture() -> (PathBuf, Option<NamedTempFile>) {
    let persistent = PathBuf::from("/work/cargo-target-ralph/mmapzstd-fixtures/decompress_256mib.zst");
    if persistent.exists() {
        return (persistent, None);
    }
    eprintln!("Persistent fixture not found; generating 256 MiB temp fixture…");
    let tmp = make_temp_fixture();
    let path = tmp.path().to_path_buf();
    (path, Some(tmp))
}

fn make_temp_fixture() -> NamedTempFile {
    const BLOCK: usize = 4096;
    const TOTAL: usize = 256 * 1024 * 1024;
    let mut data = vec![0u8; TOTAL];
    let mut rng = rand::thread_rng();
    let mut offset = 0;
    while offset < TOTAL {
        let end = (offset + BLOCK).min(TOTAL);
        rng.fill_bytes(&mut data[offset..end]);
        offset = end;
        let end = (offset + BLOCK).min(TOTAL);
        for b in &mut data[offset..end] {
            *b = 0xAB;
        }
        offset = end;
    }
    let compressed = zstd::encode_all(data.as_slice(), 3).expect("encode fixture");
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(&compressed).expect("write fixture");
    f
}

// ── bench harness ─────────────────────────────────────────────────────────────

fn measure_once(path: &Path, factory: &dyn Fn(&Path) -> Box<dyn io::Read>) -> (Duration, u64) {
    let (minflt_before, _) = read_proc_stat();
    let start = std::time::Instant::now();
    let mut reader = factory(path);
    io::copy(&mut reader, &mut io::sink()).expect("io::copy");
    let elapsed = start.elapsed();
    let (minflt_after, _) = read_proc_stat();
    (elapsed, minflt_after - minflt_before)
}

fn bench(
    label: &str,
    path: &Path,
    factory: &dyn Fn(&Path) -> Box<dyn io::Read>,
    pte_kb_before: u64,
) {
    const TOTAL_RUNS: usize = 5;
    const WARMUP: usize = 2;

    let mut all: Vec<(Duration, u64)> = Vec::with_capacity(TOTAL_RUNS);
    for _ in 0..TOTAL_RUNS {
        all.push(measure_once(path, factory));
    }

    let measured = &all[WARMUP..];
    let n = measured.len() as f64;
    let avg_ms = measured.iter().map(|(d, _)| d.as_secs_f64()).sum::<f64>() * 1000.0 / n;
    let avg_faults = measured.iter().map(|(_, f)| *f).sum::<u64>() / measured.len() as u64;
    let throughput_mbs = DECOMPRESSED_BYTES as f64 / 1_048_576.0 / (avg_ms / 1000.0);
    let pte_kb_after = read_vm_pte_kb();
    let pte_delta = pte_kb_after.saturating_sub(pte_kb_before);

    println!(
        "{:<52}  {:>8.2} ms  {:>9.1} MB/s  {:>6} faults  VmPTE+{} kB",
        label, avg_ms, throughput_mbs, avg_faults, pte_delta
    );
}

// ── hugepage availability check ───────────────────────────────────────────────

fn check_hugepages_available() -> bool {
    let test = HugepageAlloc::try_hugepage(SCRATCH_LEN);
    test.is_some()
    // HugepageAlloc dropped here → munmap called
}

fn maybe_emit_escalation() {
    let free = read_hugepages_free();
    if free > 0 {
        return; // hugepages exist but failed for another reason — don't escalate
    }
    let path =
        "/work/ralph-self-improvement/workspace/.escalations/cycle03-hugepages-setup.md";
    if std::path::Path::new(path).exists() {
        return;
    }
    let content = r#"# Escalation: Reserve Static Huge Pages (cycle-03 H4a/H4b/H4c)

## What I need

`vm.nr_hugepages` set to at least 160 so that `MAP_HUGETLB | MAP_HUGE_2MB`
succeeds for the mmapzstd cycle-03 hugepage-scratch benchmarks.

## Context

Task `hugepage-bufreader-scratch` (cycle-03) is testing H4 variants where
the BufReader scratch buffer is placed on a 2 MiB `MAP_HUGETLB` anonymous
page. H4a/H4b/H4c all failed with ENOMEM because `HugePages_Free=0`.
160 × 2 MiB = 320 MiB covers the 2 MiB scratch × multiple runs with room
to spare.

## Exact command

```bash
sudo sysctl vm.nr_hugepages=160
```

To revert after testing:

```bash
sudo sysctl vm.nr_hugepages=0
```

## Expected outcome

After applying, re-run from `/work/mmapzstd/.worktrees/03-hugepages`:

```bash
CARGO_TARGET_DIR=/work/cargo-target-ralph cargo run --release --example measure_bufreader_hugepage
```

The H4a/H4b/H4c variants will show results instead of "Skipped".
"#;
    let _ = fs::write(path, content);
    eprintln!("Escalation written to {path}");
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let (path, _fixture_guard) = get_or_create_fixture();

    println!("=== measure_bufreader_hugepage ===");
    println!("Fixture: {}", path.display());
    println!();

    // VmPTE baseline before any allocation.
    let pte_base = read_vm_pte_kb();
    println!("VmPTE baseline: {} kB", pte_base);

    // Check and report hugepage availability.
    let hugepages_ok = check_hugepages_available();
    if hugepages_ok {
        println!("MAP_HUGETLB: available");
    } else {
        let errno = unsafe { *libc::__errno_location() };
        let free = read_hugepages_free();
        println!(
            "MAP_HUGETLB: UNAVAILABLE (errno={errno}, HugePages_Free={free})"
        );
        if free == 0 {
            maybe_emit_escalation();
        }
    }
    println!();

    // Warm the page cache.
    measure_once(&path, &|p| {
        let f = File::open(p).expect("open");
        let buf = BufReader::with_capacity(65_536, f);
        Box::new(zstd::stream::Decoder::new(buf).expect("decoder"))
    });

    println!(
        "{:<52}  {:>10}  {:>11}  {:>12}  {:>10}",
        "variant", "avg ms", "throughput", "min-faults", "VmPTE delta"
    );
    println!("{}", "-".repeat(103));

    // H4-ctrl — normal BufReader 64 KiB (cycle-02 baseline, always runs)
    let pte_before_ctrl = read_vm_pte_kb();
    bench("H4-ctrl BufReader 64 KiB (normal pages)", &path, &|p| {
        let f = File::open(p).expect("open");
        let buf = BufReader::with_capacity(65_536, f);
        Box::new(zstd::stream::Decoder::new(buf).expect("decoder"))
    }, pte_before_ctrl);

    if hugepages_ok {
        // H4a — hugepage scratch, 64 KiB read chunks
        let pte_before = read_vm_pte_kb();
        bench("H4a hugepage-scratch 64 KiB chunks (read)", &path, &|p| {
            let alloc = HugepageAlloc::try_hugepage(SCRATCH_LEN).expect("MAP_HUGETLB");
            let f = File::open(p).expect("open");
            let scratch = ScratchReader::new(f, alloc, 64 * 1024, false);
            Box::new(zstd::stream::Decoder::new(scratch).expect("decoder"))
        }, pte_before);

        // H4b — hugepage scratch, 256 KiB read chunks
        let pte_before = read_vm_pte_kb();
        bench("H4b hugepage-scratch 256 KiB chunks (read)", &path, &|p| {
            let alloc = HugepageAlloc::try_hugepage(SCRATCH_LEN).expect("MAP_HUGETLB");
            let f = File::open(p).expect("open");
            let scratch = ScratchReader::new(f, alloc, 256 * 1024, false);
            Box::new(zstd::stream::Decoder::new(scratch).expect("decoder"))
        }, pte_before);

        // H4c — hugepage scratch, 64 KiB pread64 chunks
        let pte_before = read_vm_pte_kb();
        bench("H4c hugepage-scratch 64 KiB chunks (pread64)", &path, &|p| {
            let alloc = HugepageAlloc::try_hugepage(SCRATCH_LEN).expect("MAP_HUGETLB");
            let f = File::open(p).expect("open");
            let scratch = ScratchReader::new(f, alloc, 64 * 1024, true);
            Box::new(zstd::stream::Decoder::new(scratch).expect("decoder"))
        }, pte_before);
    } else {
        println!("{:<52}  SKIPPED: operator pending (HugePages_Free=0)", "H4a hugepage-scratch 64 KiB chunks (read)");
        println!("{:<52}  SKIPPED: operator pending (HugePages_Free=0)", "H4b hugepage-scratch 256 KiB chunks (read)");
        println!("{:<52}  SKIPPED: operator pending (HugePages_Free=0)", "H4c hugepage-scratch 64 KiB chunks (pread64)");
    }

    println!();
    println!("Note: winner threshold = 8,634 MB/s × 1.05 = 9,066 MB/s");
    println!("      (cycle-02 BufReader-64-KiB baseline = 8,634 MB/s)");
}
