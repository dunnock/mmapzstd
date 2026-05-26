use std::fs;
use std::io::{self, Write};

use rand::RngCore;
use tempfile::NamedTempFile;

fn read_proc_stat() -> (u64, u64) {
    // /proc/self/stat field 10 = minflt, field 12 = majflt (1-indexed)
    let stat = fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat.split_whitespace().collect();
    let minflt: u64 = fields.get(9).and_then(|s| s.parse().ok()).unwrap_or(0);
    let majflt: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    (minflt, majflt)
}

fn read_vm_hwm_kb() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if line.starts_with("VmHWM:") {
            return line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        }
    }
    0
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

fn main() {
    let fixture = make_fixture();
    let path = fixture.path().to_path_buf();

    let (minflt_before, majflt_before) = read_proc_stat();

    let start = std::time::Instant::now();
    let file = std::fs::File::open(&path).expect("open file");
    let mmap = unsafe { memmap2::MmapOptions::new().map(&file).expect("mmap") };
    let mut dec = mmapzstd::decoder::Decoder::from_mmap(mmap).expect("from_mmap");
    io::copy(&mut dec, &mut io::sink()).expect("copy");
    let elapsed = start.elapsed();

    let (minflt_after, majflt_after) = read_proc_stat();
    let hwm_kb = read_vm_hwm_kb();

    let bytes: u64 = 256 * 1024 * 1024;
    let throughput_mb = bytes as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64();

    println!("=== mmapzstd::Decoder ===");
    println!("elapsed:      {:.3} ms", elapsed.as_secs_f64() * 1000.0);
    println!("throughput:   {:.1} MB/s", throughput_mb);
    println!("minor faults: {}", minflt_after - minflt_before);
    println!("major faults: {}", majflt_after - majflt_before);
    println!(
        "Peak RSS:     {} kB ({:.1} MB)",
        hwm_kb,
        hwm_kb as f64 / 1024.0
    );
}
