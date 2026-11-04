//! Inputs found by the fuzz targets (`fuzz/`), checked against C libzstd
//! through every decoding API, with and without a dictionary.

use std::io::Read;

use zstd::zstd_safe::{DCtx, InBuffer, OutBuffer};
use zstd_native::{Decoder, Decompressor, Dictionary, StreamDecoder};

/// libzstd's one-shot decoder (`ZSTD_decompressDCtx`).
fn c_one_shot(src: &[u8], dict: Option<&[u8]>) -> Result<Vec<u8>, &'static str> {
    let mut d = DCtx::create();
    if let Some(dict) = dict {
        d.load_dictionary(dict)
            .map_err(zstd::zstd_safe::get_error_name)?;
    }
    let mut out = Vec::with_capacity(1 << 20);
    d.decompress(&mut out, src)
        .map_err(zstd::zstd_safe::get_error_name)?;
    Ok(out)
}

/// libzstd's streaming decoder (`ZSTD_decompressStream`) fed `piece` bytes
/// at a time: `Ok` once a frame ended with all input consumed. With the
/// whole frame in one piece and room for its content size, libzstd takes
/// its one-shot shortcut instead of the streaming stages.
fn c_stream(src: &[u8], dict: Option<&[u8]>, piece: usize) -> Result<Vec<u8>, &'static str> {
    let mut d = DCtx::create();
    if let Some(dict) = dict {
        d.load_dictionary(dict)
            .map_err(zstd::zstd_safe::get_error_name)?;
    }
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 17];
    let mut last = 1;
    for chunk in src.chunks(piece) {
        let mut inb = InBuffer::around(chunk);
        loop {
            let mut outb = OutBuffer::around(&mut buf[..]);
            last = d
                .decompress_stream(&mut outb, &mut inb)
                .map_err(zstd::zstd_safe::get_error_name)?;
            let w = outb.pos();
            out.extend_from_slice(&buf[..w]);
            if inb.pos() == chunk.len() && w < buf.len() {
                break;
            }
        }
    }
    if last == 0 {
        Ok(out)
    } else {
        Err("incomplete")
    }
}

/// zstd-native's streaming decoder fed `piece` bytes at a time.
fn native_stream(src: &[u8], dict: Option<&[u8]>, piece: usize) -> Result<Vec<u8>, String> {
    let mut d = match dict {
        Some(b) => StreamDecoder::with_dictionary(Dictionary::new(b).unwrap()),
        None => StreamDecoder::new(),
    };
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 17];
    for chunk in src.chunks(piece) {
        let mut input = chunk;
        loop {
            let p = d.decode(input, &mut buf).map_err(|e| format!("{e:?}"))?;
            input = &input[p.read..];
            out.extend_from_slice(&buf[..p.written]);
            if input.is_empty() && p.written < buf.len() {
                break;
            }
        }
    }
    if d.at_frame_boundary() {
        Ok(out)
    } else {
        Err("incomplete".into())
    }
}

/// zstd-native's `io::Read` decoder.
fn native_read(src: &[u8], dict: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let r = match dict {
        Some(b) => Decoder::with_dictionary(src, Dictionary::new(b).unwrap()).read_to_end(&mut out),
        None => Decoder::new(src).read_to_end(&mut out),
    };
    r.map(|_| out).map_err(|e| e.to_string())
}

/// zstd-native's one-shot decoder.
fn native_one_shot(src: &[u8], dict: Option<&[u8]>) -> Result<Vec<u8>, String> {
    match dict {
        Some(b) => Decompressor::with_dictionary(Dictionary::new(b).unwrap()).decompress(src),
        None => zstd_native::decompress(src),
    }
    .map_err(|e| format!("{e:?}"))
}

/// What every API does with `src`: zstd-native's streaming and `io::Read`
/// decoders agree with libzstd's streaming stages (`ZSTD_decompressContinue`),
/// and its one-shot decoder with libzstd's (`ZSTD_decompressFrame`).
/// zstd-native may be stricter, never more lenient.
fn check(src: &[u8], stream_ok: bool, one_shot_ok: bool) {
    for dict in [None, Some(&[255u8][..]), Some(&b"0123456789abcdef"[..])] {
        let what = |api: &str| format!("{api}, dictionary {dict:?}, frame {src:02x?}");
        // libzstd, byte by byte (the streaming stages) and whole (the
        // one-shot shortcut).
        let c_bytes = c_stream(src, dict, 1);
        let c_whole = c_stream(src, dict, src.len());
        let c_one = c_one_shot(src, dict);
        assert_eq!(
            c_bytes.is_ok(),
            stream_ok,
            "{}: {c_bytes:?}",
            what("libzstd stream")
        );
        assert_eq!(
            c_one.is_ok(),
            one_shot_ok,
            "{}: {c_one:?}",
            what("libzstd one-shot")
        );
        for piece in [1, 2, src.len()] {
            let n = native_stream(src, dict, piece);
            assert_eq!(
                n.is_ok(),
                stream_ok,
                "{}: {n:?}",
                what(&format!("stream {piece}"))
            );
            if let Ok(n) = &n {
                assert_eq!(Ok(n), c_bytes.as_ref(), "{}", what("stream"));
                assert_eq!(Ok(n), c_whole.as_ref(), "{}", what("stream"));
            }
        }
        let r = native_read(src, dict);
        assert_eq!(r.is_ok(), stream_ok, "{}: {r:?}", what("io::Read"));
        let o = native_one_shot(src, dict);
        assert_eq!(o.is_ok(), one_shot_ok, "{}: {o:?}", what("one-shot"));
        if let Ok(o) = &o {
            assert_eq!(Ok(o), c_one.as_ref(), "{}", what("one-shot"));
        }
    }
}

