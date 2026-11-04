//! The streaming decoder on arbitrary bytes, fed in arbitrary pieces with
//! arbitrary output sizes, with a dictionary from arbitrary bytes and an
//! arbitrary window limit: never crashes or hangs, and agrees with
//! libzstd's `ZSTD_decompressStream`, the one-shot decoder and the
//! `std::io::Read` wrapper.
#![no_main]

use std::io::Read;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use zstd_native::Dictionary;
use zstd_native_fuzz::{
    DictSpec, OUT_CAP, c_accepts_ddict, c_decompress, c_stream_decode, chunk_sizes, first_diff,
    native_stream_decode,
};

#[derive(Arbitrary, Debug)]
struct Input {
    dict: DictSpec,
    window_log_max_n: u8,
    in_sizes: Vec<u16>,
    out_sizes: Vec<u16>,
    asm: bool,
    data: Vec<u8>,
}

fuzz_target!(|input: Input| {
    let dict_bytes = input.dict.bytes();
    let dict = match &dict_bytes {
        None => None,
        Some(d) => match Dictionary::new(d) {
            Ok(x) => Some(x),
            // zstd-native may be stricter than libzstd.
            Err(_) => return,
        },
    };
    if let Some(d) = &dict_bytes {
        assert!(
            c_accepts_ddict(d),
            "zstd-native accepts a dictionary libzstd rejects"
        );
    }
    let window_log_max = match input.window_log_max_n % 3 {
        0 => 0,
        _ => 10 + (input.window_log_max_n as u32 % 18),
    };
    let in_sizes = chunk_sizes(&input.in_sizes);
    let out_sizes = chunk_sizes(&input.out_sizes);
    let data = &input.data;

    zstd_native::__private::set_asm(Some(input.asm));
    let n = native_stream_decode(data, &in_sizes, &out_sizes, dict, window_log_max, OUT_CAP);
    zstd_native::__private::set_asm(None);
    if n.capped {
        return;
    }
    let c = c_stream_decode(
        data,
        &in_sizes,
        &out_sizes,
        dict_bytes.as_deref(),
        window_log_max,
        OUT_CAP,
    );
    // Decoded bytes are decoded bytes: what both produced must agree.
    let common = n.output.len().min(c.output.len());
    assert!(
        n.output[..common] == c.output[..common],
        "streaming output differs from libzstd: {} (native {:?}, libzstd {:?})",
        first_diff(&n.output, &c.output),
        (n.complete, &n.error),
        (c.complete, &c.error)
    );
    if n.error.is_some() {
        return;
    }
    if n.complete {
        assert!(
            c.error.is_none(),
            "zstd-native completes, libzstd rejects ({:?})",
            c.error
        );
        assert!(
            n.output.len() == c.output.len(),
            "{}",
            first_diff(&n.output, &c.output)
        );
        if !c.complete {
            // ZSTD_decompressStream returns 0 only right after a zstd frame:
            // not for empty input nor after a trailing skippable frame. The
            // one-shot API then decides whether the input is complete.
            let one = c_decompress(data, OUT_CAP, dict_bytes.as_deref());
            assert!(
                one.as_ref().is_ok_and(|v| *v == n.output),
                "zstd-native completes, libzstd does not: {:?}",
                one.map(|v| v.len())
            );
        }

        // The one-shot decoder and the io::Read wrapper agree.
        let one_shot = match &dict_bytes {
            Some(d) => zstd_native::Decompressor::with_dictionary(Dictionary::new(d).unwrap())
                .decompress(data),
            None => zstd_native::decompress(data),
        };
        match one_shot {
            Ok(v) => assert!(
                v == n.output,
                "one-shot differs: {}",
                first_diff(&v, &n.output)
            ),
            // The one-shot decoder has no window limit; otherwise it must agree.
            Err(e) => assert!(
                window_log_max != 0,
                "one-shot decoder rejects what the streaming one accepts: {e:?}"
            ),
        }
        let mut io_out = Vec::new();
        let r = match &dict_bytes {
            Some(d) => {
                let mut dec =
                    zstd_native::Decoder::with_dictionary(&data[..], Dictionary::new(d).unwrap());
                dec.set_window_log_max(if window_log_max == 0 {
                    27
                } else {
                    window_log_max
                });
                dec.read_to_end(&mut io_out)
            }
            None => {
                let mut dec = zstd_native::Decoder::new(&data[..]);
                dec.set_window_log_max(if window_log_max == 0 {
                    27
                } else {
                    window_log_max
                });
                dec.read_to_end(&mut io_out)
            }
        };
        r.expect("io::Read decoder fails where StreamDecoder completes");
        assert!(
            io_out == n.output,
            "io::Read output differs: {}",
            first_diff(&io_out, &n.output)
        );
    }
});
