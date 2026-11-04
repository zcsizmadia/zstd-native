//! Decodes what libzstd produces: arbitrary data compressed by libzstd
//! with arbitrary parameters (level, checksum, content size, window,
//! long distance matching, dictionary, flush points) must round-trip
//! exactly through every zstd-native decoding API, with the asm and the
//! Rust Huffman loops.
#![no_main]

use std::io::Read;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zstd::zstd_safe::{CCtx, CDict, CParameter};
use zstd_native::{Dictionary, EndOp};
use zstd_native_fuzz::{
    DictSpec, c_stream_compress, chunk_sizes, first_diff, level_from, native_stream_decode,
    window_log_from,
};

#[derive(Arbitrary, Debug)]
struct Input {
    level_n: u8,
    checksum: bool,
    no_content_size: bool,
    window_log_n: u8,
    ldm: bool,
    dict: DictSpec,
    /// Flush points (lengths of the pieces before each flush).
    flushes: Vec<u16>,
    in_sizes: Vec<u16>,
    out_sizes: Vec<u16>,
    asm: bool,
    data: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let level = level_from(input.level_n);
    let cap = if level >= 16 { 48 << 10 } else { 256 << 10 };
    let data = &input.data[..input.data.len().min(cap)];
    let window_log = window_log_from(input.window_log_n);

    // libzstd compresses.
    let mut c = CCtx::create();
    c.set_parameter(CParameter::CompressionLevel(level))
        .unwrap();
    c.set_parameter(CParameter::ChecksumFlag(input.checksum))
        .unwrap();
    c.set_parameter(CParameter::ContentSizeFlag(!input.no_content_size))
        .unwrap();
    c.set_parameter(CParameter::WindowLog(window_log)).unwrap();
    c.set_parameter(CParameter::EnableLongDistanceMatching(input.ldm))
        .unwrap();
    let dict_bytes = input.dict.bytes();
    if let Some(d) = &dict_bytes {
        // A corrupt dictionary (libzstd parses a loaded one only when the
        // frame starts): nothing to decode.
        if CDict::try_create(d, level).is_none() {
            return;
        }
        c.load_dictionary(d).expect("ZSTD_CCtx_loadDictionary");
    }
    let mut ops = Vec::new();
    let mut left = data.len();
    for &n in input.flushes.iter().take(8) {
        let n = (n as usize).min(left);
        left -= n;
        ops.push((n, EndOp::Flush, 64 << 10));
    }
    ops.push((left, EndOp::End, 64 << 10));
    let frame = c_stream_compress(&mut c, data, &ops, Some(data.len() as u64));

    // zstd-native must load the dictionary libzstd loaded...
    let make_dict = || {
        dict_bytes
            .as_deref()
            .map(|d| Dictionary::new(d).expect("zstd-native rejects a dictionary libzstd accepts"))
    };
    // ...and decode the frame, with every API and both Huffman loop kinds.
    for asm in [input.asm, !input.asm] {
        zstd_native::__private::set_asm(Some(asm));
        let mut dec = match make_dict() {
            Some(d) => zstd_native::Decompressor::with_dictionary(d),
            None => zstd_native::Decompressor::new(),
        };
        let v = dec
            .decompress(&frame)
            .unwrap_or_else(|e| panic!("zstd-native rejects libzstd's frame: {e:?}"));
        assert!(v == data, "decompress() differs: {}", first_diff(&v, data));
        let mut buf = vec![0u8; data.len()];
        let n = dec
            .decompress_to_slice(&frame, &mut buf)
            .unwrap_or_else(|e| panic!("decompress_to_slice (exact size): {e:?}"));
        assert!(
            n == data.len() && buf == data,
            "decompress_to_slice differs"
        );
        if !data.is_empty() {
            let r = dec.decompress_to_slice(&frame, &mut buf[..data.len() - 1]);
            assert!(
                matches!(r, Err(zstd_native::Error::OutputTooSmall)),
                "one byte short of output: {r:?}"
            );
        }
    }
    zstd_native::__private::set_asm(Some(input.asm));
    let in_sizes = chunk_sizes(&input.in_sizes);
    let out_sizes = chunk_sizes(&input.out_sizes);
    let s = native_stream_decode(&frame, &in_sizes, &out_sizes, make_dict(), 0, usize::MAX);
    assert!(
        s.error.is_none() && s.complete,
        "streaming decode fails: {:?}, complete {}",
        s.error,
        s.complete
    );
    assert!(
        s.output == data,
        "streaming output differs: {}",
        first_diff(&s.output, data)
    );

    let mut io_out = Vec::new();
    match make_dict() {
        Some(d) => zstd_native::Decoder::with_dictionary(&frame[..], d).read_to_end(&mut io_out),
        None => zstd_native::Decoder::new(&frame[..]).read_to_end(&mut io_out),
    }
    .expect("io::Read decoder fails");
    assert!(
        io_out == data,
        "io::Read output differs: {}",
        first_diff(&io_out, data)
    );
    zstd_native::__private::set_asm(None);

    // The header reports what libzstd wrote.
    let size = zstd_native::content_size(&frame).expect("header");
    if input.no_content_size {
        assert_eq!(size, None);
    } else {
        assert_eq!(size, Some(data.len() as u64));
    }
});
