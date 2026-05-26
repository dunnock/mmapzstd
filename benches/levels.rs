#![allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss
)]

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use rand::RngCore;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

const FIXTURE_DIR: &str = "/work/cargo-target-ralph/mmapzstd-fixtures";
const DECOMPRESSED_SIZE: u64 = 256 * 1024 * 1024;
const GIB_DECOMPRESSED_SIZE: u64 = 2 * 1024 * 1024 * 1024;

#[cfg(target_os = "linux")]
const HUGEPAGE: usize = 2 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAP_HUGE_2MB: libc::c_int = 21 << 26;

// ------ proc helpers ----------------------------------------------------------

struct ProcSnapshot {
    minflt: u64,
    vmrss_kib: u64,
}

impl ProcSnapshot {
    fn now() -> Self {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
        let minflt = stat
            .split_whitespace()
            .nth(9)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let vmrss_kib = status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Self { minflt, vmrss_kib }
    }

    fn delta(&self, earlier: &Self) -> (i64, i64) {
        (
            self.minflt as i64 - earlier.minflt as i64,
            self.vmrss_kib as i64 - earlier.vmrss_kib as i64,
        )
    }
}

/// Read smaps fields (THPeligible, FilePmdMapped, AnonHugePages) for the VMA at `addr`.
fn smaps_at(addr: usize) -> String {
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let mut in_vma = false;
    let mut parts = Vec::new();
    for line in smaps.lines() {
        // VMA header lines start with a hex address digit
        let is_header = line
            .chars()
            .next()
            .map(|c| c.is_ascii_hexdigit())
            .unwrap_or(false);
        if is_header {
            if let Some(dash) = line.find('-') {
                let tail = &line[dash + 1..];
                let end_hex = tail.split_whitespace().next().unwrap_or("0");
                if let (Ok(s), Ok(e)) = (
                    usize::from_str_radix(&line[..dash], 16),
                    usize::from_str_radix(end_hex, 16),
                ) {
                    in_vma = addr >= s && addr < e;
                }
            }
            continue;
        }
        if in_vma
            && (line.starts_with("AnonHugePages:")
                || line.starts_with("FilePmdMapped:")
                || line.starts_with("THPeligible:"))
        {
            parts.push(line.trim().to_string());
        }
    }
    if parts.is_empty() {
        "(no hugepage smaps fields)".to_string()
    } else {
        parts.join("; ")
    }
}

/// Read smaps hugepage fields for the file-backed VMA containing `path`.
fn smaps_for_file(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let mut in_vma = false;
    let mut parts = Vec::new();
    for line in smaps.lines() {
        let is_header = line
            .chars()
            .next()
            .map(|c| c.is_ascii_hexdigit())
            .unwrap_or(false);
        if is_header {
            in_vma = line.contains(path_str.as_ref());
            continue;
        }
        if in_vma
            && (line.starts_with("AnonHugePages:")
                || line.starts_with("FilePmdMapped:")
                || line.starts_with("THPeligible:"))
        {
            parts.push(line.trim().to_string());
        }
    }
    if parts.is_empty() {
        format!("(no file mapping found for {})", path.display())
    } else {
        parts.join("; ")
    }
}

// ------ fixture ---------------------------------------------------------------

fn generate_corpus() -> Vec<u8> {
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
    data
}

fn ensure_fixtures() -> (PathBuf, PathBuf) {
    std::fs::create_dir_all(FIXTURE_DIR).expect("create fixtures dir");
    let l3 = PathBuf::from(FIXTURE_DIR).join("levels_256mib_l3.zst");
    let l9 = PathBuf::from(FIXTURE_DIR).join("levels_256mib_l9.zst");
    if !l3.exists() || !l9.exists() {
        eprintln!("Generating 256 MiB corpus (once)...");
        let corpus = generate_corpus();
        if !l3.exists() {
            let c = zstd::encode_all(corpus.as_slice(), 3).expect("encode l3");
            std::fs::write(&l3, &c).expect("write l3");
            eprintln!(
                "  L3: {} bytes ({:.1} MiB)",
                c.len(),
                c.len() as f64 / 1048576.0
            );
        }
        if !l9.exists() {
            let c = zstd::encode_all(corpus.as_slice(), 9).expect("encode l9");
            std::fs::write(&l9, &c).expect("write l9");
            eprintln!(
                "  L9: {} bytes ({:.1} MiB)",
                c.len(),
                c.len() as f64 / 1048576.0
            );
        }
    }
    (l3, l9)
}

