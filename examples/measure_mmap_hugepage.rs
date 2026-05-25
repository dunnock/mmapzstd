/// Measure hugepage promotion for the mmapzstd mmap region.
///
/// Reads /proc/self/smaps before and after decode to report:
///   - FilePmdMapped (2 MiB file-backed huge pages — nonzero if THP fired)
///   - AnonHugePages (should be 0 for file-backed mmap)
///   - THPeligible flag
///
/// Also tries H3b (MAP_HUGETLB anon) and H3c (memfd_create MFD_HUGETLB)
/// and reports success or ENOMEM.
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

const FIXTURE_DIR: &str = "/work/cargo-target-ralph/mmapzstd-fixtures";
const FIXTURE_NAME: &str = "decompress_256mib.zst";

fn main() -> io::Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(FIXTURE_DIR).join(FIXTURE_NAME));

    if !path.exists() {
        eprintln!("Fixture not found at {}. Run gen_fixture first.", path.display());
        std::process::exit(1);
    }

    println!("=== measure_mmap_hugepage ===");
    println!("Fixture: {}", path.display());
    println!();

    measure_h3a(&path)?;

    #[cfg(target_os = "linux")]
    {
        measure_h3b(&path);
        measure_h3c(&path);
    }

    Ok(())
}

fn measure_h3a(path: &Path) -> io::Result<()> {
    println!("--- H3a: file mmap + MADV_HUGEPAGE on ext4 ---");

    // Open and mmap (same as Decoder::open internals).
    let file = File::open(path)?;
    let mmap = unsafe { memmap2::MmapOptions::new().populate().map(&file)? };

    // Apply the same madvise hints as Decoder::open.
    {
        use memmap2::Advice;
        mmap.advise(Advice::Sequential)?;
        #[cfg(target_os = "linux")]
        {
            mmap.advise(Advice::HugePage)?;
            let _ = mmap.advise(Advice::PopulateRead);
        }
    }

    let mmap_addr = mmap.as_ptr() as usize;
    let mmap_len = mmap.len();
    println!(
        "mmap: addr=0x{mmap_addr:016x}  len={} bytes ({:.1} MiB)",
        mmap_len,
        mmap_len as f64 / 1024.0 / 1024.0
    );

    // Read smaps before decode.
    let before = read_smaps_for_addr(mmap_addr, mmap_len);
    println!("smaps BEFORE decode:");
    print_smaps_stats(&before);

    // Decode to sink (same workload as the bench).
    let start = Instant::now();
    let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap)?;
    let decompressed_bytes = io::copy(&mut dec, &mut io::sink())?;
    let elapsed = start.elapsed();

    let throughput = decompressed_bytes as f64 / elapsed.as_secs_f64() / 1024.0 / 1024.0;
    println!(
        "decode: {:.2} ms  ({:.0} MB/s decompressed)",
        elapsed.as_secs_f64() * 1000.0,
        throughput
    );

    // Re-open and re-mmap to read smaps at the same address range post-decode.
    let file2 = File::open(path)?;
    let mmap2 = unsafe { memmap2::MmapOptions::new().populate().map(&file2)? };
    {
        use memmap2::Advice;
        mmap2.advise(Advice::Sequential)?;
        #[cfg(target_os = "linux")]
        {
            mmap2.advise(Advice::HugePage)?;
            let _ = mmap2.advise(Advice::PopulateRead);
        }
    }
    let after = read_smaps_for_addr(mmap2.as_ptr() as usize, mmap2.len());
    println!("smaps AFTER fresh mmap + madvise:");
    print_smaps_stats(&after);
    println!();

    Ok(())
}

#[cfg(target_os = "linux")]
fn measure_h3b(path: &Path) {
    println!("--- H3b: MAP_ANON | MAP_HUGETLB | MAP_HUGE_2MB ---");

    let compressed_len = path.metadata().expect("stat").len() as usize;
    const HUGEPAGE: usize = 2 * 1024 * 1024;
    let aligned_len = (compressed_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);
    let map_huge_2mb: libc::c_int = 21 << 26;

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            aligned_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB | map_huge_2mb,
            -1,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        let errno = unsafe { *libc::__errno_location() };
        let hugepages_free = read_meminfo_hugepages_free();
        println!(
            "  SKIPPED: mmap(MAP_HUGETLB) failed errno={errno} — HugePages_Free={hugepages_free}"
        );
        emit_escalation();
        println!();
        return;
    }

    {
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, compressed_len) };
        File::open(path).expect("open").read_exact(buf).expect("read");
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, compressed_len) };

    let start = Instant::now();
    let mut dec = mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
    let decompressed = io::copy(&mut dec, &mut io::sink()).expect("copy");
    let elapsed = start.elapsed();

    let throughput = decompressed as f64 / elapsed.as_secs_f64() / 1024.0 / 1024.0;
    println!("  decode: {:.2} ms  ({:.0} MB/s)", elapsed.as_secs_f64() * 1000.0, throughput);

    unsafe { libc::munmap(ptr, aligned_len) };
    println!();
}

