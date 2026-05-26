use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use rand::RngCore;

const FIXTURE_DIR: &str = "/work/cargo-target-ralph/mmapzstd-fixtures";
const FIXTURE_NAME: &str = "decompress_256mib.zst";

fn fixture_path() -> PathBuf {
    PathBuf::from(FIXTURE_DIR).join(FIXTURE_NAME)
}

fn ensure_fixture(path: &Path) {
    if path.exists() {
        return;
    }
    std::fs::create_dir_all(FIXTURE_DIR).expect("create fixtures dir");

    const BLOCK: usize = 4096;
    const TOTAL: usize = 256 * 1024 * 1024;

    let mut data = vec![0u8; TOTAL];
    let mut rng = rand::thread_rng();
    let mut offset = 0;
    while offset < TOTAL {
        // incompressible block
        let end = (offset + BLOCK).min(TOTAL);
        rng.fill_bytes(&mut data[offset..end]);
        offset = end;
        // compressible block
        let end = (offset + BLOCK).min(TOTAL);
        for b in &mut data[offset..end] {
            *b = 0xAB;
        }
        offset = end;
    }

    let compressed = zstd::encode_all(data.as_slice(), 3).expect("encode fixture");
    let mut f = File::create(path).expect("create fixture");
    f.write_all(&compressed).expect("write fixture");
}

fn bench_decompress(c: &mut Criterion) {
    let path = fixture_path();
    ensure_fixture(&path);

    // H3a: mmapzstd decoder with fixture on ext4 (/work bind-mount), THP eligible.
    // MADV_SEQUENTIAL + MADV_HUGEPAGE are applied in from_mmap via apply_madvise().
    let mut group = c.benchmark_group("mmapzstd");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("decompress", |b| {
        b.iter(|| {
            let file = File::open(&path).expect("open");
            let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
            let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
            io::copy(&mut dec, &mut io::sink()).expect("copy");
        });
    });
    group.finish();

    let mut group = c.benchmark_group("baseline");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("decompress", |b| {
        b.iter(|| {
            let file = File::open(&path).expect("open");
            let buf = BufReader::with_capacity(65536, file);
            let mut dec = zstd::stream::Decoder::new(buf).expect("decoder");
            io::copy(&mut dec, &mut io::sink()).expect("copy");
        });
    });
    group.finish();

    // H3b: MAP_ANON | MAP_HUGETLB | MAP_HUGE_2MB — explicit 2 MiB pages.
    // Skipped if HugePages_Free == 0.
    #[cfg(target_os = "linux")]
    bench_h3b(c, &path);

    // H3c: memfd_create(MFD_HUGETLB) — hugetlbfs-backed fd, same fallback as H3b.
    #[cfg(target_os = "linux")]
    bench_h3c(c, &path);
}

/// H3b: load compressed data into an anonymous MAP_HUGETLB region, decode from slice.
#[cfg(target_os = "linux")]
fn bench_h3b(c: &mut Criterion, path: &Path) {
    use std::ptr;

    let compressed_len = path.metadata().expect("stat fixture").len() as usize;

    // 2 MiB alignment
    const HUGEPAGE: usize = 2 * 1024 * 1024;
    let aligned_len = (compressed_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);

    // MAP_HUGE_2MB = 21 << MAP_HUGE_SHIFT (26) = 21 << 26
    let map_huge_2mb: libc::c_int = 21 << 26;

    // SAFETY: raw mmap syscall; return value checked before use.
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            aligned_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_HUGETLB | map_huge_2mb,
            -1,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        let errno = unsafe { *libc::__errno_location() };
        eprintln!(
            "H3b: Skipped — MAP_HUGETLB mmap failed (errno={errno}). \
             HugePages_Free=0; run `sudo sysctl vm.nr_hugepages=160` to enable."
        );
        emit_hugepage_escalation();
        return;
    }

    // Fill the hugepage region with the compressed file content.
    // Scope the mutable borrow so it is dropped before creating the shared slice below.
    {
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, compressed_len) };
        File::open(path)
            .expect("open fixture")
            .read_exact(buf)
            .expect("read fixture into hugepage buf");
    }
    let slice: &[u8] = unsafe { std::slice::from_raw_parts(ptr as *const u8, compressed_len) };

    let mut group = c.benchmark_group("h3b_hugepage_anon");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("decompress", |b| {
        b.iter(|| {
            let mut dec = mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
            io::copy(&mut dec, &mut io::sink()).expect("copy");
        });
    });
    group.finish();

    // SAFETY: munmap the hugepage allocation.
    unsafe { libc::munmap(ptr, aligned_len) };
}

