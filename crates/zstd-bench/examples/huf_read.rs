//! Time Huffman table reads alone (ns per tree description), on the tree
//! descriptions of small frames from the source and log corpora.

use std::time::Instant;

fn main() {
    let src = zstd_bench::source_corpus();
    let logs = zstd_bench::log_corpus(1 << 20);
    for (name, corpus) in [("source", &src), ("log", &logs)] {
        let frames: Vec<Vec<u8>> = corpus
            .as_chunks::<1024>()
            .0
            .iter()
            .take(256)
            .map(|c| zstd_bench::c_compress(c, 3))
            .collect();
        let descs: Vec<&[u8]> = frames
            .iter()
            .filter_map(|f| zstd_native::__private::huf_first_tree_description(f))
            .collect();
        let n = 2000;
        let t = Instant::now();
        let mut acc = 0;
        for d in &descs {
            acc += zstd_native::__private::huf_bench_read(d, n);
        }
        let ns = t.elapsed().as_secs_f64() * 1e9 / (descs.len() * n) as f64;
        println!(
            "{name}: {} tree descriptions, {ns:.0} ns per read ({acc})",
            descs.len()
        );
    }
}
