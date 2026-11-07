//! Decoding with dictionaries, against libzstd: zstd-format (trained) and
//! raw content dictionaries, frames of every size class (one block, many
//! blocks), one-shot and streaming, a decompressor switching dictionaries
//! between frames (each switch makes the dictionary "cold": the first
//! block reads the dictionary's tables in place, with the cold-dictionary
//! loads and the prefetching sequence loop), clones of one dictionary
//! shared by threads, and the first block's entropy modes the frames
//! cover (the dictionary's tables used as they are, or partly replaced).

use zstd::zstd_safe::{DCtx, DDict};
use zstd_bench::{Rng, log_corpus, source_corpus};
use zstd_native::{Decompressor, Dictionary, Error, StreamDecoder};

struct Frame {
    /// Index in the dictionaries, `None` without one.
    dict: Option<usize>,
    data: Vec<u8>,
    comp: Vec<u8>,
}

/// Two trained dictionaries (different IDs and tables) and raw content.
fn dictionaries() -> Vec<Vec<u8>> {
    let log = log_corpus(2 << 20);
    let src = source_corpus();
    let log_samples: Vec<&[u8]> = log.chunks(2000).collect();
    let src_samples: Vec<&[u8]> = src.chunks(1500).take(1500).collect();
    vec![
        zstd::dict::from_samples(&log_samples, 32 << 10).unwrap(),
        zstd::dict::from_samples(&src_samples, 16 << 10).unwrap(),
        src[300_000..320_000].to_vec(),
    ]
}

/// Frames for every dictionary (and none) at a spread of levels and sizes:
/// libzstd keeps a dictionary's tables (repeat modes, treeless literals)
/// or replaces some of them.
fn frames(dicts: &[Vec<u8>]) -> Vec<Frame> {
    let log = log_corpus(4 << 20);
    let src = source_corpus();
    let (levels, sizes): (&[i32], &[usize]) = if cfg!(miri) {
        (&[3], &[700])
    } else {
        (
            &[-1, 1, 3, 6, 9, 13, 19],
            &[30, 500, 1500, 2500, 4096, 8000, 20_000, 300_000],
        )
    };
    let mut r = Rng::new(80);
    let mut v = Vec::new();
    for (di, dict) in dicts.iter().map(Some).chain([None]).enumerate() {
        for &level in levels {
            for &n in sizes {
                // Mostly data like the dictionary's (the log corpus for the
                // first, the source for the others), so its tables fit.
                let corpus = if (di == 0) == (r.below(4) != 0) {
                    &log
                } else {
                    &src
                };
                let at = r.below((corpus.len() - n) as u64) as usize;
                let data = corpus[at..at + n].to_vec();
                let comp = match dict {
                    Some(d) => zstd::bulk::Compressor::with_dictionary(level, d)
                        .unwrap()
                        .compress(&data)
                        .unwrap(),
                    None => zstd::bulk::compress(&data, level).unwrap(),
                };
                let dict = dict.map(|_| di);
                v.push(Frame { dict, data, comp });
            }
        }
    }
    v
}

/// libzstd's output for `f` (with a `DDict`, as the cold-dictionary case).
fn c_decode(f: &Frame, dicts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0u8; f.data.len() + 64];
    let mut dctx = DCtx::create();
    let n = match f.dict {
        Some(d) => {
            let ddict = DDict::create(&dicts[d]);
            dctx.decompress_using_ddict(&mut out[..], &f.comp, &ddict)
        }
        None => dctx.decompress(&mut out[..], &f.comp),
    }
    .unwrap();
    out.truncate(n);
    out
}

