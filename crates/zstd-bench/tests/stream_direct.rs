//! The streaming decoder's direct output (decoding into the caller's
//! buffer when it has room, issue #12) against C libzstd: output buffer
//! sizes from 1 byte to more than the stream, input in pieces of many
//! sizes, frames with and without a content size, windows much smaller and
//! larger than the output, multiple and skippable frames, dictionaries,
//! and the `std::io::Read` adapter's `read_to_end` / `read_to_string`,
//! including reader errors in the middle of a frame.

use std::io::{self, BufRead, Read, Write};

use zstd_bench::{Rng, c_compress, log_corpus, numeric_corpus, source_corpus};
use zstd_native::{Decoder, Dictionary, Error, StreamDecoder};

/// libzstd's streaming decoder (`ZSTD_decompressStream`): the reference.
fn c_decode(src: &[u8], dict: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    match dict {
        Some(d) => zstd::stream::read::Decoder::with_dictionary(src, d)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap(),
        None => zstd::stream::read::Decoder::with_buffer(src)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap(),
    };
    out
}

/// A libzstd streaming frame: window log `wlog`, content size when
/// `pledged`, with a checksum.
fn c_frame(
    data: &[u8],
    level: i32,
    wlog: Option<u32>,
    pledged: bool,
    dict: Option<&[u8]>,
) -> Vec<u8> {
    let mut enc = match dict {
        Some(d) => zstd::stream::write::Encoder::with_dictionary(Vec::new(), level, d).unwrap(),
        None => zstd::stream::write::Encoder::new(Vec::new(), level).unwrap(),
    };
    enc.include_checksum(true).unwrap();
    enc.include_contentsize(pledged).unwrap();
    if pledged {
        enc.set_pledged_src_size(Some(data.len() as u64)).unwrap();
    }
    if let Some(w) = wlog {
        enc.window_log(w).unwrap();
    }
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

fn skippable(n: usize) -> Vec<u8> {
    let mut v = 0x184D_2A53u32.to_le_bytes().to_vec();
    v.extend_from_slice(&(n as u32).to_le_bytes());
    v.extend((0..n).map(|i| i as u8));
    v
}

/// Decodes `src` with input pieces from `ins` and output buffers from
/// `outs` (cycled; 0 picks a random size up to the stream's length).
fn stream(
    dec: &mut StreamDecoder,
    src: &[u8],
    ins: &[usize],
    outs: &[usize],
    total: usize,
    rng: &mut Rng,
) -> Result<Vec<u8>, Error> {
    let mut got = Vec::new();
    let mut buf = Vec::new();
    let mut ip = 0;
    for i in 0.. {
        let n_in = ins[i % ins.len()].min(src.len() - ip);
        let n_out = match outs[i % outs.len()] {
            0 => 1 + rng.below(total as u64 + 1) as usize,
            n => n,
        };
        buf.resize(n_out, 0);
        let p = dec.decode(&src[ip..ip + n_in], &mut buf)?;
        ip += p.read;
        got.extend_from_slice(&buf[..p.written]);
        if ip == src.len() && p.read == 0 && p.written == 0 {
            return if p.at_frame_boundary {
                Ok(got)
            } else {
                Err(Error::Truncated)
            };
        }
    }
    unreachable!()
}

/// Every combination of input piece and output buffer size on `src`.
fn check_sizes(name: &str, src: &[u8], want: &[u8], dict: Option<&[u8]>, rng: &mut Rng) {
    let len = want.len();
    let out_sizes: Vec<Vec<usize>> = vec![
        vec![1],
        vec![7],
        vec![1000],
        vec![(128 << 10) - 1],
        vec![128 << 10],
        vec![1 << 20],
        vec![len.saturating_sub(1).max(1)],
        vec![len.max(1)],
        vec![len + 1],
        vec![2 * len + 100],
        // Big, then small, then big again: frames switch to the window
        // buffer mid-frame and start direct again.
        vec![len + 1, 3, 1 << 20, 1],
        vec![0],
    ];
    let in_sizes: &[&[usize]] = &[
        &[usize::MAX],
        &[4096],
        &[128 << 10],
        &[13, 100_000],
        &[1 << 20, 1],
    ];
    for ins in in_sizes {
        for outs in &out_sizes {
            // Byte-sized pieces are slow on big inputs: only a few.
            if outs == &[1] && ins.iter().all(|&n| n < 4096) && src.len() > 100_000 {
                continue;
            }
            let mut dec = match dict {
                Some(d) => StreamDecoder::with_dictionary(Dictionary::new(d).unwrap()),
                None => StreamDecoder::new(),
            };
            let got = stream(&mut dec, src, ins, outs, len, rng)
                .unwrap_or_else(|e| panic!("{name}: in {ins:?} out {outs:?}: {e}"));
            assert!(
                got == want,
                "{name}: in {ins:?} out {outs:?}: output differs"
            );
            // The decoder is reused: a second pass on the same state.
            let got = stream(&mut dec, src, ins, outs, len, rng).unwrap();
            assert!(
                got == want,
                "{name}: in {ins:?} out {outs:?}: second pass differs"
            );
        }
    }
    // `read_to_end` on whole input and through a small reader buffer,
    // `read_to_string` where the data is text.
    fn decoder<'a>(
        r: Box<dyn BufRead + 'a>,
        dict: Option<&[u8]>,
    ) -> Decoder<Box<dyn BufRead + 'a>> {
        match dict {
            Some(d) => Decoder::with_dictionary(r, Dictionary::new(d).unwrap()),
            None => Decoder::with_buffer(r),
        }
    }
    let mk = |r| decoder(r, dict);
    for cap in [usize::MAX, 1, 1000, 128 << 10] {
        let r: Box<dyn BufRead> = if cap == usize::MAX {
            Box::new(src)
        } else {
            Box::new(io::BufReader::with_capacity(cap, src))
        };
        let mut v = b"existing".to_vec();
        mk(r).read_to_end(&mut v).unwrap();
        assert!(v[8..] == *want, "{name}: read_to_end through {cap}");
    }
    if let Ok(text) = std::str::from_utf8(want) {
        let mut s = String::from("x");
        mk(Box::new(src)).read_to_string(&mut s).unwrap();
        assert!(s[1..] == *text, "{name}: read_to_string");
    }
    // Plain reads with a large buffer.
    let mut d = mk(Box::new(io::BufReader::with_capacity(128 << 10, src)));
    let mut got = Vec::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = d.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert!(got == want, "{name}: 1 MiB reads");
}