/// H3c: memfd_create(MFD_HUGETLB) — hugetlbfs file descriptor backed by huge pages.
#[cfg(target_os = "linux")]
fn bench_h3c(c: &mut Criterion, path: &Path) {
    use std::ptr;

    // MFD_HUGETLB = 0x0004, MFD_HUGE_SHIFT = 26, MFD_HUGE_2MB = 21 << 26
    let mfd_hugetlb: libc::c_ulong = 0x0004;
    let mfd_huge_2mb: libc::c_ulong = 21 << 26;
    let flags = mfd_hugetlb | mfd_huge_2mb;

    // SAFETY: syscall with checked return.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            b"mmapzstd-hugetlb\0".as_ptr(),
            flags,
        )
    };

    if fd < 0 {
        let errno = unsafe { *libc::__errno_location() };
        eprintln!(
            "H3c: Skipped — memfd_create(MFD_HUGETLB) failed (errno={errno}). \
             HugePages_Free=0; run `sudo sysctl vm.nr_hugepages=160` to enable."
        );
        // Escalation already emitted by H3b; no duplicate needed.
        return;
    }

    let compressed_len = path.metadata().expect("stat fixture").len() as usize;
    const HUGEPAGE: usize = 2 * 1024 * 1024;
    let aligned_len = (compressed_len + HUGEPAGE - 1) & !(HUGEPAGE - 1);

    // SAFETY: ftruncate to reserve space in the hugetlbfs fd.
    let rc = unsafe { libc::ftruncate(fd as libc::c_int, aligned_len as libc::off_t) };
    if rc != 0 {
        eprintln!("H3c: Skipped — ftruncate failed (errno={})", unsafe {
            *libc::__errno_location()
        });
        unsafe { libc::close(fd as libc::c_int) };
        return;
    }

    // SAFETY: mmap the memfd normally (no MAP_HUGETLB needed; the fd is already hugetlbfs).
    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
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
        eprintln!("H3c: Skipped — mmap of memfd failed (errno={errno})");
        return;
    }

    {
        let wbuf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, compressed_len) };
        File::open(path)
            .expect("open fixture")
            .read_exact(wbuf)
            .expect("read fixture into memfd hugepage buf");
    }
    let slice: &[u8] = unsafe { std::slice::from_raw_parts(ptr as *const u8, compressed_len) };

    let mut group = c.benchmark_group("h3c_hugepage_memfd");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("decompress", |b| {
        b.iter(|| {
            let mut dec = mmapzstd::decoder::Decoder::from_slice(slice).expect("from_slice");
            io::copy(&mut dec, &mut io::sink()).expect("copy");
        });
    });
    group.finish();

    unsafe { libc::munmap(ptr, aligned_len) };
}

/// Emit the hugepage operator escalation file (idempotent).
#[cfg(target_os = "linux")]
fn emit_hugepage_escalation() {
    let path = "/work/ralph-self-improvement/workspace/.escalations/cycle03-hugepages-setup.md";
    if std::path::Path::new(path).exists() {
        return;
    }
    let content = r#"# Escalation: Reserve Static Huge Pages for H3b/H3c (cycle-03)

## What I need

`vm.nr_hugepages` set to at least 160 so that `MAP_HUGETLB | MAP_HUGE_2MB` and
`memfd_create(MFD_HUGETLB)` succeed.

## Context

Task `hugepage-mmap` (cycle-03) is testing H3b and H3c: allocating the compressed
input (~130 MiB) into 2 MiB static huge pages so the TLB needs only ~65 entries
instead of ~33,000 for 4 KiB pages. H3a (ext4 fixture + MADV_HUGEPAGE) is running
and will report results.

H3b/H3c require pre-allocated huge pages (`HugePages_Free > 0`). Current state:
- `HugePages_Total: 0` / `HugePages_Free: 0`
- 160 × 2 MiB = 320 MiB needed (compressed fixture ~130 MiB, aligned to 2 MiB boundary)

## Exact command

```bash
sudo sysctl vm.nr_hugepages=160
```

To revert after testing:

```bash
sudo sysctl vm.nr_hugepages=0
```

## Expected outcome

After the operator applies the command, re-run from
`/work/mmapzstd/.worktrees/03-hugepages`:

```bash
CARGO_TARGET_DIR=/work/cargo-target-ralph cargo bench --bench decompress
```

H3b and H3c groups should appear in criterion output. Reply in
`.escalations/cycle03-hugepages-setup.reply.md` with the bench results.
"#;
    let _ = std::fs::write(path, content);
}

criterion_group!(benches, bench_decompress);
criterion_main!(benches);
