use std::io::Write;
use std::process::Command;
use tempfile::NamedTempFile;

const CORPUS_SIZE: usize = 4 * 1024 * 1024;

fn hugepages_free() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("HugePages_Free:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn make_fixture() -> NamedTempFile {
    use rand::RngCore;
    const BLOCK: usize = 4096;

    let mut data = vec![0u8; CORPUS_SIZE];
    let mut rng = rand::thread_rng();
    let mut offset = 0;
    while offset < CORPUS_SIZE {
        let end = (offset + BLOCK).min(CORPUS_SIZE);
        rng.fill_bytes(&mut data[offset..end]);
        offset = end;
        let end = (offset + BLOCK).min(CORPUS_SIZE);
        for b in &mut data[offset..end] {
            *b = 0xAB;
        }
        offset = end;
    }

    let compressed = zstd::encode_all(data.as_slice(), 3).expect("compress corpus");
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(&compressed).expect("write fixture");
    f
}

/// Run the bench binary with --csv and return parsed rows (skipping header).
fn run_bench(mode: &str, fixture: &std::path::Path) -> Vec<Vec<String>> {
    let bin = env!("CARGO_BIN_EXE_mmapzstd-bench");
    let output = Command::new(bin)
        .args(["--mode", mode, "--runs", "1", "--warmup", "0", "--csv"])
        .arg(fixture)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {bin}: {e}"));

    assert!(
        output.status.success(),
        "binary exited with {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("binary output is utf-8");
    stdout
        .lines()
        .skip(1) // skip CSV header
        .filter(|l| !l.is_empty())
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect()
}

fn assert_row(rows: &[Vec<String>], mode: &str) {
    assert_eq!(rows.len(), 1, "expected 1 data row for mode {mode}");
    let row = &rows[0];
    // CSV columns: mode,run,wall_ns,decompressed_bytes,throughput_mbps,drss_kib,minfaults
    let decompressed: u64 = row[3].parse().unwrap_or_else(|_| panic!("parse decompressed: {:?}", row[3]));
    let throughput: f64 = row[4].parse().unwrap_or_else(|_| panic!("parse throughput: {:?}", row[4]));

    assert_eq!(
        decompressed,
        CORPUS_SIZE as u64,
        "decompressed size mismatch for mode {mode}: got {decompressed}, want {CORPUS_SIZE}"
    );
    assert!(
        throughput > 100.0,
        "throughput suspiciously low for mode {mode}: {throughput:.1} MB/s"
    );
}

#[test]
fn smoke_hugepage_anon() {
    let fixture = make_fixture();
    let rows = run_bench("hugepage-anon", fixture.path());
    assert_row(&rows, "hugepage-anon");
}

#[test]
fn smoke_bufreader() {
    let fixture = make_fixture();
    let rows = run_bench("bufreader", fixture.path());
    assert_row(&rows, "bufreader");
}

#[test]
fn smoke_hugepage_memfd() {
    let free = hugepages_free();
    if free < 4 {
        eprintln!("Skipping smoke_hugepage_memfd: HugePages_Free={free} < 4");
        return;
    }
    let fixture = make_fixture();
    let rows = run_bench("hugepage-memfd", fixture.path());
    assert_row(&rows, "hugepage-memfd");
}
