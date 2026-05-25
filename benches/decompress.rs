use std::fs::File;
use std::io::{self, BufReader, Write};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use rand::RngCore;
use tempfile::NamedTempFile;

fn make_fixture() -> NamedTempFile {
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
    let mut f = NamedTempFile::new().expect("tempfile");
    f.write_all(&compressed).expect("write fixture");
    f
}

fn bench_decompress(c: &mut Criterion) {
    let fixture = make_fixture();
    let path = fixture.path().to_path_buf();

    let mut group = c.benchmark_group("mmapzstd");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("decompress", |b| {
        b.iter(|| {
            let mut dec = mmapzstd::decoder::Decoder::open(&path).expect("open");
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
}

criterion_group!(benches, bench_decompress);
criterion_main!(benches);