fn ensure_1gib_fixture() -> PathBuf {
    std::fs::create_dir_all(FIXTURE_DIR).expect("create fixtures dir");
    let path = PathBuf::from(FIXTURE_DIR).join("levels_1gib_l3.zst");
    if path.exists() {
        let sz = path.metadata().expect("stat 1gib").len();
        eprintln!(
            "1GiB fixture: {} bytes ({:.1} MiB compressed)",
            sz,
            sz as f64 / 1_048_576.0
        );
        return path;
    }
    eprintln!("Generating ≥1 GiB corpus (once — 2 GiB decompressed)...");
    const BLOCK: usize = 4096;
    const TOTAL: usize = 2 * 1024 * 1024 * 1024;
    const CHUNK: usize = 4 * 1024 * 1024;

    let mut rng = rand::thread_rng();
    let file = std::fs::File::create(&path).expect("create 1gib fixture");
    let mut encoder = zstd::stream::Encoder::new(file, 3).expect("zstd encoder");

    let mut buf = vec![0u8; CHUNK];
    let mut generated = 0usize;
    while generated < TOTAL {
        let chunk_len = CHUNK.min(TOTAL - generated);
        let mut offset = 0;
        while offset < chunk_len {
            let end = (offset + BLOCK).min(chunk_len);
            rng.fill_bytes(&mut buf[offset..end]);
            offset = end;
            let end = (offset + BLOCK).min(chunk_len);
            for b in &mut buf[offset..end] {
                *b = 0xAB;
            }
            offset = end;
        }
        encoder.write_all(&buf[..chunk_len]).expect("encode chunk");
        generated += chunk_len;
        if generated % (256 * 1024 * 1024) == 0 {
            eprintln!("  ...{}/{} MiB", generated >> 20, TOTAL >> 20);
        }
    }
    encoder.finish().expect("finish encoder");

    let sz = path.metadata().expect("stat 1gib").len();
    eprintln!(
        "  1GiB corpus: {} bytes ({:.1} MiB compressed, from {} MiB decompressed)",
        sz,
        sz as f64 / 1_048_576.0,
        TOTAL >> 20
    );
    path
}

/// Sum Private_Hugetlb and AnonHugePages across all VMAs (KiB).
fn smaps_hugepage_totals() -> (u64, u64) {
    let smaps = std::fs::read_to_string("/proc/self/smaps").unwrap_or_default();
    let mut private_hugetlb = 0u64;
    let mut anon_huge = 0u64;
    for line in smaps.lines() {
        if let Some(rest) = line.strip_prefix("Private_Hugetlb:") {
            private_hugetlb += rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("AnonHugePages:") {
            anon_huge += rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    (private_hugetlb, anon_huge)
}

// ------ Linux hugepage helpers ------------------------------------------------

#[cfg(target_os = "linux")]
fn alloc_hugepage_anon(data_len: usize) -> Option<(*mut libc::c_void, usize)> {
    let aligned = (data_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);
    // SAFETY: mmap syscall; return value checked before use.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            aligned,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB | MAP_HUGE_2MB,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        None
    } else {
        Some((ptr, aligned))
    }
}

#[cfg(target_os = "linux")]
fn alloc_memfd_hugepage(data_len: usize) -> Option<(*mut libc::c_void, usize)> {
    let mfd_hugetlb: libc::c_ulong = 0x0004;
    let mfd_huge_2mb: libc::c_ulong = 21 << 26;
    let aligned = (data_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);
    // SAFETY: syscall with checked return.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            b"levels-bench\0".as_ptr(),
            mfd_hugetlb | mfd_huge_2mb,
        )
    };
    if fd < 0 {
        return None;
    }
    let fd = fd as libc::c_int;
    if unsafe { libc::ftruncate(fd, aligned as libc::off_t) } != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    // SAFETY: mmap the hugetlbfs fd.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            aligned,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe { libc::close(fd) };
    if ptr == libc::MAP_FAILED {
        None
    } else {
        Some((ptr, aligned))
    }
}

