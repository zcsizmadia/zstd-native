//! Compresses arbitrary data with arbitrary parameters (level -7 to 22,
//! window log, long distance matching, workers, job size and overlap,
//! checksum, content size and dictionary ID flags,
//! loaded or prepared dictionaries from arbitrary bytes), one-shot with a
//! reused context or streaming with arbitrary input pieces, flushes,
//! frame ends and output sizes: the output must be byte-identical with
//! libzstd's for the same calls, and both libraries must decode it back.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zstd_native::Dictionary;
use zstd_native_fuzz::{
    EncDict, EncParams, OUT_CAP, c_accepts_dict, c_cdict, c_context, c_decompress,
    c_stream_compress, first_diff, first_frame_len, head, native_encoder, native_stream_compress,
    script_from,
};

#[derive(Arbitrary, Debug)]
struct Input {
    p: EncParams,
    one_shot: bool,
    pledge: bool,
    ops: Vec<(u32, u8, u16)>,
    final_out: u16,
    asm: bool,
    data: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let p = &input.p;
    let data = &input.data[..input.data.len().min(p.data_cap())];
    let mut e = match (c_accepts_dict(p), native_encoder(p)) {
        (true, Some(e)) => e,
        (false, None) => return,
        (true, None) => panic!(
            "zstd-native rejects a dictionary libzstd accepts: {:?}",
            p.dict
        ),
        (false, Some(_)) => panic!(
            "zstd-native accepts a dictionary libzstd rejects: {:?}",
            p.dict
        ),
    };
    let cdict = c_cdict(p);
    let mut c = c_context(p, cdict.as_ref());

    // The content of the frames, and the output of each library.
    let (content, want, got): (Vec<u8>, Vec<u8>, Vec<u8>);
    if input.one_shot && p.window_log() == 0 && !p.ldm {
        // ZSTD_compress2 against Compressor, twice on the same contexts (the
        // second frame reuses the tables).
        let mut nc = native_compressor(p);
        let mut want_v = Vec::new();
        let mut got_v = Vec::new();
        let mut content_v = Vec::new();
        for piece in [data, &data[..data.len() / 2]] {
            let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(piece.len()));
            c.compress2(&mut out, piece)
                .unwrap_or_else(|e| panic!("libzstd: {}", zstd_native_fuzz::c_error(e)));
            let mine = nc
                .compress(piece)
                .unwrap_or_else(|e| panic!("zstd-native: {e:?}"));
            want_v.extend_from_slice(&out);
            got_v.extend_from_slice(&mine);
            content_v.extend_from_slice(piece);
        }
        (content, want, got) = (content_v, want_v, got_v);
    } else {
        let ops = script_from(&input.ops, input.final_out, data.len());
        let consumed: usize = ops.iter().map(|o| o.0).sum();
        let pledged = input.pledge.then(|| first_frame_len(&ops));
        want = c_stream_compress(&mut c, data, &ops, pledged);
        got = native_stream_compress(&mut e, data, &ops, pledged);
        content = data[..consumed].to_vec();
    }
    assert!(
        got == want,
        "output differs from libzstd: {} (native {} / libzstd {})",
        first_diff(&got, &want),
        head(&got),
        head(&want)
    );

    // Both libraries decode it back.
    let dict_bytes = p.dict.spec().and_then(|s| s.bytes());
    zstd_native::__private::set_asm(Some(input.asm));
    let mut dec = match &dict_bytes {
        Some(d) => zstd_native::Decompressor::with_dictionary(Dictionary::new(d).unwrap()),
        None => zstd_native::Decompressor::new(),
    };
    let back = dec
        .decompress(&got)
        .unwrap_or_else(|e| panic!("zstd-native can't decode its own output: {e:?}"));
    zstd_native::__private::set_asm(None);
    assert!(
        back == content,
        "round trip differs: {}",
        first_diff(&back, &content)
    );
    let c_back = c_decompress(&got, OUT_CAP.max(content.len()), dict_bytes.as_deref())
        .unwrap_or_else(|e| panic!("libzstd can't decode the output: {e}"));
    assert!(
        c_back == content,
        "libzstd round trip differs: {}",
        first_diff(&c_back, &content)
    );
});

/// A one-shot zstd-native compressor with `p` applied (dictionary already
/// known to load, see `native_encoder`).
fn native_compressor(p: &EncParams) -> zstd_native::Compressor {
    let mut c = zstd_native::Compressor::new(p.level());
    match &p.dict {
        EncDict::None => {}
        EncDict::Loaded(s) => {
            if let Some(d) = s.bytes() {
                c.set_dictionary(&d).unwrap();
            }
        }
        EncDict::Prepared(s) => {
            if let Some(d) = s.bytes() {
                c.set_prepared_dictionary(
                    &zstd_native::EncoderDictionary::new(&d, p.level()).unwrap(),
                );
            }
        }
    }
    c.set_workers(p.workers());
    c.set_job_size(p.job_size());
    c.set_overlap_log(p.overlap_log());
    c.set_checksum(p.checksum());
    c.set_content_size_flag(p.content_size());
    c.set_dict_id_flag(p.dict_id());
    c
}
