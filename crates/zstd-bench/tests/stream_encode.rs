//! The streaming encoder must produce exactly libzstd's bytes
//! (`ZSTD_compressStream2`, buffered mode) for the same level, input
//! pieces, operations and output buffer sizes.

use std::io::Write;

use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CParameter, InBuffer, OutBuffer};
use zstd_bench::{Rng, log_corpus, random_corpus, source_corpus};
use zstd_native::{EndOp, StreamEncoder};

/// One call sequence: `ops[i]` gives `input` bytes with an operation, the
/// call repeated (with the rest of the input and a fresh output buffer of
/// `out_size` bytes) until the input is consumed and, for flush and end,
/// nothing remains to flush.
#[derive(Clone, Debug)]
struct Script {
    level: i32,
    window_log: u32,
    pledged: Option<u64>,
    /// `ZSTD_c_checksumFlag` and `ZSTD_c_contentSizeFlag`.
    checksum: bool,
    content_size: bool,
    /// (input length, operation, output buffer size)
    ops: Vec<(usize, EndOp, usize)>,
}

impl Script {
    fn new(level: i32) -> Self {
        Script {
            level,
            window_log: 0,
            pledged: None,
            checksum: false,
            content_size: true,
            ops: Vec::new(),
        }
    }

    fn total(&self) -> usize {
        self.ops.iter().map(|o| o.0).sum()
    }
}

fn directive(op: EndOp) -> ZSTD_EndDirective {
    match op {
        EndOp::Continue => ZSTD_EndDirective::ZSTD_e_continue,
        EndOp::Flush => ZSTD_EndDirective::ZSTD_e_flush,
        EndOp::End => ZSTD_EndDirective::ZSTD_e_end,
    }
}

/// Runs `script` on `data` with libzstd.
fn run_c(script: &Script, data: &[u8], cctx: Option<&mut CCtx<'static>>) -> Vec<u8> {
    let mut own;
    let cctx = match cctx {
        Some(c) => c,
        None => {
            own = CCtx::create();
            own.set_parameter(CParameter::CompressionLevel(script.level))
                .unwrap();
            if script.window_log != 0 {
                own.set_parameter(CParameter::WindowLog(script.window_log))
                    .unwrap();
            }
            &mut own
        }
    };
    cctx.set_parameter(CParameter::ChecksumFlag(script.checksum))
        .unwrap();
    cctx.set_parameter(CParameter::ContentSizeFlag(script.content_size))
        .unwrap();
    if script.pledged.is_some() {
        cctx.set_pledged_src_size(script.pledged).unwrap();
    }
    let mut out = Vec::new();
    let mut pos = 0;
    let mut buf = Vec::new();
    for &(len, op, out_size) in &script.ops {
        let piece = &data[pos..pos + len];
        pos += len;
        let mut input = InBuffer::around(piece);
        buf.resize(out_size, 0);
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            let remaining = cctx
                .compress_stream2(&mut output, &mut input, directive(op))
                .unwrap_or_else(|e| panic!("libzstd: {}", zstd::zstd_safe::get_error_name(e)));
            let n = output.pos();
            out.extend_from_slice(&buf[..n]);
            let done = input.pos() == piece.len() && (op == EndOp::Continue || remaining == 0);
            if done {
                break;
            }
        }
    }
    out
}

/// Runs `script` on `data` with zstd-native.
fn run_native(script: &Script, data: &[u8], enc: Option<&mut StreamEncoder>) -> Vec<u8> {
    let mut own;
    let enc = match enc {
        Some(e) => e,
        None => {
            own = StreamEncoder::new(script.level);
            own.set_window_log(script.window_log);
            &mut own
        }
    };
    enc.set_checksum(script.checksum).unwrap();
    enc.set_content_size_flag(script.content_size).unwrap();
    if script.pledged.is_some() {
        enc.set_pledged_src_size(script.pledged).unwrap();
    }
    let mut out = Vec::new();
    let mut pos = 0;
    let mut buf = Vec::new();
    for &(len, op, out_size) in &script.ops {
        let mut piece = &data[pos..pos + len];
        pos += len;
        buf.resize(out_size, 0);
        loop {
            let p = enc
                .compress(piece, &mut buf, op)
                .unwrap_or_else(|e| panic!("zstd-native: {e}"));
            piece = &piece[p.read..];
            out.extend_from_slice(&buf[..p.written]);
            if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                break;
            }
        }
    }
    out
}