#[cfg(target_os = "linux")]
fn fill_hugepage(ptr: *mut libc::c_void, len: usize, path: &Path) {
    // SAFETY: ptr points to an allocated hugepage region of at least `len` bytes.
    let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
    File::open(path)
        .expect("open fixture")
        .read_exact(buf)
        .expect("fill hugepage region");
}

// ------ one-shot proc stats ---------------------------------------------------

fn print_one_shot_stats(l3: &Path, l9: &Path) {
    eprintln!("\n=== one-shot proc stats (warm cache) ===");

    for (lname, path) in [("l3", l3), ("l9", l9)] {
        // bufreader-64k
        {
            let s0 = ProcSnapshot::now();
            let f = File::open(path).expect("open");
            let mut dec =
                zstd::stream::Decoder::new(BufReader::with_capacity(65536, f)).expect("dec");
            io::copy(&mut dec, &mut io::sink()).expect("copy");
            let s1 = ProcSnapshot::now();
            let (mf, rss) = s1.delta(&s0);
            eprintln!("bufreader-64k/{lname}:  minflt={mf:+}  vmrss_delta={rss:+} KiB");
        }

        // mmap (file-backed)
        {
            let s0 = ProcSnapshot::now();
            let file = File::open(path).expect("open file");
            let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
            let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
            // Read smaps while the mmap is still live (before io::copy drops pages)
            let sm = smaps_for_file(path);
            io::copy(&mut dec, &mut io::sink()).expect("copy");
            let s1 = ProcSnapshot::now();
            let (mf, rss) = s1.delta(&s0);
            eprintln!("mmap/{lname}:  minflt={mf:+}  vmrss_delta={rss:+} KiB");
            eprintln!("  smaps[mmap/{lname}]: {sm}");
        }

        // hugepage-anon
        #[cfg(target_os = "linux")]
        {
            let dlen = path.metadata().expect("stat").len() as usize;
            match alloc_hugepage_anon(dlen) {
                None => eprintln!("hugepage-anon/{lname}:  SKIPPED (MAP_HUGETLB failed)"),
                Some((ptr, aligned)) => {
                    fill_hugepage(ptr, dlen, path);
                    // SAFETY: ptr valid, dlen ≤ aligned allocation.
                    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, dlen) };
                    let addr = ptr as usize;
                    let s0 = ProcSnapshot::now();
                    let mut dec =
                        mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                    let s1 = ProcSnapshot::now();
                    let (mf, rss) = s1.delta(&s0);
                    eprintln!("hugepage-anon/{lname}:  minflt={mf:+}  vmrss_delta={rss:+} KiB");
                    eprintln!("  smaps[hugepage-anon/{lname}]: {}", smaps_at(addr));
                    // SAFETY: munmap the hugepage region.
                    unsafe { libc::munmap(ptr, aligned) };
                }
            }
        }

        // hugepage-memfd
        #[cfg(target_os = "linux")]
        {
            let dlen = path.metadata().expect("stat").len() as usize;
            match alloc_memfd_hugepage(dlen) {
                None => eprintln!("hugepage-memfd/{lname}: SKIPPED (memfd_create failed)"),
                Some((ptr, aligned)) => {
                    fill_hugepage(ptr, dlen, path);
                    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, dlen) };
                    let addr = ptr as usize;
                    let s0 = ProcSnapshot::now();
                    let mut dec =
                        mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                    let s1 = ProcSnapshot::now();
                    let (mf, rss) = s1.delta(&s0);
                    eprintln!("hugepage-memfd/{lname}: minflt={mf:+}  vmrss_delta={rss:+} KiB");
                    eprintln!("  smaps[hugepage-memfd/{lname}]: {}", smaps_at(addr));
                    unsafe { libc::munmap(ptr, aligned) };
                }
            }
        }

        // hugepage-streaming-8m
        #[cfg(target_os = "linux")]
        {
            const WINDOW: usize = 8 * 1024 * 1024;
            let (hp_priv_before, hp_anon_before) = smaps_hugepage_totals();
            let s0 = ProcSnapshot::now();
            match mmapzstd::decoder::Decoder::open_hugepage_streaming(path, WINDOW) {
                Err(e) => {
                    eprintln!("hugepage-streaming-8m/{lname}: SKIPPED ({e})");
                }
                Ok(mut dec) => {
                    let (hp_priv_open, hp_anon_open) = smaps_hugepage_totals();
                    let scratch_kib = (hp_priv_open + hp_anon_open)
                        .saturating_sub(hp_priv_before + hp_anon_before);
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                    let s1 = ProcSnapshot::now();
                    let (mf, rss) = s1.delta(&s0);
                    eprintln!(
                        "hugepage-streaming-8m/{lname}: minflt={mf:+}  vmrss_delta={rss:+} KiB  \
                         scratch_hugepages={scratch_kib} KiB"
                    );
                }
            }
        }
    }
    eprintln!();
}

