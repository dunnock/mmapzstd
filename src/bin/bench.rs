use clap::{Parser, ValueEnum};
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::time::Instant;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "mmapzstd-bench", version)]
struct Args {
    path: PathBuf,
    #[arg(long, default_value = "hugepage-anon")]
    mode: Mode,
    #[arg(long, default_value_t = 3)]
    runs: u32,
    #[arg(long, default_value_t = 65536)]
    bufreader_buf: usize,
    #[arg(long, default_value = "null")]
    sink: SinkKind,
    #[arg(long, default_value_t = 1)]
    warmup: u32,
    #[arg(long)]
    csv: bool,
}

#[derive(Clone, ValueEnum)]
enum Mode {
    HugepageAnon,
    HugepageMemfd,
    Bufreader,
}

#[derive(Clone, ValueEnum)]
enum SinkKind {
    Null,
    Count,
}

struct RunResult {
    wall_ns: u64,
    decompressed_bytes: u64,
    throughput_mbps: f64,
    drss_kib: i64,
    minfaults: u64,
}

fn read_vmrss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn read_minflt() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    // Field 10 (1-indexed) = minflt; field 2 (comm) is wrapped in parens.
    // Find closing ')' then parse the 8th whitespace-delimited token after it.
    let after_comm = stat.find(')').map(|i| &stat[i + 1..]).unwrap_or(&stat);
    let parts: Vec<&str> = after_comm.split_whitespace().collect();
    // After ')': state[0] ppid[1] pgrp[2] session[3] tty_nr[4] tpgid[5] flags[6] minflt[7]
    parts.get(7).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn hugepages_free() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("HugePages_Free:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn needed_hugepages(compressed_len: u64) -> u64 {
    const HUGEPAGE: u64 = 2 * 1024 * 1024;
    compressed_len.div_ceil(HUGEPAGE)
}

fn open_reader_hugepage_anon(path: &std::path::Path) -> io::Result<Box<dyn Read>> {
    #[cfg(target_os = "linux")]
    {
        mmapzstd::decoder::Decoder::open_hugepage(path).map(|d| Box::new(d) as Box<dyn Read>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        mmapzstd::decoder::Decoder::open(path).map(|d| Box::new(d) as Box<dyn Read>)
    }
}

fn open_reader_hugepage_memfd(path: &std::path::Path) -> io::Result<Box<dyn Read>> {
    #[cfg(target_os = "linux")]
    {
        mmapzstd::decoder::Decoder::open_hugepage_memfd(path).map(|d| Box::new(d) as Box<dyn Read>)
    }
    #[cfg(not(target_os = "linux"))]
    {
        mmapzstd::decoder::Decoder::open(path).map(|d| Box::new(d) as Box<dyn Read>)
    }
}

fn open_reader_bufreader(path: &std::path::Path, buf_size: usize) -> io::Result<Box<dyn Read>> {
    let file = std::fs::File::open(path)?;
    let buf = BufReader::with_capacity(buf_size, file);
    zstd::stream::Decoder::new(buf).map(|d| Box::new(d) as Box<dyn Read>)
}

fn measure_run(
    mode: &Mode,
    path: &std::path::Path,
    bufreader_buf: usize,
    sink: &SinkKind,
) -> io::Result<RunResult> {
    let rss_before = read_vmrss_kib() as i64;
    let minflt_before = read_minflt();
    let t0 = Instant::now();

    let mut reader: Box<dyn Read> = match mode {
        Mode::HugepageAnon => open_reader_hugepage_anon(path)?,
        Mode::HugepageMemfd => open_reader_hugepage_memfd(path)?,
        Mode::Bufreader => open_reader_bufreader(path, bufreader_buf)?,
    };

    let decompressed_bytes = match sink {
        SinkKind::Null => io::copy(&mut reader, &mut io::sink())?,
        SinkKind::Count => {
            let mut buf = Vec::new();
            io::copy(&mut reader, &mut buf)?
        }
    };

    let wall_ns = t0.elapsed().as_nanos() as u64;
    let rss_after = read_vmrss_kib() as i64;
    let minflt_after = read_minflt();

    let wall_secs = wall_ns as f64 / 1_000_000_000.0;
    let throughput_mbps = decompressed_bytes as f64 / wall_secs / (1024.0 * 1024.0);

    Ok(RunResult {
        wall_ns,
        decompressed_bytes,
        throughput_mbps,
        drss_kib: rss_after - rss_before,
        minfaults: minflt_after - minflt_before,
    })
}

fn with_commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out.chars().rev().collect()
}