const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

fn frame(header: &[u8], blocks: &[u8]) -> Vec<u8> {
    [&MAGIC[..], header, blocks].concat()
}

/// `stream_decode`: a single-segment frame with content size 0 has a
/// window and so a largest block (`blockSizeMax`) of 0. libzstd's streaming
/// stages reject any block whose compressed size exceeds that, and an RLE
/// block's compressed size is 1 even when it regenerates 0 bytes
/// (`ZSTD_decompressContinue`, zstd_decompress.c:1315). The one-shot
/// decoder has no such check.
#[test]
fn empty_rle_block_in_empty_single_segment_frame() {
    // Single segment, 1-byte content size 0; RLE of 0 bytes (not last),
    // then an empty raw last block: the fuzz input.
    check(
        &frame(&[0x20, 0x00], &[0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]),
        false,
        true,
    );
    // The RLE block of 0 bytes as the last block.
    check(
        &frame(&[0x20, 0x00], &[0x03, 0x00, 0x00, 0x5a]),
        false,
        true,
    );
    // With a checksum (of no bytes: XXH64("") = 0xef46db3751d8e999).
    check(
        &frame(
            &[0x24, 0x00],
            &[0x03, 0x00, 0x00, 0x5a, 0x99, 0xe9, 0xd8, 0x51],
        ),
        false,
        true,
    );
    // An empty raw block is fine everywhere.
    check(&frame(&[0x20, 0x00], &[0x01, 0x00, 0x00]), true, true);
    // Content size 1: the RLE block's 1 byte fits the 1-byte window.
    check(&frame(&[0x20, 0x01], &[0x0b, 0x00, 0x00, 0x5a]), true, true);
    // An RLE block of 0 bytes in a frame with a window.
    check(
        &frame(
            &[0x20, 0x01],
            &[0x02, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x5a],
        ),
        true,
        true,
    );
    check(
        &frame(&[0x00, 0x00], &[0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]),
        true,
        true,
    );
}

/// Neighbours of the above where libzstd's streaming stages and its
/// one-shot decoder disagree (upstream meta/zstd #4880, #4882, #3508,
/// #4832): whatever zstd-native's streaming or one-shot decoder accepts,
/// the matching libzstd decoder accepts with the same output.
#[test]
fn never_more_lenient_than_libzstd() {
    let frames: &[Vec<u8>] = &[
        // Content size 1,000,000, 5 bytes, then an empty last block:
        // libzstd's streaming stages skip the content size check after an
        // empty last block (#4880).
        frame(
            &[0xa0, 0x40, 0x42, 0x0f, 0x00],
            &[
                0x28, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o', 0x01, 0x00, 0x00,
            ],
        ),
        // The same with a checksum after the empty last block.
        frame(
            &[0xa4, 0x40, 0x42, 0x0f, 0x00],
            &[
                0x28, 0x00, 0x00, b'h', b'e', b'l', b'l', b'o', 0x01, 0x00, 0x00, 0, 0, 0, 0,
            ],
        ),
        // A compressed block of 0 bytes: an empty block to the streaming
        // stages, too short for the one-shot decoder (#3508).
        frame(&[0x00, 0x00], &[0x05, 0x00, 0x00]),
        frame(&[0x00, 0x00], &[0x04, 0x00, 0x00, 0x01, 0x00, 0x00]),
        // RLE and raw blocks larger than a 1 KiB window: only the
        // streaming stages check (#4832).
        frame(&[0x00, 0x00], &[0x83, 0x3e, 0x00, 0x5a]),
        frame(&[0x00, 0x00], &[0x01, 0x20, 0x00]) // 1024 bytes: fits
            .into_iter()
            .chain(std::iter::repeat_n(7u8, 1024))
            .collect(),
        frame(&[0x00, 0x00], &[0x09, 0x20, 0x00])
            .into_iter()
            .chain(std::iter::repeat_n(7u8, 1025))
            .collect(),
        // A compressed block one byte over a 3-byte window (content size 3).
        frame(
            &[0x20, 0x03],
            &[0x2d, 0x00, 0x00, 0x18, 0x61, 0x62, 0x63, 0x00],
        ),
        // Skippable frames with sizes the one-shot decoder rejects (#4882).
        vec![0x50, 0x2a, 0x4d, 0x18, 0xf8, 0xff, 0xff, 0xff],
        [
            &[0x50, 0x2a, 0x4d, 0x18, 0xff, 0xff, 0xff, 0xff][..],
            &[0; 16],
        ]
        .concat(),
    ];
    for src in frames {
        for dict in [None, Some(&[255u8][..])] {
            let what = |api: &str| format!("{api}, dictionary {dict:?}, frame {src:02x?}");
            let c_bytes = c_stream(src, dict, 1);
            let c_one = c_one_shot(src, dict);
            for piece in [1, 2, src.len()] {
                if let Ok(n) = native_stream(src, dict, piece) {
                    assert_eq!(
                        Ok(&n),
                        c_bytes.as_ref(),
                        "{}",
                        what(&format!("stream {piece}"))
                    );
                }
            }
            if let Ok(r) = native_read(src, dict) {
                assert_eq!(Ok(&r), c_bytes.as_ref(), "{}", what("io::Read"));
            }
            if let Ok(o) = native_one_shot(src, dict) {
                assert_eq!(Ok(&o), c_one.as_ref(), "{}", what("one-shot"));
            }
        }
    }
}
