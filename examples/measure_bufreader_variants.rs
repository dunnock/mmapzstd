/// Benchmark BufReader optimisation hypotheses for zstd decompression.
///
/// H1: vary BufReader capacity (64 KiB / 256 KiB / 1 MiB / 4 MiB)
/// H2: posix_fadvise(SEQUENTIAL) + posix_fadvise(WILLNEED) before decode
/// H3: splice() — analysed but not implemented (two syscalls + user-copy = worse)
/// H4: raw File (no BufReader) passed directly to zstd decoder
use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::Path;
use std::time::Duration;

use rand::RngCore;
use tempfile::NamedTempFile;

fn read_proc_stat() -> (u64, u64) {
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let minflt: u64 = fields.get(9).and_then(|s| s.parse().ok()).unwrap_or(0);
    let majflt: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    (minflt, majflt)
}

fn make_fixture() -> NamedTempFile {
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

fn drop_caches() {
    // Best-effort: requires root. Silently ignored if unavailable.
    let _ = fs::write("/proc/sys/vm/drop_caches", "1");
}

fn measure_once(path: &Path, variant_fn: &dyn Fn(&Path) -> Box<dyn io::Read>) -> (Duration, u64) {
    let (minflt_before, _) = read_proc_stat();
    let start = std::time::Instant::now();
    let mut reader = variant_fn(path);
    io::copy(&mut reader, &mut io::sink()).expect("io::copy");
    let elapsed = start.elapsed();
    let (minflt_after, _) = read_proc_stat();
    (elapsed, minflt_after - minflt_before)
}

fn bench(label: &str, path: &Path, variant_fn: &dyn Fn(&Path) -> Box<dyn io::Read>) {
    const TOTAL_RUNS: usize = 5;
    const WARMUP: usize = 2;

    let mut all: Vec<(Duration, u64)> = Vec::with_capacity(TOTAL_RUNS);
    for _ in 0..TOTAL_RUNS {
        all.push(measure_once(path, variant_fn));
    }

    let measured = &all[WARMUP..];
    let n = measured.len() as f64;
    let avg_ms =
        measured.iter().map(|(d, _)| d.as_secs_f64()).sum::<f64>() * 1000.0 / n;
    let avg_faults = measured.iter().map(|(_, f)| *f).sum::<u64>() / measured.len() as u64;
    let bytes = 256u64 * 1024 * 1024;
    let throughput_mbs = bytes as f64 / 1_048_576.0 / (avg_ms / 1000.0);

    println!(
        "{:<50}  {:>8.2} ms  {:>9.1} MB/s  {:>6} faults",
        label, avg_ms, throughput_mbs, avg_faults
    );
}

// H1 — BufReader with capacity `cap`
fn bufreader_cap(cap: usize) -> impl Fn(&Path) -> Box<dyn io::Read> {
    move |path| {
        let file = File::open(path).expect("open");
        let buf = BufReader::with_capacity(cap, file);
        let dec = zstd::stream::Decoder::new(buf).expect("decoder");
        Box::new(dec)
    }
}

// H2 — 64 KiB BufReader + posix_fadvise(SEQUENTIAL) + posix_fadvise(WILLNEED)
#[cfg(unix)]
fn bufreader_fadvise(path: &Path) -> Box<dyn io::Read> {
    use std::os::unix::io::AsRawFd;
    let file = File::open(path).expect("open");
    unsafe {
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
        libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED);
    }
    let buf = BufReader::with_capacity(65_536, file);
    let dec = zstd::stream::Decoder::new(buf).expect("decoder");
    Box::new(dec)
}

// H4 — raw File passed directly (no BufReader); zstd makes frequent small reads
fn raw_file(path: &Path) -> Box<dyn io::Read> {
    let file = File::open(path).expect("open");
    let dec = zstd::stream::Decoder::new(file).expect("decoder");
    Box::new(dec)
}

fn main() {
    eprintln!("Generating 256 MiB fixture (zstd level 3)...");
    let fixture = make_fixture();
    let path = fixture.path();

    // Warm the page cache for at least one pass before measuring.
    drop_caches();
    measure_once(path, &bufreader_cap(65_536));

    println!();
    println!(
        "{:<50}  {:>10}  {:>11}  {:>12}",
        "variant", "avg ms", "throughput", "min-faults"
    );
    println!("{}", "-".repeat(90));

    // H1 — buffer size sweep
    bench("H1a BufReader  64 KiB (baseline)", path, &bufreader_cap(64 * 1024));
    bench("H1b BufReader 256 KiB", path, &bufreader_cap(256 * 1024));
    bench("H1c BufReader   1 MiB", path, &bufreader_cap(1024 * 1024));
    bench("H1d BufReader   4 MiB", path, &bufreader_cap(4 * 1024 * 1024));

    // H2 — posix_fadvise
    #[cfg(unix)]
    bench("H2  BufReader 64 KiB + fadvise(SEQ+WILLNEED)", path, &bufreader_fadvise);

    // H4 — raw File (no BufReader)
    bench("H4  raw File (no BufReader)", path, &raw_file);

    println!();
    println!("Note: H3 (splice) not implemented — splice -> pipe -> read requires two");
    println!("      syscalls per chunk vs one for BufReader and still copies data into");
    println!("      user space before zstd can process it. Analysis: no benefit expected.");
}