/// Runs `script` with both and checks the outputs are equal and decode to
/// `data`. Returns the output.
fn check(name: &str, script: &Script, data: &[u8]) -> Vec<u8> {
    let data = &data[..script.total()];
    let want = run_c(script, data, None);
    let got = run_native(script, data, None);
    compare(name, script, &got, &want);
    assert!(
        zstd_native::decompress(&got).unwrap() == data,
        "{name}: round trip"
    );
    got
}

fn compare(name: &str, script: &Script, got: &[u8], want: &[u8]) {
    if got != want {
        let first = got
            .iter()
            .zip(want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        let ops: Vec<_> = script.ops.iter().take(12).collect();
        panic!(
            "{name} L{} wlog {} pledged {:?} ({} bytes): output differs from libzstd at byte {first} \
             (sizes {} vs {}); first ops {ops:?}",
            script.level,
            script.window_log,
            script.pledged,
            script.total(),
            got.len(),
            want.len()
        );
    }
}

/// `n` bytes of input: the source corpus repeated with some changes (so
/// that matches reach far back, across the input buffer's wrap).
fn input(n: usize) -> Vec<u8> {
    let src = source_corpus();
    let mut out = Vec::with_capacity(n);
    let mut r = Rng::new(9);
    while out.len() < n {
        let start = r.below(src.len() as u64 / 2) as usize;
        let len = (r.below(300_000) as usize + 1000).min(src.len() - start);
        out.extend_from_slice(&src[start..start + len]);
        // a little noise
        let k = out.len() - 1;
        out[k] ^= r.next_u64() as u8;
    }
    out.truncate(n);
    out
}

/// Continue calls of `chunk` bytes, then an empty end call.
fn chunked(level: i32, window_log: u32, n: usize, chunk: usize, out_size: usize) -> Script {
    let mut s = Script::new(level);
    s.window_log = window_log;
    let mut left = n;
    while left > 0 {
        let c = chunk.min(left);
        s.ops.push((c, EndOp::Continue, out_size));
        left -= c;
    }
    s.ops.push((0, EndOp::End, out_size));
    s
}

const OUT: usize = 1 << 17; // smaller than compress_bound(128 KB): uses the output buffer

/// The levels' strategies: fast (negative and 1), dfast (3), greedy (5),
/// lazy (7, row), lazy2 (12), btlazy2 (14, with small windows), btopt (16),
/// btultra2 (19).
const LEVELS: &[i32] = &[-5, 1, 3, 5, 7, 12, 14, 16, 19];

#[test]
fn chunking_does_not_matter() {
    // Small windows (log 17 to 19) so that the input buffer wraps several
    // times.
    let data = input(1_500_000);
    for &level in LEVELS {
        for wlog in [17, 19] {
            let n = if level >= 16 { 700_000 } else { data.len() };
            let mut first = None;
            for (chunk, out) in [(n, OUT), (131_072, 1 << 20), (65_537, 4096), (7_777, 777)] {
                let s = chunked(level, wlog, n, chunk, out);
                let got = check("chunks", &s, &data);
                match &first {
                    None => first = Some(got),
                    Some(f) => assert!(
                        *f == got,
                        "L{level} wlog {wlog}: output depends on the chunk size {chunk}"
                    ),
                }
            }
        }
    }
}

#[test]
fn single_bytes() {
    let data = input(400_000);
    for level in [1, 3, 6, 13, 17] {
        let s = chunked(level, 17, data.len(), 1, 100);
        check("bytes", &s, &data);
    }
}

#[test]
fn default_windows() {
    // The levels' own windows (unknown size): 512 KB (level 1) to 8 MB
    // (level 19). Inputs a few times the window for the fast levels.
    let data = input(5 << 20);
    for (level, n) in [
        (-1, 3 << 20),
        (1, 3 << 20),
        (2, 3 << 20),
        (4, 5 << 20),
        (6, 5 << 20),
    ] {
        let s = chunked(level, 0, n, 100_000, OUT);
        check("default", &s, &data);
    }
}

#[test]
fn flushes() {
    let data = input(1_200_000);
    let mut r = Rng::new(1);
    for &level in LEVELS {
        for wlog in [0, 18] {
            let n = if level >= 16 { 600_000 } else { data.len() };
            let mut s = Script::new(level);
            s.window_log = wlog;
            let mut left = n;
            while left > 0 {
                let c = (r.below(150_000) as usize + 1).min(left);
                let op = match r.below(4) {
                    0 => EndOp::Flush,
                    _ => EndOp::Continue,
                };
                s.ops
                    .push((c, op, [64, 4096, OUT, 1 << 20][r.below(4) as usize]));
                left -= c;
            }
            s.ops.push((0, EndOp::Flush, 1000));
            s.ops.push((0, EndOp::Flush, 1000));
            s.ops.push((0, EndOp::End, 1000));
            check("flushes", &s, &data);
        }
    }
}

/// Random call sequences: chunk sizes, operations (ending frames in the
/// middle, with or without input) and output sizes.
#[test]
fn random_scripts() {
    let data = input(2_000_000);
    let mut r = Rng::new(2);
    for round in 0..40 {
        let level = [-3, 1, 2, 3, 4, 5, 6, 8, 10, 12, 13, 15, 16, 18][round % 14];
        let wlog = [0, 17, 18, 20][r.below(4) as usize];
        let n = if level >= 16 { 500_000 } else { 1_500_000 };
        let mut s = Script::new(level);
        s.window_log = wlog;
        // every other round with the checksum, every third without the
        // content size
        s.checksum = round % 2 == 1;
        s.content_size = round % 3 != 2;
        let mut left = n;
        while left > 0 {
            let c = match r.below(5) {
                0 => r.below(100) as usize,
                1 => 131_072,
                2 => 262_144,
                _ => r.below(200_000) as usize,
            }
            .min(left);
            let op = match r.below(10) {
                0 => EndOp::Flush,
                1 => EndOp::End,
                _ => EndOp::Continue,
            };
            let out = [1, 100, 4096, OUT, 1 << 20, 4 << 20][r.below(6) as usize];
            s.ops.push((c, op, out));
            left -= c;
        }
        s.ops.push((0, EndOp::End, 4096));
        let data = &data[..n];
        let want = run_c(&s, data, None);
        let got = run_native(&s, data, None);
        compare("random", &s, &got, &want);
        // several frames: decode all of them
        let mut dec = Vec::new();
        zstd_native::Decoder::new(&got[..])
            .read_to_end_helper(&mut dec)
            .unwrap();
        assert!(dec == data, "random L{level}: round trip");
    }
}

/// `read_to_end` without importing `Read` at the top (it would clash with
/// nothing, but keeps the imports minimal).
trait ReadToEnd {
    fn read_to_end_helper(&mut self, out: &mut Vec<u8>) -> std::io::Result<usize>;
}

impl<R: std::io::Read> ReadToEnd for R {
    fn read_to_end_helper(&mut self, out: &mut Vec<u8>) -> std::io::Result<usize> {
        self.read_to_end(out)
    }
}

/// libzstd compresses an end call's input directly, without the input
/// buffer, when it finds that buffer empty (at the start of a frame, or
/// just after wrapping) and the output has room for `compress_bound` of the
/// input: block boundaries then differ.
#[test]
fn end_with_input() {
    let data = input(1_500_000);
    for &level in LEVELS {
        let n = if level >= 16 { 700_000 } else { data.len() };
        for wlog in [0, 17] {
            for out in [100, OUT, 4 << 20] {
                // everything in one end call (the content size is known)
                let mut s = Script::new(level);
                s.window_log = wlog;
                s.ops.push((n, EndOp::End, out));
                let got = check("one call", &s, &data);
                if out == 4 << 20 && wlog == 0 {
                    // that is libzstd's one-shot compression
                    assert!(got == zstd_native::compress(&data[..n], level).unwrap());
                }
                // the input buffer just wrapped (2 blocks of 128 KB fill
                // a 128 KB window's buffer), then the end with the rest
                let mut s = Script::new(level);
                s.window_log = wlog;
                s.ops.push((262_144, EndOp::Continue, OUT));
                s.ops.push((n - 262_144, EndOp::End, out));
                check("after wrap", &s, &data);
                // an empty continue first: no size known, empty buffer
                let mut s = Script::new(level);
                s.window_log = wlog;
                s.ops.push((0, EndOp::Continue, out));
                s.ops.push((n, EndOp::End, out));
                check("empty first", &s, &data);
            }
        }
    }
}

#[test]
fn small_inputs() {
    let data = input(300_000);
    for &level in LEVELS {
        for n in [
            0, 1, 7, 8, 100, 1000, 65_536, 131_071, 131_072, 131_073, 262_144, 300_000,
        ] {
            for pledged in [false, true] {
                // continue + end, end only, and flushes in between
                let mut s = chunked(level, 0, n, 50_000, OUT);
                if pledged {
                    s.pledged = Some(n as u64);
                }
                check("small", &s, &data);
                let mut s = Script::new(level);
                if pledged {
                    s.pledged = Some(n as u64);
                }
                s.ops.push((n / 2, EndOp::Flush, 50));
                s.ops.push((n - n / 2, EndOp::End, 50));
                check("small flush", &s, &data);
            }
        }
    }
}

#[test]
fn pledged_size() {
    let data = input(3 << 20);
    for &level in &[-5, 1, 3, 5, 7, 12, 16] {
        for n in [500_000, 3 << 20] {
            let n = if level >= 16 { n.min(1_000_000) } else { n };
            let mut s = chunked(level, 0, n, 100_000, OUT);
            s.pledged = Some(n as u64);
            let got = check("pledged", &s, &data);
            assert_eq!(zstd_native::content_size(&got).unwrap(), Some(n as u64));
        }
    }
    // a wrong size is an error
    for (pledged, n) in [(1000u64, 999usize), (1000, 1001)] {
        let mut enc = StreamEncoder::new(3);
        enc.set_pledged_src_size(Some(pledged)).unwrap();
        let mut out = vec![0u8; 4096];
        let r = enc
            .compress(&data[..n], &mut out, EndOp::Continue)
            .and_then(|_| enc.compress(&[], &mut out, EndOp::End));
        assert_eq!(r.unwrap_err(), zstd_native::Error::SrcSizeWrong);
        // the encoder starts over
        assert!(enc.at_frame_boundary());
    }
    // not in the middle of a frame
    let mut enc = StreamEncoder::new(3);
    let mut out = vec![0u8; 4096];
    enc.compress(b"abc", &mut out, EndOp::Continue).unwrap();
    assert_eq!(
        enc.set_pledged_src_size(Some(3)),
        Err(zstd_native::Error::StageWrong)
    );
}

/// A reused encoder (several frames): the row match finder's hash salt
/// changes from frame to frame in both.
#[test]
fn several_frames() {
    let data = input(800_000);
    for &level in &[1, 3, 5, 7, 11, 16] {
        let mut cctx = CCtx::create();
        cctx.set_parameter(CParameter::CompressionLevel(level))
            .unwrap();
        let mut enc = StreamEncoder::new(level);
        for (k, n) in [300_000usize, 1000, 0, 500_000, 70_000]
            .into_iter()
            .enumerate()
        {
            let piece = &data[k * 1000..k * 1000 + n];
            let mut s = chunked(level, 0, n, 33_333, OUT);
            if k == 3 {
                s.pledged = Some(n as u64);
            }
            let want = run_c(&s, piece, Some(&mut cctx));
            let got = run_native(&s, piece, Some(&mut enc));
            compare("frames", &s, &got, &want);
        }
    }
}

/// Level 22 with an unknown size: a 128 MiB window, with long distance
/// matching (see `ldm_encode.rs` for more); with a known size up to
/// 64 MiB, a smaller window without.
#[test]
fn level22_unknown_and_known_size() {
    let data = input(200_000);
    check(
        "L22 unknown",
        &chunked(22, 0, data.len(), 50_000, OUT),
        &data,
    );
    check("L22 tiny", &chunked(22, 0, 3, 50_000, OUT), &data);
    let mut s = chunked(22, 0, data.len(), 50_000, OUT);
    s.pledged = Some(data.len() as u64);
    check("L22", &s, &data);
}

#[test]
fn io_encoder() {
    let data = {
        let mut v = log_corpus(1 << 20);
        v.extend_from_slice(&random_corpus(100_000));
        v.extend_from_within(..500_000);
        v
    };
    for level in [1, 3, 9, 19] {
        for chunk in [1000, 100_000, data.len()] {
            let mut enc = zstd_native::Encoder::new(Vec::new(), level);
            let mut c = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
            for (i, piece) in data.chunks(chunk).enumerate() {
                enc.write_all(piece).unwrap();
                c.write_all(piece).unwrap();
                if i % 7 == 3 {
                    enc.flush().unwrap();
                    c.flush().unwrap();
                }
            }
            let got = enc.finish().unwrap();
            let want = c.finish().unwrap();
            assert!(
                got == want,
                "io L{level} chunk {chunk}: differs from libzstd"
            );
            let mut back = Vec::new();
            zstd_native::Decoder::new(&got[..])
                .read_to_end_helper(&mut back)
                .unwrap();
            assert!(back == data);
        }
    }
    // empty
    let enc = zstd_native::Encoder::new(Vec::new(), 3);
    let got = enc.finish().unwrap();
    let want = zstd::stream::write::Encoder::new(Vec::new(), 3)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(got, want);
    assert_eq!(zstd_native::decompress(&got).unwrap(), b"");
    // pledged
    let mut enc = zstd_native::Encoder::new(Vec::new(), 3);
    enc.set_pledged_src_size(Some(10)).unwrap();
    enc.write_all(b"0123456789").unwrap();
    let got = enc.finish().unwrap();
    assert_eq!(zstd_native::content_size(&got).unwrap(), Some(10));
}

/// Windows of 1 to 16 KB (blocks as small): the input buffer wraps every
/// few blocks, and greedy to lazy2 use the hash chain (window log up to
/// 14) instead of the row match finder.
#[test]
fn small_windows() {
    let data = input(400_000);
    for level in [-2, 1, 2, 3, 5, 6, 8, 13, 16, 17, 18, 19, 20, 21, 22] {
        for wlog in [10, 12, 14] {
            let n = if level >= 17 { 150_000 } else { data.len() };
            let s = chunked(level, wlog, n, 50_000, OUT);
            check("small window", &s, &data);
        }
    }
}

/// With the content checksum (`ZSTD_c_checksumFlag`) and without the
/// content size (`ZSTD_c_contentSizeFlag`): a few levels per strategy,
/// flushes, tiny output buffers (the checksum written out in pieces), the
/// end-with-input shortcut, pledged sizes, empty frames, reused encoders
/// and the `io::Write` encoder.
#[test]
fn frame_flags() {
    let data = input(1_500_000);
    for &level in &[-5, 1, 2, 3, 4, 5, 7, 12, 14, 16, 19, 22] {
        let n = if level >= 16 { 300_000 } else { 1_200_000 };
        for (checksum, content_size) in [(true, true), (true, false), (false, false)] {
            let what = format!("flags {checksum} {content_size}");
            let mut variants = Vec::new();
            // chunks with flushes
            let mut s = Script::new(level);
            for (i, c) in data[..n].chunks(70_000).enumerate() {
                let op = if i % 3 == 1 {
                    EndOp::Flush
                } else {
                    EndOp::Continue
                };
                s.ops.push((c.len(), op, OUT));
            }
            s.ops.push((0, EndOp::End, 5));
            variants.push(s);
            // the last piece with End and a large output (the shortcut)
            let mut s = chunked(level, 0, n, 100_000, OUT);
            s.ops.pop();
            s.ops.last_mut().unwrap().1 = EndOp::End;
            s.ops.last_mut().unwrap().2 = 4 << 20;
            variants.push(s);
            // one End call with everything, pledged
            let mut s = Script::new(level);
            s.pledged = Some(50_000);
            s.ops.push((50_000, EndOp::End, 1 << 20));
            variants.push(s);
            // pledged, tiny output buffers
            let mut s = chunked(level, 0, 20_000, 3000, 3);
            s.pledged = Some(20_000);
            variants.push(s);
            // empty frames
            variants.push(chunked(level, 0, 0, 1, OUT));
            variants.push(chunked(level, 0, 0, 1, 1));
            for mut s in variants {
                s.checksum = checksum;
                s.content_size = content_size;
                check(&what, &s, &data);
            }
        }
    }
    // a reused encoder, the flag switched between frames
    let mut cctx = CCtx::create();
    cctx.set_parameter(CParameter::CompressionLevel(3)).unwrap();
    let mut enc = StreamEncoder::new(3);
    for k in 0..6 {
        let n = [100_000, 0, 7, 300_000, 1000, 50_000][k];
        let mut s = chunked(3, 0, n, 33_333, OUT);
        s.checksum = k % 2 == 0;
        s.content_size = k % 3 != 1;
        if k == 3 {
            s.pledged = Some(n as u64);
        }
        let piece = &data[k * 1000..k * 1000 + n];
        let want = run_c(&s, piece, Some(&mut cctx));
        let got = run_native(&s, piece, Some(&mut enc));
        compare("reused, flags", &s, &got, &want);
    }
    // not in the middle of a frame
    let mut out = vec![0u8; 4096];
    enc.compress(b"abc", &mut out, EndOp::Continue).unwrap();
    assert_eq!(enc.set_checksum(true), Err(zstd_native::Error::StageWrong));
    assert_eq!(
        enc.set_content_size_flag(false),
        Err(zstd_native::Error::StageWrong)
    );
    // the io::Write encoder, with flushes
    for level in [1, 3, 9, 19] {
        let src = &data[..600_000];
        let mut enc = zstd_native::Encoder::new(Vec::new(), level);
        enc.set_checksum(true).unwrap();
        let mut c = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        c.include_checksum(true).unwrap();
        for (i, piece) in src.chunks(50_000).enumerate() {
            enc.write_all(piece).unwrap();
            c.write_all(piece).unwrap();
            if i % 4 == 1 {
                enc.flush().unwrap();
                c.flush().unwrap();
            }
        }
        let got = enc.finish().unwrap();
        let want = c.finish().unwrap();
        assert!(got == want, "io checksum L{level}: differs from libzstd");
        assert!(zstd_native::decompress(&got).unwrap() == src);
        // pledged, without the content size in the header
        let mut enc = zstd_native::Encoder::new(Vec::new(), level);
        enc.set_checksum(true).unwrap();
        enc.set_content_size_flag(false).unwrap();
        enc.set_pledged_src_size(Some(src.len() as u64)).unwrap();
        let mut c = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        c.include_checksum(true).unwrap();
        c.include_contentsize(false).unwrap();
        c.set_pledged_src_size(Some(src.len() as u64)).unwrap();
        enc.write_all(src).unwrap();
        c.write_all(src).unwrap();
        let got = enc.finish().unwrap();
        let want = c.finish().unwrap();
        assert!(got == want, "io pledged L{level}: differs from libzstd");
        assert_eq!(zstd_native::content_size(&got).unwrap(), None);
        let mut enc = zstd_native::Encoder::new(Vec::new(), level);
        enc.write_all(b"x").unwrap();
        assert!(enc.set_checksum(true).is_err());
    }
}