#[test]
fn frames_against_libzstd() {
    let mut rng = Rng::new(12);
    let src = source_corpus();
    let src = &src[..src.len().min(1 << 20)];
    let log = log_corpus(1 << 20);
    for (data, dname) in [(src, "source"), (&log[..], "log")] {
        for level in [1, 3, 19] {
            let one = c_compress(data, level);
            assert!(c_decode(&one, None) == data);
            check_sizes(
                &format!("{dname} L{level} one-shot"),
                &one,
                data,
                None,
                &mut rng,
            );
        }
        // Windows far smaller than the frame (the window buffer wraps many
        // times), with and without the content size, and one larger than
        // the frame.
        for (wlog, level, pledged) in [
            (10, 3, true),
            (12, 1, false),
            (17, 3, true),
            (17, 9, false),
            (23, 3, false),
        ] {
            let f = c_frame(data, level, Some(wlog), pledged, None);
            assert!(c_decode(&f, None) == data);
            check_sizes(
                &format!("{dname} wlog {wlog} L{level} fcs {pledged}"),
                &f,
                data,
                None,
                &mut rng,
            );
        }
    }
}

#[test]
fn multiple_and_skippable_frames() {
    let mut rng = Rng::new(13);
    let a = numeric_corpus(300_000);
    let b = log_corpus(500_000);
    let mut src = skippable(0);
    src.extend(c_compress(&a, 3));
    src.extend(skippable(17));
    src.extend(c_frame(&b, 5, Some(12), false, None));
    src.extend(c_frame(&b[..1000], 1, None, true, None));
    src.extend(c_compress(&[], 3)); // empty frame
    src.extend(c_frame(&a[..70_000], 19, Some(10), true, None));
    src.extend(skippable(5));
    let want = c_decode(&src, None);
    let mut expect = a.clone();
    expect.extend_from_slice(&b);
    expect.extend_from_slice(&b[..1000]);
    expect.extend_from_slice(&a[..70_000]);
    assert!(want == expect);
    check_sizes("multi-frame", &src, &want, None, &mut rng);
}

