//! The streaming decoder against C libzstd: input and output in pieces of
//! random sizes, windows small enough to wrap the window buffer many times.

use std::io::{Read, Write};

use zstd_bench::{Rng, c_compress, log_corpus, numeric_corpus, random_corpus, source_corpus};
use zstd_native::{Decoder, Error, StreamDecoder};

/// Decodes `src` feeding at most `in_max` input and `out_max` output bytes
/// per call (random sizes from 1).
fn stream_decode(
    dec: &mut StreamDecoder,
    src: &[u8],
    rng: &mut Rng,
    in_max: usize,
    out_max: usize,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; out_max];
    let mut ip = 0;
    loop {
        let n_in = (1 + rng.below(in_max as u64) as usize).min(src.len() - ip);
        let n_out = 1 + rng.below(out_max as u64) as usize;
        let p = dec.decode(&src[ip..ip + n_in], &mut buf[..n_out])?;
        ip += p.read;
        out.extend_from_slice(&buf[..p.written]);
        if ip == src.len() && p.written == 0 && p.read == 0 {
            return if p.at_frame_boundary {
                Ok(out)
            } else {
                Err(Error::Truncated)
            };
        }
    }
}

fn c_stream(
    data: &[u8],
    level: i32,
    window_log: Option<u32>,
    checksum: bool,
    ldm: bool,
) -> Vec<u8> {
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
    enc.include_checksum(checksum).unwrap();
    enc.include_contentsize(false).unwrap();
    if let Some(w) = window_log {
        enc.window_log(w).unwrap();
    }
    if ldm {
        enc.long_distance_matching(true).unwrap();
    }
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

#[test]
fn wraps_and_piece_sizes() {
    let mut rng = Rng::new(77);
    let mut big = source_corpus();
    big.extend(log_corpus(2 << 20));
    let copy = big[..300_000].to_vec();
    big.extend(&copy); // long-range repeat
    for (wlog, level, ldm) in [
        (10, 3, false),
        (12, 1, false),
        (17, 3, false),
        (17, 19, false),
        (20, 6, true),
        (23, 3, true),
    ] {
        let c = c_stream(&big, level, Some(wlog), true, ldm);
        let mut dec = StreamDecoder::new();
        for (in_max, out_max) in [
            (1 << 20, 1 << 20),
            (7, 1 << 16),
            (100_000, 13),
            (4096, 4096),
        ] {
            let got = stream_decode(&mut dec, &c, &mut rng, in_max, out_max)
                .unwrap_or_else(|e| panic!("wlog {wlog} L{level}: {e}"));
            assert!(
                got == big,
                "wlog {wlog} L{level} in {in_max} out {out_max}: output differs"
            );
        }
    }
}

#[test]
fn read_adapter_frames_and_errors() {
    let a = numeric_corpus(500_000);
    let b = log_corpus(300_000);
    let mut c = c_compress(&a, 3);
    c.extend_from_slice(&0x184D_2A5Fu32.to_le_bytes());
    c.extend_from_slice(&3u32.to_le_bytes());
    c.extend_from_slice(b"xyz");
    c.extend(c_stream(&b, 5, None, true, false));
    let mut want = a.clone();
    want.extend_from_slice(&b);

    let mut got = Vec::new();
    Decoder::new(&c[..]).read_to_end(&mut got).unwrap();
    assert!(got == want);
    // Small reads.
    let mut d = Decoder::with_buffer(std::io::BufReader::with_capacity(5, &c[..]));
    let mut got = Vec::new();
    let mut buf = [0u8; 3];
    loop {
        let n = d.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got == want);

    // Truncated: an UnexpectedEof error, at every cut point (sampled).
    for cut in (1..c.len()).step_by(c.len() / 97 + 1) {
        let mut out = Vec::new();
        let r = Decoder::new(&c[..cut]).read_to_end(&mut out);
        let boundary = cut == c.len();
        if !boundary {
            let e = r.expect_err("truncated input must fail");
            assert!(
                e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::InvalidData,
                "cut {cut}: {e:?}"
            );
        }
    }

    // Corrupt checksum.
    let mut bad = c_stream(&b[..10_000], 3, None, true, false);
    let n = bad.len();
    bad[n - 2] ^= 0x40;
    let e = Decoder::new(&bad[..])
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn window_limit() {
    let data = random_corpus(1 << 20);
    let c = c_stream(&data, 1, Some(20), false, false);
    let mut dec = StreamDecoder::new();
    dec.set_window_log_max(19);
    let mut out = vec![0u8; 1 << 16];
    assert_eq!(dec.decode(&c, &mut out), Err(Error::WindowTooLarge));
    let mut dec = StreamDecoder::new();
    dec.set_window_log_max(20);
    let mut rng = Rng::new(1);
    assert!(stream_decode(&mut dec, &c, &mut rng, 1 << 16, 1 << 16).unwrap() == data);
}

/// An empty frame (one empty last raw block) whose header gives window log
/// `log`, with no content size (`fcs: false`) or a content size of 0 (so the
/// streaming decoder needs no window buffer).
fn empty_frame(log: u32, fcs: bool) -> Vec<u8> {
    let mut f = 0xFD2F_B528u32.to_le_bytes().to_vec();
    f.push(if fcs { 0x80 } else { 0 }); // not single segment
    f.push(((log - 10) << 3) as u8);
    if fcs {
        f.extend_from_slice(&[0; 4]);
    }
    f.extend_from_slice(&[1, 0, 0]);
    f
}

/// Streams `src` through `dec` in one call.
fn stream_once(dec: &mut StreamDecoder, src: &[u8]) -> Result<bool, Error> {
    let mut out = [0u8; 64];
    let p = dec.decode(src, &mut out)?;
    Ok(p.read == src.len() && p.written == 0 && p.at_frame_boundary)
}

#[test]
fn window_log_max_by_pointer_width() {
    // ZSTD_WINDOWLOG_MAX is 31, 30 on 32-bit targets: larger frame headers
    // fail with frameParameter_windowTooLarge in every mode, one-shot too
    let w31 = empty_frame(31, false);
    let w31_fcs = empty_frame(31, true);
    let w30_fcs = empty_frame(30, true);
    let c_ok = |f: &[u8]| {
        zstd::bulk::Decompressor::new()
            .unwrap()
            .decompress(f, 16)
            .is_ok()
    };
    let io_decode = |f: &[u8], log: u32| {
        let mut d = Decoder::new(f);
        d.set_window_log_max(log);
        let mut v = Vec::new();
        d.read_to_end(&mut v).map(|_| v)
    };
    // window log 30: accepted everywhere (the streaming limit can be 30)
    assert!(c_ok(&w30_fcs));
    assert_eq!(zstd_native::decompress(&w30_fcs), Ok(Vec::new()));
    let mut dec = StreamDecoder::new();
    dec.set_window_log_max(31); // clamped to 30 on 32-bit targets
    assert_eq!(stream_once(&mut dec, &w30_fcs), Ok(true));
    assert!(io_decode(&w30_fcs, 31).unwrap().is_empty());
    // the default streaming limit (27) rejects window log 31 everywhere
    let mut dec = StreamDecoder::new();
    assert_eq!(stream_once(&mut dec, &w31_fcs), Err(Error::WindowTooLarge));

    #[cfg(target_pointer_width = "64")]
    {
        assert!(c_ok(&w31));
        let fh = zstd_native::FrameHeader::parse(&w31).unwrap();
        assert_eq!(fh.window_size, 1 << 31);
        assert_eq!(zstd_native::content_size(&w31), Ok(None));
        assert_eq!(zstd_native::decompress(&w31), Ok(Vec::new()));
        let mut dec = StreamDecoder::new();
        dec.set_window_log_max(31);
        assert_eq!(stream_once(&mut dec, &w31_fcs), Ok(true));
        assert!(io_decode(&w31_fcs, 31).unwrap().is_empty());
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(!c_ok(&w31));
        assert_eq!(
            zstd_native::FrameHeader::parse(&w31),
            Err(Error::WindowTooLarge)
        );
        assert_eq!(zstd_native::content_size(&w31), Err(Error::WindowTooLarge));
        assert_eq!(zstd_native::decompress(&w31), Err(Error::WindowTooLarge));
        assert_eq!(
            zstd_native::decompress(&w31_fcs),
            Err(Error::WindowTooLarge)
        );
        let mut dec = StreamDecoder::new();
        dec.set_window_log_max(31);
        assert_eq!(stream_once(&mut dec, &w31_fcs), Err(Error::WindowTooLarge));
        let e = io_decode(&w31_fcs, 31).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }
}

#[test]
fn dictionary_streaming() {
    let corpus = log_corpus(2 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(3000).collect();
    let dict = zstd::dict::from_samples(&samples, 32 << 10).unwrap();
    let mut rng = Rng::new(5);
    for s in samples.iter().take(100) {
        let mut c = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        let comp = c.compress(s).unwrap();
        let mut dec = StreamDecoder::with_dictionary(zstd_native::Dictionary::new(&dict).unwrap());
        let got = stream_decode(&mut dec, &comp, &mut rng, 64, 64).unwrap();
        assert!(got == *s);
    }
}

#[test]
fn levels_one_shot_frames_streamed() {
    let mut rng = Rng::new(9);
    for data in [
        source_corpus(),
        log_corpus(1 << 20),
        numeric_corpus(1 << 20),
    ] {
        for level in [-3, 1, 3, 9, 19] {
            let c = c_compress(&data, level);
            let mut dec = StreamDecoder::new();
            let got = stream_decode(&mut dec, &c, &mut rng, 1 << 14, 1 << 14).unwrap();
            assert!(got == data, "L{level}");
        }
    }
}