#[cfg(target_os = "linux")]
fn measure_h3c(path: &Path) {
    println!("--- H3c: memfd_create(MFD_HUGETLB | MFD_HUGE_2MB) ---");

    let mfd_hugetlb: libc::c_ulong = 0x0004;
    let mfd_huge_2mb: libc::c_ulong = 21 << 26;

    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            b"mmapzstd-hugetlb\0".as_ptr(),
            mfd_hugetlb | mfd_huge_2mb,
        )
    };

    if fd < 0 {
        let errno = unsafe { *libc::__errno_location() };
        let hugepages_free = read_meminfo_hugepages_free();
        println!(
            "  SKIPPED: memfd_create(MFD_HUGETLB) failed errno={errno} — HugePages_Free={hugepages_free}"
        );
        println!();
        return;
    }

    let compressed_len = path.metadata().expect("stat").len() as usize;
    const HUGEPAGE: usize = 2 * 1024 * 1024;
    let aligned_len = (compressed_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);

    let rc = unsafe { libc::ftruncate(fd as libc::c_int, aligned_len as libc::off_t) };
    if rc != 0 {
        let errno = unsafe { *libc::__errno_location() };
        println!("  SKIPPED: ftruncate failed errno={errno}");
        unsafe { libc::close(fd as libc::c_int) };
        println!();
        return;
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            aligned_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd as libc::c_int,
            0,
        )
    };
    unsafe { libc::close(fd as libc::c_int) };

    if ptr == libc::MAP_FAILED {
        let errno = unsafe { *libc::__errno_location() };
        println!("  SKIPPED: mmap of memfd failed errno={errno}");
        println!();
        return;
    }

    {
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, compressed_len) };
        File::open(path).expect("open").read_exact(buf).expect("read");
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, compressed_len) };

    let start = Instant::now();
    let mut dec = mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
    let decompressed = io::copy(&mut dec, &mut io::sink()).expect("copy");
    let elapsed = start.elapsed();

    let throughput = decompressed as f64 / elapsed.as_secs_f64() / 1024.0 / 1024.0;
    println!("  decode: {:.2} ms  ({:.0} MB/s)", elapsed.as_secs_f64() * 1000.0, throughput);

    unsafe { libc::munmap(ptr, aligned_len) };
    println!();
}

// ── /proc/self/smaps helpers ─────────────────────────────────────────────────

#[derive(Default, Debug)]
struct SmapsStats {
    file_pmd_mapped_kb: u64,
    anon_huge_pages_kb: u64,
    thp_eligible: Option<u8>,
    size_kb: u64,
    rss_kb: u64,
}

fn read_smaps_for_addr(addr: usize, len: usize) -> SmapsStats {
    let end_addr = addr + len;
    let smaps = match std::fs::read_to_string("/proc/self/smaps") {
        Ok(s) => s,
        Err(_) => return SmapsStats::default(),
    };

    let mut stats = SmapsStats::default();
    let mut in_region = false;

    for line in smaps.lines() {
        // Header line: "7f...000-7f...000 r--p ..."
        if let Some((range, _)) = line.split_once(' ') {
            if let Some((start_hex, end_hex)) = range.split_once('-') {
                if let (Ok(s), Ok(e)) = (
                    usize::from_str_radix(start_hex, 16),
                    usize::from_str_radix(end_hex, 16),
                ) {
                    // Enter region if our mmap address falls within this VMA.
                    in_region = s <= addr && addr < e && e <= end_addr + 4096;
                    continue;
                }
            }
        }

        if !in_region {
            continue;
        }

        if let Some(rest) = line.strip_prefix("FilePmdMapped:") {
            stats.file_pmd_mapped_kb += parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("AnonHugePages:") {
            stats.anon_huge_pages_kb += parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("THPeligible:") {
            stats.thp_eligible = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("Size:") {
            stats.size_kb += parse_kb(rest);
        } else if let Some(rest) = line.strip_prefix("Rss:") {
            stats.rss_kb += parse_kb(rest);
        }
    }

    stats
}

fn parse_kb(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn print_smaps_stats(s: &SmapsStats) {
    println!("  Size:          {:8} kB", s.size_kb);
    println!("  Rss:           {:8} kB", s.rss_kb);
    println!(
        "  FilePmdMapped: {:8} kB  ← 2 MiB huge pages (nonzero = THP active)",
        s.file_pmd_mapped_kb
    );
    println!(
        "  AnonHugePages: {:8} kB  ← should be 0 for file-backed mmap",
        s.anon_huge_pages_kb
    );
    if let Some(v) = s.thp_eligible {
        println!("  THPeligible:          {v}  ← 1 = kernel considers mapping THP-eligible");
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn read_meminfo_hugepages_free() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("HugePages_Free:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn emit_escalation() {
    let path = "/work/ralph-self-improvement/workspace/.escalations/cycle03-hugepages-setup.md";
    if std::path::Path::new(path).exists() {
        return;
    }
    let content = r#"# Escalation: Reserve Static Huge Pages for H3b/H3c (cycle-03)

## What I need

`vm.nr_hugepages` set to at least 160 so that `MAP_HUGETLB | MAP_HUGE_2MB` and
`memfd_create(MFD_HUGETLB)` succeed for the mmapzstd cycle-03 hugepage benchmarks.

## Context

Task `hugepage-mmap` (cycle-03) is testing H3 variants:
- **H3a**: file mmap on ext4 + MADV_HUGEPAGE — running, results captured in docs.
- **H3b**: compressed data in MAP_ANON|MAP_HUGETLB region, decode via `Decoder::from_slice`.
- **H3c**: same via `memfd_create(MFD_HUGETLB)`.

H3b/H3c failed with errno=ENOMEM because `HugePages_Free=0`.
160 × 2 MiB = 320 MiB covers the ~130 MiB compressed fixture (aligned to 2 MiB).

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
CARGO_TARGET_DIR=/work/cargo-target-ralph cargo bench --bench decompress
```

The `h3b_hugepage_anon` and `h3c_hugepage_memfd` criterion groups should appear.
Reply in `.escalations/cycle03-hugepages-setup.reply.md` with bench results.
"#;
    let _ = std::fs::write(path, content);
    eprintln!("Escalation written to {path}");
}
