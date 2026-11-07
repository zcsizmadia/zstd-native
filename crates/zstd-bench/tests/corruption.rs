//! Corrupt input must never panic or misbehave, and anything zstd-native
//! accepts, C libzstd must accept with the same output (zstd-native is at
//! least as strict; libzstd's fast Huffman path doesn't check that streams
//! are fully consumed, so it accepts some inputs zstd-native rejects).
//!
//! Run with debug assertions (`--profile ci`) so the `unsafe` hot paths'
//! assertions are checked. `ZSTD_NATIVE_CORRUPTION_ITERS` scales it up.
//!
//! The tests switch the process-wide sequence loop choice
//! (`set_seq_loop`), so they run one at a time.

use std::sync::{Mutex, MutexGuard};

use zstd_bench::{Rng, c_compress, log_corpus, numeric_corpus, source_corpus};

static SERIAL: Mutex<()> = Mutex::new(());

/// Held by every test: each sets the sequence loop for its own decodes.
/// Starts from the automatic choice (a failed test may have left one set).
fn serial() -> MutexGuard<'static, ()> {
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    zstd_native::__private::set_seq_loop(None);
    g
}

fn c_decompress(src: &[u8], cap: usize) -> Option<Vec<u8>> {
    zstd::bulk::Decompressor::new()
        .unwrap()
        .decompress(src, cap)
        .ok()
}