/// The first compressed block's literals type (with 1 or 4 streams) and
/// sequence modes, as a label.
fn first_block_modes(comp: &[u8]) -> Option<String> {
    let fhd = comp[4];
    let single = (fhd >> 5) & 1 == 1;
    let did = [0, 1, 2, 4][(fhd & 3) as usize];
    let fcs = [usize::from(single), 2, 4, 8][(fhd >> 6) as usize];
    let mut p = 5 + usize::from(!single) + did + fcs;
    loop {
        let bh = u32::from_le_bytes([comp[p], comp[p + 1], comp[p + 2], 0]);
        let (last, btype, bsize) = (bh & 1 == 1, (bh >> 1) & 3, (bh >> 3) as usize);
        p += 3;
        if btype == 2 {
            break;
        }
        if last {
            return None;
        }
        p += if btype == 1 { 1 } else { bsize };
    }
    let b = &comp[p..];
    let (lt, lhl) = (b[0] & 3, (b[0] >> 2) & 3);
    let (lh, csize) = if lt < 2 {
        let (lh, n) = match lhl {
            1 => (2, (u16::from_le_bytes([b[0], b[1]]) >> 4) as usize),
            3 => (3, (u32::from_le_bytes([b[0], b[1], b[2], 0]) >> 4) as usize),
            _ => (1, (b[0] >> 3) as usize),
        };
        (lh, if lt == 0 { n } else { 1 })
    } else {
        let h = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        match lhl {
            0 | 1 => (3, (h >> 14) & 0x3FF),
            2 => (4, h >> 18),
            _ => (5, (h >> 22) + ((b[4] as usize) << 10)),
        }
    };
    let lits = match (lt, lhl) {
        (3, 0) => "treeless-1",
        (3, _) => "treeless-4",
        (2, _) => "huffman",
        _ => "raw/rle",
    };
    let s = &b[lh + csize..];
    let (nb, ip) = match s[0] {
        0xFF => (1, 3),
        b if b > 0x7F => (1, 2),
        b => (usize::from(b), 1),
    };
    let seqs = if nb == 0 {
        "none"
    } else {
        match s[ip] >> 2 {
            0x3F => "repeat",
            m if m & 0x30 == 0x30 || m & 0xC == 0xC || m & 3 == 3 => "partial-repeat",
            _ => "new",
        }
    };
    Some(format!("{lits}/{seqs}"))
}

#[test]
fn matches_libzstd() {
    let dicts = dictionaries();
    let frames = frames(&dicts);
    let parsed: Vec<Dictionary> = dicts.iter().map(|d| Dictionary::new(d).unwrap()).collect();
    assert_ne!(parsed[0].id(), parsed[1].id());
    let mut modes = std::collections::BTreeSet::new();
    for (i, f) in frames.iter().enumerate() {
        assert!(c_decode(f, &dicts) == f.data, "libzstd, frame {i}");
        if f.dict.is_some_and(|d| d < 2)
            && let Some(m) = first_block_modes(&f.comp)
        {
            modes.insert(m);
        }
    }
    if !cfg!(miri) {
        // The ways a block can start from a dictionary's tables.
        for want in [
            "treeless-1/repeat",
            "treeless-4/repeat",
            "treeless-1/partial-repeat",
            "huffman/repeat",
        ] {
            assert!(modes.contains(want), "no {want} frame in {modes:?}");
        }
    }

    // One decompressor switching dictionaries (or keeping the same one)
    // between frames, in a random order; both sequence loops.
    let mut r = Rng::new(7);
    let mut dec = Decompressor::new();
    let mut installed = None;
    for round in 0..if cfg!(miri) { 2 } else { 4 } {
        zstd_native::__private::set_seq_loop(match round {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        });
        for _ in 0..frames.len() {
            let i = r.below(frames.len() as u64) as usize;
            let f = &frames[i];
            if installed != Some(f.dict) || r.below(4) == 0 {
                dec.set_dictionary(f.dict.map(|d| parsed[d].clone()));
                installed = Some(f.dict);
            }
            if r.below(2) == 0 {
                let got = dec.decompress(&f.comp).unwrap();
                assert!(got == f.data, "frame {i} round {round}");
            } else {
                let mut out = vec![0u8; f.data.len()];
                assert_eq!(dec.decompress_to_slice(&f.comp, &mut out), Ok(f.data.len()));
                assert!(out == f.data, "frame {i} round {round} (slice)");
            }
        }
    }
    zstd_native::__private::set_seq_loop(None);

    // A wrong dictionary (or none) for a frame with a dictionary ID.
    for f in &frames {
        if f.dict.is_some_and(|d| d < 2) {
            let other = parsed[1 - f.dict.unwrap()].clone();
            dec.set_dictionary(Some(other));
            assert_eq!(dec.decompress(&f.comp), Err(Error::DictionaryMismatch));
            dec.set_dictionary(None);
            assert_eq!(dec.decompress(&f.comp), Err(Error::DictionaryMismatch));
        }
    }
}

