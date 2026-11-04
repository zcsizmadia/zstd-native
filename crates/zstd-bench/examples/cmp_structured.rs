//! Byte-exactness probe: compresses one dataset with libzstd, zstd-native and
//! structured-zstd and reports sizes and whether the outputs are identical.
//! `cargo run --profile bench -p zstd-bench --example cmp_structured -- <dataset> <level>`

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let ds = args.get(1).map_or("source", |s| s);
    let level: i32 = args.get(2).map_or(19, |s| s.parse().unwrap());
    let data = zstd_bench::datasets()
        .into_iter()
        .find(|(n, _)| n == ds)
        .expect("dataset")
        .1;
    let c = zstd_bench::c_compress(&data, level);
    let n = zstd_native::compress(&data, level).unwrap();
    let s = structured_zstd::encoding::compress_slice_to_vec(
        &data,
        structured_zstd::encoding::CompressionLevel::Level(level),
    );
    println!(
        "{ds} L{level}: c {} native {} (identical: {}) structured {} (identical: {})",
        c.len(),
        n.len(),
        c == n,
        s.len(),
        c == s
    );
    // Where does the structured output first differ?
    if c != s {
        let first = c
            .iter()
            .zip(&s)
            .position(|(a, b)| a != b)
            .unwrap_or(c.len().min(s.len()));
        println!("  first difference at byte {first} of {}", c.len());
    }
}