#[test]
fn dictionaries() {
    let mut rng = Rng::new(14);
    let corpus = log_corpus(2 << 20);
    let samples: Vec<&[u8]> = corpus.chunks(3000).collect();
    let dict = zstd::dict::from_samples(&samples, 32 << 10).unwrap();
    let raw = &corpus[..20_000]; // raw content dictionary
    for d in [&dict[..], raw] {
        let mut src = Vec::new();
        let mut want = Vec::new();
        for (i, s) in samples.iter().skip(50).take(40).enumerate() {
            // Small windows: the dictionary is in reach only at the start.
            let wlog = [None, Some(10), Some(12)][i % 3];
            src.extend(c_frame(s, 1 + (i % 7) as i32, wlog, i % 2 == 0, Some(d)));
            want.extend_from_slice(s);
        }
        // A long frame with a window smaller than the dictionary.
        let long = &corpus[1 << 20..(1 << 20) + 300_000];
        src.extend(c_frame(long, 3, Some(14), true, Some(d)));
        want.extend_from_slice(long);
        assert!(c_decode(&src, Some(d)) == want);
        check_sizes("dictionary", &src, &want, Some(d), &mut rng);
    }
}

/// A reader that fails (`WouldBlock`, `Interrupted`) now and then and
/// hands out input in small pieces.
struct Flaky<'a> {
    data: &'a [u8],
    pos: usize,
    rng: Rng,
}

impl Read for Flaky<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.fill_buf()?.len().min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl BufRead for Flaky<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self.rng.below(8) {
            0 => return Err(io::ErrorKind::WouldBlock.into()),
            1 => return Err(io::ErrorKind::Interrupted.into()),
            _ => {}
        }
        let n = (1 + self.rng.below(5_000) as usize).min(self.data.len() - self.pos);
        Ok(&self.data[self.pos..self.pos + n])
    }
    fn consume(&mut self, n: usize) {
        self.pos += n;
    }
}

/// Reader errors in the middle of a direct frame: `read_to_end` returns
/// them with the output so far, and decoding goes on from there with
/// `read_to_end` or `read`.
#[test]
fn read_to_end_reader_errors() {
    let data = log_corpus(1 << 20);
    for (wlog, pledged) in [(10, true), (12, false), (20, true), (23, false)] {
        let mut src = c_frame(&data, 3, Some(wlog), pledged, None);
        src.extend(c_compress(&data[..200_000], 1));
        let mut want = data.clone();
        want.extend_from_slice(&data[..200_000]);
        for seed in 0..8 {
            let mut d = Decoder::with_buffer(Flaky {
                data: &src,
                pos: 0,
                rng: Rng::new(seed),
            });
            let mut got = Vec::new();
            let mut buf = [0u8; 5000];
            let mut errors = 0;
            loop {
                // `read_to_end` retries `Interrupted` itself.
                match d.read_to_end(&mut got) {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        errors += 1;
                        // A plain read in between, every other time: the
                        // frame goes on from the window buffer.
                        if errors % 2 == 0 {
                            match d.read(&mut buf) {
                                Ok(n) => got.extend_from_slice(&buf[..n]),
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                                    ) => {}
                                Err(e) => panic!("wlog {wlog} seed {seed}: {e}"),
                            }
                        }
                    }
                    Err(e) => panic!("wlog {wlog} seed {seed}: {e}"),
                }
            }
            assert!(errors > 3, "wlog {wlog} seed {seed}: {errors} errors");
            assert!(
                got == want,
                "wlog {wlog} fcs {pledged} seed {seed}: output differs"
            );
        }
    }
}

/// Truncated and corrupt streams fail the same way through
/// `read_to_end` (direct into the `Vec`) and small reads (window buffer).
#[test]
fn read_to_end_errors() {
    let data = log_corpus(300_000);
    let c = c_frame(&data, 3, Some(12), true, None);
    for cut in (1..c.len()).step_by(c.len() / 50 + 1) {
        let e = Decoder::new(&c[..cut])
            .read_to_end(&mut Vec::new())
            .unwrap_err();
        assert!(
            matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ),
            "cut {cut}: {e:?}"
        );
    }
    let mut bad = c.clone();
    let n = bad.len();
    bad[n - 2] ^= 0x40; // checksum
    let e = Decoder::new(&bad[..])
        .read_to_end(&mut Vec::new())
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    // A failed decoder stays failed.
    let mut d = Decoder::new(&bad[..]);
    assert!(d.read_to_end(&mut Vec::new()).is_err());
    assert!(d.read_to_end(&mut Vec::new()).is_err());
    // Invalid UTF-8: an error, the string unchanged.
    let bin = c_compress(&[b'a', 0xFF, b'b'], 1);
    let mut s = String::from("keep");
    assert!(Decoder::new(&bin[..]).read_to_string(&mut s).is_err());
    assert_eq!(s, "keep");
}
