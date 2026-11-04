//! Decodes arbitrary bytes one-shot: must never crash or hang, the asm
//! and Rust Huffman loops must agree, and whenever zstd-native accepts the
//! input, libzstd must accept it with the same output (zstd-native may
//! reject more).
#![no_main]

use std::cell::RefCell;

use libfuzzer_sys::fuzz_target;
use zstd_native_fuzz::{OUT_CAP, c_decompress, first_diff};

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; OUT_CAP]);
}

fuzz_target!(|data: &[u8]| {
    // Header parsing never panics.
    let _ = zstd_native::content_size(data);
    let _ = zstd_native::FrameHeader::parse(data);

    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        let mut dec = zstd_native::Decompressor::new();

        // The asm Huffman loops where the CPU has them, then the Rust ones.
        zstd_native::__private::set_asm(Some(true));
        let a = dec.decompress_to_slice(data, &mut buf);
        let a_out = a.as_ref().map(|&n| buf[..n].to_vec()).ok();
        zstd_native::__private::set_asm(Some(false));
        let b = dec.decompress_to_slice(data, &mut buf);
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "asm and Rust Huffman loops disagree"
        );
        if let (Some(a_out), Ok(n)) = (&a_out, b) {
            assert!(
                a_out[..] == buf[..n],
                "asm and Rust Huffman loops disagree: {}",
                first_diff(a_out, &buf[..n])
            );
        }
        zstd_native::__private::set_asm(None);

        // The growing-vector API agrees with the slice API.
        if let Some(a_out) = &a_out {
            let v = dec
                .decompress(data)
                .expect("decompress() after decompress_to_slice() ok");
            assert!(
                v == *a_out,
                "decompress() != decompress_to_slice(): {}",
                first_diff(&v, a_out)
            );
            let v = zstd_native::decompress(data).expect("one-call decompress ok");
            assert!(
                v == *a_out,
                "cached decompress() differs: {}",
                first_diff(&v, a_out)
            );
        }

        // libzstd must accept what zstd-native accepts, with the same output.
        let c = c_decompress(data, OUT_CAP, None);
        match (&a, &c) {
            (Ok(_), Err(e)) => panic!("zstd-native accepts, libzstd rejects ({e})"),
            (Ok(_), Ok(c)) => {
                let a_out = a_out.as_ref().unwrap();
                assert!(
                    c == a_out,
                    "output differs from libzstd: {}",
                    first_diff(a_out, c)
                );
            }
            (Err(zstd_native::Error::OutputTooSmall), Ok(c)) => {
                panic!(
                    "zstd-native needs more than {} bytes of output, libzstd {}",
                    OUT_CAP,
                    c.len()
                )
            }
            _ => {}
        }
    });
});