fn format_bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if n >= 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

fn median_u64(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn main() -> io::Result<()> {
    let args = Args::parse();

    let file_size = std::fs::metadata(&args.path)?.len();

    let mode_name = match args.mode {
        Mode::HugepageAnon => "hugepage-anon",
        Mode::HugepageMemfd => "hugepage-memfd",
        Mode::Bufreader => "bufreader",
    };

    // Warn when hugepages likely insufficient before attempting hugepage modes.
    #[cfg(target_os = "linux")]
    if matches!(args.mode, Mode::HugepageAnon | Mode::HugepageMemfd) {
        let free = hugepages_free();
        let needed = needed_hugepages(file_size);
        if free < needed {
            eprintln!(
                "note: hugepage allocation failed (HugePages_Free={}), using fallback path",
                free
            );
        }
    }

    if !args.csv {
        println!("mmapzstd-bench v{VERSION}");
        println!("file: {}", args.path.display());
        println!(
            "size: {} bytes ({:.1} MiB compressed)",
            with_commas(file_size),
            file_size as f64 / (1024.0 * 1024.0)
        );
        println!("mode: {mode_name}");
        println!("runs: {} (warmup {} discarded)", args.runs, args.warmup);
        println!();
    }

    for _ in 0..args.warmup {
        measure_run(&args.mode, &args.path, args.bufreader_buf, &args.sink)?;
    }

    let mut results: Vec<(u32, RunResult)> = Vec::new();
    for i in 0..args.runs {
        let r = measure_run(&args.mode, &args.path, args.bufreader_buf, &args.sink)?;
        results.push((i + 1, r));
    }

    if args.csv {
        println!("mode,run,wall_ns,decompressed_bytes,throughput_mbps,drss_kib,minfaults");
        for (run, r) in &results {
            println!(
                "{mode_name},{run},{},{},{:.2},{},{}",
                r.wall_ns, r.decompressed_bytes, r.throughput_mbps, r.drss_kib, r.minfaults
            );
        }
    } else {
        println!(
            "| {:<3} | {:<9} | {:<12} | {:<17} | {:<10} | {:<12} |",
            "run", "wall", "decompressed", "throughput (MB/s)", "dRSS (KiB)", "minor faults"
        );
        println!(
            "|-----|-----------|--------------|-------------------|------------|--------------|"
        );
        for (run, r) in &results {
            println!(
                "| {:<3} | {:<9} | {:<12} | {:<17} | {:<10} | {:<12} |",
                run,
                format!("{} ms", r.wall_ns / 1_000_000),
                format_bytes(r.decompressed_bytes),
                with_commas(r.throughput_mbps as u64),
                r.drss_kib,
                r.minfaults,
            );
        }

        if !results.is_empty() {
            let wall_ns_vals: Vec<u64> = results.iter().map(|(_, r)| r.wall_ns).collect();
            let mbps_vals: Vec<f64> = results.iter().map(|(_, r)| r.throughput_mbps).collect();
            let med_ns = median_u64(wall_ns_vals);
            let med_mbps = median_f64(mbps_vals);
            println!();
            println!(
                "median: {} ms / {} MB/s",
                med_ns / 1_000_000,
                with_commas(med_mbps as u64)
            );
        }
    }

    Ok(())
}