/// A dictionary parsed for each frame and dropped after it (libzstd's
/// `ZSTD_createDDict` + `ZSTD_freeDDict` per frame: the allocator often
/// gives the next one the same address, which counts as warm, as in
/// libzstd), or dropped only when the next one replaces it (always cold);
/// frames up to 300 KB, so first blocks with 128 KiB of literals.
#[test]
fn parsed_for_each_frame() {
    let dicts = dictionaries();
    let frames = frames(&dicts);
    let mut dec = Decompressor::new();
    for keep in [false, true] {
        for (i, f) in frames.iter().enumerate() {
            let Some(d) = f.dict else { continue };
            dec.set_dictionary(Some(Dictionary::new(&dicts[d]).unwrap()));
            let mut out = vec![0u8; f.data.len()];
            assert_eq!(dec.decompress_to_slice(&f.comp, &mut out), Ok(f.data.len()));
            assert!(out == f.data, "frame {i} keep {keep}");
            if !keep {
                dec.set_dictionary(None);
            }
        }
    }
}

/// Every frame of one dictionary concatenated, through a fresh decompressor
/// and a stream decoder (random input and output pieces).
#[test]
fn concatenated_frames_and_streaming() {
    let dicts = dictionaries();
    let frames = frames(&dicts);
    let mut r = Rng::new(11);
    for (d, dict) in dicts.iter().enumerate() {
        let mine: Vec<&Frame> = frames.iter().filter(|f| f.dict == Some(d)).collect();
        let comp: Vec<u8> = mine.iter().flat_map(|f| f.comp.iter().copied()).collect();
        let want: Vec<u8> = mine.iter().flat_map(|f| f.data.iter().copied()).collect();
        let parsed = Dictionary::new(dict).unwrap();
        let mut dec = Decompressor::with_dictionary(parsed.clone());
        assert!(dec.decompress(&comp).unwrap() == want, "dict {d}");

        let mut s = StreamDecoder::with_dictionary(parsed);
        let mut got = Vec::new();
        let mut out = vec![0u8; 1 << 17];
        let mut ip = 0;
        for step in 0.. {
            assert!(step < 1_000_000, "dict {d}: no progress");
            let inn = (1 + r.below(70_000) as usize).min(comp.len() - ip);
            let on = 1 + r.below(out.len() as u64) as usize;
            let p = s.decode(&comp[ip..ip + inn], &mut out[..on]).unwrap();
            ip += p.read;
            got.extend_from_slice(&out[..p.written]);
            if ip == comp.len() && p.written < on && p.at_frame_boundary {
                break;
            }
        }
        assert!(got == want, "dict {d} streamed");
    }
}

/// Clones of a dictionary share its tables: threads decoding at once.
#[test]
fn shared_between_threads() {
    let dicts = dictionaries();
    let frames = frames(&dicts);
    let parsed: Vec<Dictionary> = dicts.iter().map(|d| Dictionary::new(d).unwrap()).collect();
    std::thread::scope(|sc| {
        for t in 0..if cfg!(miri) { 2 } else { 4 } {
            let (frames, parsed) = (&frames, &parsed);
            sc.spawn(move || {
                let mut dec = Decompressor::new();
                for f in frames.iter().skip(t).step_by(2) {
                    dec.set_dictionary(f.dict.map(|d| parsed[d].clone()));
                    assert!(dec.decompress(&f.comp).unwrap() == f.data);
                }
            });
        }
    });
}