fn print_1gib_stats(gib: &Path) {
    eprintln!("\n=== 1 GiB one-shot proc stats (warm cache) ===");

    // bufreader-64k
    {
        let s0 = ProcSnapshot::now();
        let f = File::open(gib).expect("open");
        let mut dec =
            zstd::stream::Decoder::new(BufReader::with_capacity(65536, f)).expect("dec");
        io::copy(&mut dec, &mut io::sink()).expect("copy");
        let s1 = ProcSnapshot::now();
        let (mf, rss) = s1.delta(&s0);
        eprintln!("bufreader-64k/1gib: minflt={mf:+}  vmrss_delta={rss:+} KiB");
    }

    // mmap
    {
        let s0 = ProcSnapshot::now();
        let file = File::open(gib).expect("open");
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
        let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
        io::copy(&mut dec, &mut io::sink()).expect("copy");
        let s1 = ProcSnapshot::now();
        let (mf, rss) = s1.delta(&s0);
        eprintln!("mmap/1gib: minflt={mf:+}  vmrss_delta={rss:+} KiB");
    }

    // hugepage-streaming-8m
    #[cfg(target_os = "linux")]
    {
        const WINDOW: usize = 8 * 1024 * 1024;
        let (hp_priv_before, hp_anon_before) = smaps_hugepage_totals();
        let s0 = ProcSnapshot::now();
        match mmapzstd::decoder::Decoder::open_hugepage_streaming(gib, WINDOW) {
            Err(e) => eprintln!("hugepage-streaming-8m/1gib: SKIPPED ({e})"),
            Ok(mut dec) => {
                let (hp_priv_open, hp_anon_open) = smaps_hugepage_totals();
                let scratch_kib = (hp_priv_open + hp_anon_open)
                    .saturating_sub(hp_priv_before + hp_anon_before);
                io::copy(&mut dec, &mut io::sink()).expect("copy");
                let s1 = ProcSnapshot::now();
                let (mf, rss) = s1.delta(&s0);
                eprintln!(
                    "hugepage-streaming-8m/1gib: minflt={mf:+}  vmrss_delta={rss:+} KiB  \
                     scratch_hugepages={scratch_kib} KiB"
                );
            }
        }
    }
    eprintln!();
}

// ------ benchmark entry -------------------------------------------------------