#[test]
fn mutated_frames() {
    let _serial = serial();
    let iters: usize = std::env::var("ZSTD_NATIVE_CORRUPTION_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let src = source_corpus();
    let inputs: Vec<Vec<u8>> = vec![
        src[..20_000].to_vec(),
        src[50_000..51_000].to_vec(),
        log_corpus(30_000),
        numeric_corpus(30_000),
        b"tiny".to_vec(),
    ];
    let mut frames = Vec::new();
    for d in &inputs {
        for level in [1, 3, 9, 19] {
            frames.push((d.len(), c_compress(d, level)));
        }
    }
    let mut r = Rng::new(2024);
    let mut accepted = 0;
    let mut dec = zstd_native::Decompressor::new();
    for i in 0..iters {
        let (orig_len, f) = &frames[i % frames.len()];
        let mut m = f.clone();
        match r.below(4) {
            0 => {
                // Flip a few bits.
                for _ in 0..1 + r.below(4) {
                    let p = r.below(m.len() as u64) as usize;
                    m[p] ^= 1 << r.below(8);
                }
            }
            1 => {
                // Overwrite a byte (skip the magic number half the time).
                let p = r.below(m.len() as u64) as usize;
                m[p] = r.next_u64() as u8;
            }
            2 => {
                // Truncate.
                let n = r.below(m.len() as u64) as usize;
                m.truncate(n);
            }
            _ => {
                // Duplicate a slice inside the frame.
                let a = r.below(m.len() as u64) as usize;
                let b = (a + r.below(64) as usize).min(m.len());
                let piece = m[a..b].to_vec();
                let p = r.below(m.len() as u64) as usize;
                m.splice(p..p, piece);
            }
        }
        // Alternate the asm and Rust Huffman loops (where the CPU has both),
        // and the fused and the prefetching sequence loops.
        zstd_native::__private::set_asm(Some(i % 2 == 0));
        zstd_native::__private::set_seq_loop(Some(i / 2 % 2 == 0));
        let native = dec.decompress(&m);
        let mut buf = vec![0u8; orig_len + 100];
        let native_slice = dec.decompress_to_slice(&m, &mut buf);
        if let Ok(v) = &native {
            accepted += 1;
            let c = c_decompress(&m, v.len().max(1) + 1000);
            assert_eq!(
                c.as_ref(),
                Some(v),
                "iteration {i}: zstd-native accepted what libzstd rejects or decodes differently"
            );
            if let Ok(n) = native_slice {
                assert_eq!(
                    &buf[..n],
                    &v[..],
                    "iteration {i}: slice and vec decoders differ"
                );
            }
        }
    }
    zstd_native::__private::set_asm(None);
    zstd_native::__private::set_seq_loop(None);
    eprintln!("{accepted} of {iters} mutated inputs decoded");
}

/// The streaming decoder on mutated input, fed in random pieces: never a
/// panic, and anything it accepts, libzstd's streaming decoder accepts
/// with the same output.
#[test]
fn mutated_streams() {
    use std::io::Write;
    let _serial = serial();
    let iters: usize = std::env::var("ZSTD_NATIVE_CORRUPTION_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let data = log_corpus(200_000);
    let mut frames = Vec::new();
    for (level, wlog) in [(1, 10), (3, 12), (9, 17), (19, 15)] {
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        enc.window_log(wlog).unwrap();
        enc.include_checksum(wlog % 2 == 0).unwrap();
        enc.write_all(&data[..(50_000 * level as usize).min(data.len())])
            .unwrap();
        frames.push(enc.finish().unwrap());
    }
    let mut r = Rng::new(31337);
    let mut accepted = 0;
    for i in 0..iters {
        let mut m = frames[i % frames.len()].clone();
        for _ in 0..1 + r.below(3) {
            let p = r.below(m.len() as u64) as usize;
            m[p] ^= 1 << r.below(8);
        }
        if r.below(4) == 0 {
            let n = r.below(m.len() as u64) as usize;
            m.truncate(n);
        }
        zstd_native::__private::set_asm(Some(i % 2 == 0));
        zstd_native::__private::set_seq_loop(Some(i / 2 % 2 == 0));
        let mut dec = zstd_native::StreamDecoder::new();
        let mut out = Vec::new();
        let mut buf = vec![0u8; 1 + r.below(70_000) as usize];
        let mut ip = 0;
        let ok = loop {
            let n = (1 + r.below(50_000) as usize).min(m.len() - ip);
            match dec.decode(&m[ip..ip + n], &mut buf) {
                Err(_) => break false,
                Ok(p) => {
                    ip += p.read;
                    out.extend_from_slice(&buf[..p.written]);
                    if ip == m.len() && p.read == 0 && p.written == 0 {
                        break p.at_frame_boundary;
                    }
                }
            }
        };
        // Empty input is zero frames: empty output, as libzstd itself (the
        // `zstd` crate's reader adds an error for streams with no frame).
        if ok && !m.is_empty() {
            accepted += 1;
            let c = zstd::stream::decode_all(&m[..]).ok();
            assert_eq!(
                c.as_ref(),
                Some(&out),
                "iteration {i}: accepted what libzstd's stream decoder rejects or decodes differently (input {} bytes: {:02x?})",
                m.len(),
                &m[..m.len().min(24)]
            );
        }
    }
    zstd_native::__private::set_asm(None);
    zstd_native::__private::set_seq_loop(None);
    eprintln!("{accepted} of {iters} mutated streams decoded");
}

/// Mutated streams through the streaming decoder's direct output (one
/// output buffer that holds everything, and `io::Read::read_to_end`) and
/// through its window buffer (small outputs): the same verdict and the
/// same output, and anything accepted, libzstd accepts with that output.
#[test]
fn mutated_streams_direct() {
    use std::io::{Read, Write};
    let _serial = serial();
    let iters: usize = std::env::var("ZSTD_NATIVE_CORRUPTION_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let data = log_corpus(200_000);
    let mut frames = Vec::new();
    for (level, wlog, pledged) in [
        (1, 10, true),
        (3, 12, false),
        (9, 17, true),
        (19, 15, false),
    ] {
        let part = &data[..(50_000 * level as usize).min(data.len())];
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        enc.window_log(wlog).unwrap();
        enc.include_checksum(wlog % 2 == 0).unwrap();
        enc.include_contentsize(pledged).unwrap();
        if pledged {
            enc.set_pledged_src_size(Some(part.len() as u64)).unwrap();
        }
        enc.write_all(part).unwrap();
        frames.push(enc.finish().unwrap());
    }
    frames.push(c_compress(&data[..30_000], 3));
    let decode = |m: &[u8], out_max: usize, r: &mut Rng| -> (bool, Vec<u8>) {
        let mut dec = zstd_native::StreamDecoder::new();
        let mut out = Vec::new();
        let mut buf = vec![0u8; out_max];
        let mut ip = 0;
        let ok = loop {
            let n = (1 + r.below(50_000) as usize).min(m.len() - ip);
            let o = 1 + r.below(out_max as u64) as usize;
            match dec.decode(&m[ip..ip + n], &mut buf[..o]) {
                Err(_) => break false,
                Ok(p) => {
                    ip += p.read;
                    out.extend_from_slice(&buf[..p.written]);
                    if ip == m.len() && p.read == 0 && p.written == 0 {
                        break p.at_frame_boundary;
                    }
                }
            }
        };
        (ok, out)
    };
    let mut r = Rng::new(4242);
    // More than any mutated stream here decodes to.
    let mut big = vec![0u8; 16 << 20];
    let mut accepted = 0;
    for i in 0..iters {
        let mut m = frames[i % frames.len()].clone();
        for _ in 0..1 + r.below(3) {
            let p = r.below(m.len() as u64) as usize;
            m[p] ^= 1 << r.below(8);
        }
        if r.below(4) == 0 {
            let n = r.below(m.len() as u64) as usize;
            m.truncate(n);
        }
        zstd_native::__private::set_asm(Some(i % 2 == 0));
        zstd_native::__private::set_seq_loop(Some(i / 2 % 2 == 0));
        // Direct: one call with all the input and room for everything.
        let mut dec = zstd_native::StreamDecoder::new();
        let direct = match dec.decode(&m, &mut big) {
            Ok(p) if p.read == m.len() && p.at_frame_boundary => Some(big[..p.written].to_vec()),
            _ => None,
        };
        // Window buffer: small outputs.
        let (ok, small) = decode(&m, 300, &mut r);
        let buffered = ok.then_some(small);
        // Mixed: random outputs up to more than the frame.
        let (ok, mixed) = decode(&m, 400_000, &mut r);
        let mixed = ok.then_some(mixed);
        let mut v = Vec::new();
        let rte = zstd_native::Decoder::with_buffer(std::io::BufReader::with_capacity(
            1 + r.below(70_000) as usize,
            &m[..],
        ))
        .read_to_end(&mut v)
        .ok()
        .map(|_| v);
        assert!(
            direct == buffered && mixed == buffered && rte == buffered,
            "iteration {i}: direct {:?}, buffered {:?}, mixed {:?}, read_to_end {:?}",
            direct.as_ref().map(Vec::len),
            buffered.as_ref().map(Vec::len),
            mixed.as_ref().map(Vec::len),
            rte.as_ref().map(Vec::len)
        );
        if let Some(out) = &buffered
            && !m.is_empty()
        {
            accepted += 1;
            let c = zstd::stream::decode_all(&m[..]).ok();
            assert_eq!(c.as_ref(), Some(out), "iteration {i}: libzstd differs");
        }
    }
    zstd_native::__private::set_asm(None);
    zstd_native::__private::set_seq_loop(None);
    eprintln!("{accepted} of {iters} mutated streams decoded");
}

/// Mutated frames compressed with dictionaries (trained and raw), decoded
/// one-shot (the dictionary installed again before some frames: its
/// tables then serve the first block in place, after the cold-dictionary
/// loads) and streamed: never a panic, and anything accepted, libzstd
/// accepts with the same output.
#[test]
fn mutated_dictionary_frames() {
    let _serial = serial();
    let iters: usize = std::env::var("ZSTD_NATIVE_CORRUPTION_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let log = log_corpus(2 << 20);
    let src = source_corpus();
    let samples: Vec<&[u8]> = log.chunks(2000).collect();
    let dicts = [
        zstd::dict::from_samples(&samples, 32 << 10).unwrap(),
        src[200_000..230_000].to_vec(),
    ];
    let parsed: Vec<_> = dicts
        .iter()
        .map(|d| zstd_native::Dictionary::new(d).unwrap())
        .collect();
    let ddicts: Vec<_> = dicts
        .iter()
        .map(|d| zstd::zstd_safe::DDict::create(d))
        .collect();
    let mut frames = Vec::new();
    for (d, dict) in dicts.iter().enumerate() {
        for (n, at) in [(300, 1_000_000), (4096, 1_500_000), (150_000, 1_700_000)] {
            for level in [1, 3, 9, 19] {
                let data = &log[at..at + n];
                let mut c = zstd::bulk::Compressor::with_dictionary(level, dict).unwrap();
                frames.push((d, n, c.compress(data).unwrap()));
            }
        }
    }
    let mut r = Rng::new(80);
    let mut accepted = 0;
    let mut dec = zstd_native::Decompressor::new();
    let mut dctx = zstd::zstd_safe::DCtx::create();
    let mut installed = None;
    for i in 0..iters {
        let (d, orig_len, f) = &frames[i % frames.len()];
        let mut m = f.clone();
        for _ in 0..1 + r.below(3) {
            let p = r.below(m.len() as u64) as usize;
            m[p] ^= 1 << r.below(8);
        }
        if r.below(4) == 0 {
            let n = r.below(m.len() as u64) as usize;
            m.truncate(n);
        }
        zstd_native::__private::set_asm(Some(i % 2 == 0));
        zstd_native::__private::set_seq_loop(match i / 2 % 3 {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        });
        // Installed again (cold), a new decompressor (cold), or kept (warm:
        // the dictionary's double-symbol table) when it is the same one.
        match r.below(3) {
            0 => dec.set_dictionary(Some(parsed[*d].clone())),
            1 => dec = zstd_native::Decompressor::with_dictionary(parsed[*d].clone()),
            _ if installed == Some(*d) => {}
            _ => dec.set_dictionary(Some(parsed[*d].clone())),
        }
        installed = Some(*d);
        let native = dec.decompress(&m);
        let mut buf = vec![0u8; orig_len + 100];
        let native_slice = dec.decompress_to_slice(&m, &mut buf);
        let mut s = zstd_native::StreamDecoder::with_dictionary(parsed[*d].clone());
        let mut streamed = Vec::new();
        let mut out = vec![0u8; 1 + r.below(70_000) as usize];
        let mut ip = 0;
        let stream_ok = loop {
            let n = (1 + r.below(50_000) as usize).min(m.len() - ip);
            match s.decode(&m[ip..ip + n], &mut out) {
                Err(_) => break false,
                Ok(p) => {
                    ip += p.read;
                    streamed.extend_from_slice(&out[..p.written]);
                    if ip == m.len() && p.read == 0 && p.written == 0 {
                        break p.at_frame_boundary;
                    }
                }
            }
        };
        let mut c_decode = |cap: usize| {
            let mut c_out = vec![0u8; cap];
            dctx.decompress_using_ddict(&mut c_out[..], &m, &ddicts[*d])
                .ok()
                .map(|n| c_out[..n].to_vec())
        };
        if let Ok(v) = &native {
            accepted += 1;
            assert_eq!(
                c_decode(v.len().max(1) + 1000).as_ref(),
                Some(v),
                "iteration {i}: zstd-native accepted what libzstd rejects or decodes differently"
            );
            if let Ok(n) = native_slice {
                assert_eq!(
                    &buf[..n],
                    &v[..],
                    "iteration {i}: slice and vec decoders differ"
                );
            }
        }
        if stream_ok && !m.is_empty() {
            assert_eq!(
                c_decode(streamed.len().max(1) + 1000).as_ref(),
                Some(&streamed),
                "iteration {i}: streamed, accepted what libzstd rejects or decodes differently"
            );
        }
    }
    zstd_native::__private::set_asm(None);
    zstd_native::__private::set_seq_loop(None);
    eprintln!("{accepted} of {iters} mutated dictionary frames decoded");
}
