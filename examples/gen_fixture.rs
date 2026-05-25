use std::io::Write;

use rand::RngCore;

fn main() {
    const BLOCK: usize = 4096;
    const TOTAL: usize = 256 * 1024 * 1024;

    let out_path = std::env::args().nth(1).expect("usage: gen_fixture <output_path>");

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
    let mut f = std::fs::File::create(&out_path).expect("create output");
    f.write_all(&compressed).expect("write fixture");
    eprintln!("wrote {} bytes to {}", compressed.len(), out_path);
}