fn bench_all(c: &mut Criterion) {
    let (l3, l9) = ensure_fixtures();
    let gib = ensure_1gib_fixture();

    let l3_size = l3.metadata().expect("stat l3").len();
    let l9_size = l9.metadata().expect("stat l9").len();
    let gib_size = gib.metadata().expect("stat gib").len();
    eprintln!("\n=== fixture sizes ===");
    eprintln!(
        "L3: {} bytes = {:.1} MiB  (ratio {:.2}:1)",
        l3_size,
        l3_size as f64 / 1_048_576.0,
        268_435_456.0 / l3_size as f64
    );
    eprintln!(
        "L9: {} bytes = {:.1} MiB  (ratio {:.2}:1)",
        l9_size,
        l9_size as f64 / 1_048_576.0,
        268_435_456.0 / l9_size as f64
    );
    eprintln!(
        "1GiB: {} bytes = {:.1} MiB compressed  (decompressed = 2048 MiB, ratio {:.2}:1)",
        gib_size,
        gib_size as f64 / 1_048_576.0,
        GIB_DECOMPRESSED_SIZE as f64 / gib_size as f64
    );

    // Warm page cache for 256 MiB fixtures before any measurements
    for path in [l3.as_path(), l9.as_path()] {
        let file = File::open(path).expect("warm mmap open");
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("warm mmap") };
        let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("warm mmap dec");
        io::copy(&mut dec, &mut io::sink()).expect("warm mmap copy");
        let f = File::open(path).expect("warm br open");
        let mut dec =
            zstd::stream::Decoder::new(BufReader::with_capacity(65536, f)).expect("warm br dec");
        io::copy(&mut dec, &mut io::sink()).expect("warm br copy");
    }

    // Warm page cache for 1 GiB fixture
    {
        let file = File::open(&gib).expect("warm gib mmap open");
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("warm gib mmap") };
        let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("warm gib dec");
        io::copy(&mut dec, &mut io::sink()).expect("warm gib mmap copy");
        let f = File::open(&gib).expect("warm gib br open");
        let mut dec =
            zstd::stream::Decoder::new(BufReader::with_capacity(65536, f)).expect("warm gib br dec");
        io::copy(&mut dec, &mut io::sink()).expect("warm gib br copy");
    }

    print_one_shot_stats(&l3, &l9);
    print_1gib_stats(&gib);

    // ---- bufreader-64k -------------------------------------------------------
    {
        let mut group = c.benchmark_group("bufreader-64k");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(20));
        group.throughput(Throughput::Bytes(DECOMPRESSED_SIZE));
        for (name, path) in [("level3", l3.as_path()), ("level9", l9.as_path())] {
            group.bench_with_input(BenchmarkId::from_parameter(name), path, |b, p| {
                b.iter(|| {
                    let f = File::open(p).expect("open");
                    let mut dec = zstd::stream::Decoder::new(BufReader::with_capacity(65536, f))
                        .expect("dec");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        group.finish();
    }

    // ---- mmap-populate -------------------------------------------------------
    {
        let mut group = c.benchmark_group("mmap");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(20));
        group.throughput(Throughput::Bytes(DECOMPRESSED_SIZE));
        for (name, path) in [("level3", l3.as_path()), ("level9", l9.as_path())] {
            group.bench_with_input(BenchmarkId::from_parameter(name), path, |b, p| {
                b.iter(|| {
                    let file = File::open(p).expect("open");
                    let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
                    let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        group.finish();
    }

    // ---- hugepage-anon -------------------------------------------------------
    #[cfg(target_os = "linux")]
    {
        let l3_len = l3.metadata().expect("stat l3").len() as usize;
        let l9_len = l9.metadata().expect("stat l9").len() as usize;
        let l3_hp = alloc_hugepage_anon(l3_len);
        let l9_hp = alloc_hugepage_anon(l9_len);
        if let Some((ptr, _)) = l3_hp {
            fill_hugepage(ptr, l3_len, &l3);
        }
        if let Some((ptr, _)) = l9_hp {
            fill_hugepage(ptr, l9_len, &l9);
        }

        let mut group = c.benchmark_group("hugepage-anon");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(20));
        group.throughput(Throughput::Bytes(DECOMPRESSED_SIZE));

        if let Some((ptr, _)) = l3_hp {
            // SAFETY: ptr valid for l3_len bytes, lives until munmap below.
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, l3_len) };
            group.bench_with_input(BenchmarkId::from_parameter("level3"), slice, |b, s| {
                b.iter(|| {
                    let mut dec = mmapzstd::decoder::Decoder::from_slice(s).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        if let Some((ptr, _)) = l9_hp {
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, l9_len) };
            group.bench_with_input(BenchmarkId::from_parameter("level9"), slice, |b, s| {
                b.iter(|| {
                    let mut dec = mmapzstd::decoder::Decoder::from_slice(s).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        group.finish();

        if let Some((ptr, aligned)) = l3_hp {
            unsafe { libc::munmap(ptr, aligned) };
        }
        if let Some((ptr, aligned)) = l9_hp {
            unsafe { libc::munmap(ptr, aligned) };
        }
    }

    // ---- hugepage-memfd ------------------------------------------------------
    #[cfg(target_os = "linux")]
    {
        let l3_len = l3.metadata().expect("stat l3").len() as usize;
        let l9_len = l9.metadata().expect("stat l9").len() as usize;
        let l3_mf = alloc_memfd_hugepage(l3_len);
        let l9_mf = alloc_memfd_hugepage(l9_len);
        if let Some((ptr, _)) = l3_mf {
            fill_hugepage(ptr, l3_len, &l3);
        }
        if let Some((ptr, _)) = l9_mf {
            fill_hugepage(ptr, l9_len, &l9);
        }

        let mut group = c.benchmark_group("hugepage-memfd");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(20));
        group.throughput(Throughput::Bytes(DECOMPRESSED_SIZE));

        if let Some((ptr, _)) = l3_mf {
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, l3_len) };
            group.bench_with_input(BenchmarkId::from_parameter("level3"), slice, |b, s| {
                b.iter(|| {
                    let mut dec = mmapzstd::decoder::Decoder::from_slice(s).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        if let Some((ptr, _)) = l9_mf {
            let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, l9_len) };
            group.bench_with_input(BenchmarkId::from_parameter("level9"), slice, |b, s| {
                b.iter(|| {
                    let mut dec = mmapzstd::decoder::Decoder::from_slice(s).expect("from_slice");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        group.finish();

        if let Some((ptr, aligned)) = l3_mf {
            unsafe { libc::munmap(ptr, aligned) };
        }
        if let Some((ptr, aligned)) = l9_mf {
            unsafe { libc::munmap(ptr, aligned) };
        }
    }

    // ---- hugepage-streaming-8m (256 MiB) -------------------------------------
    #[cfg(target_os = "linux")]
    {
        const STREAMING_WINDOW: usize = 8 * 1024 * 1024;
        let mut group = c.benchmark_group("hugepage-streaming-8m");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(20));
        group.throughput(Throughput::Bytes(DECOMPRESSED_SIZE));
        for (name, path) in [("level3", l3.as_path()), ("level9", l9.as_path())] {
            group.bench_with_input(BenchmarkId::from_parameter(name), path, |b, p| {
                b.iter(|| {
                    let mut dec =
                        mmapzstd::decoder::Decoder::open_hugepage_streaming(p, STREAMING_WINDOW)
                            .expect("open_hugepage_streaming");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            });
        }
        group.finish();
    }

    // ---- 1 GiB corpus benchmarks ---------------------------------------------

    // bufreader-64k-1gib
    {
        let mut group = c.benchmark_group("bufreader-64k-1gib");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(30));
        group.throughput(Throughput::Bytes(GIB_DECOMPRESSED_SIZE));
        group.bench_with_input(
            BenchmarkId::from_parameter("level3"),
            gib.as_path(),
            |b, p| {
                b.iter(|| {
                    let f = File::open(p).expect("open");
                    let mut dec =
                        zstd::stream::Decoder::new(BufReader::with_capacity(65536, f))
                            .expect("dec");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            },
        );
        group.finish();
    }

    // mmap-1gib
    {
        let mut group = c.benchmark_group("mmap-1gib");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(30));
        group.throughput(Throughput::Bytes(GIB_DECOMPRESSED_SIZE));
        group.bench_with_input(
            BenchmarkId::from_parameter("level3"),
            gib.as_path(),
            |b, p| {
                b.iter(|| {
                    let file = File::open(p).expect("open");
                    let mmap =
                        unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
                    let mut dec =
                        mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            },
        );
        group.finish();
    }

    // hugepage-streaming-8m-1gib
    #[cfg(target_os = "linux")]
    {
        const STREAMING_WINDOW: usize = 8 * 1024 * 1024;
        let mut group = c.benchmark_group("hugepage-streaming-8m-1gib");
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(30));
        group.throughput(Throughput::Bytes(GIB_DECOMPRESSED_SIZE));
        group.bench_with_input(
            BenchmarkId::from_parameter("level3"),
            gib.as_path(),
            |b, p| {
                b.iter(|| {
                    let mut dec =
                        mmapzstd::decoder::Decoder::open_hugepage_streaming(p, STREAMING_WINDOW)
                            .expect("open_hugepage_streaming");
                    io::copy(&mut dec, &mut io::sink()).expect("copy");
                });
            },
        );
        group.finish();
    }
}

criterion_group!(benches, bench_all);
criterion_main!(benches);
