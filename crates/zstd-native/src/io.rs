//! `std::io` adapters.

use std::io::{self, BufRead, BufReader, Read};

use crate::dict::Dictionary;
use crate::error::Error;
use crate::stream::StreamDecoder;

/// Decompresses a zstd stream from a reader (`std::io::Read`), frame after
/// frame, like the `zstd` crate's `Decoder`.
///
/// ```
/// use std::io::Read;
/// # let compressed: &[u8] = &[0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x05, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f];
/// let mut out = String::new();
/// zstd_native::Decoder::new(compressed).read_to_string(&mut out)?;
/// assert_eq!(out, "hello");
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// Input that ends inside a frame is an [`io::ErrorKind::UnexpectedEof`]
/// error; corrupt input is [`io::ErrorKind::InvalidData`] (the source is an
/// [`Error`]). Empty input is zero frames: empty output, as with
/// libzstd (the `zstd` crate's reader reports an error there instead).
#[derive(Debug)]
pub struct Decoder<R> {
    reader: R,
    dec: StreamDecoder,
    failed: bool,
}

impl<R: Read> Decoder<BufReader<R>> {
    /// Reads from `reader` through a buffer (use [`with_buffer`](Decoder::with_buffer)
    /// for a reader that already buffers, such as a `&[u8]`).
    pub fn new(reader: R) -> Self {
        Decoder::with_buffer(BufReader::with_capacity(128 << 10, reader))
    }
}

impl<R: BufRead> Decoder<R> {
    /// Reads from a buffered reader.
    pub fn with_buffer(reader: R) -> Self {
        Decoder {
            reader,
            dec: StreamDecoder::new(),
            failed: false,
        }
    }

    /// Reads from a buffered reader, using `dict` for every frame.
    pub fn with_dictionary(reader: R, dict: Dictionary) -> Self {
        Decoder {
            reader,
            dec: StreamDecoder::with_dictionary(dict),
            failed: false,
        }
    }

    /// Sets the largest window accepted (see [`StreamDecoder::set_window_log_max`]).
    pub fn set_window_log_max(&mut self, log: u32) {
        self.dec.set_window_log_max(log);
    }

    /// The underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// The underlying reader, mutably (reading from it directly loses the
    /// decoder's place in the stream).
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Returns the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: BufRead> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.failed {
            return Err(io::Error::other("zstd decoder failed earlier"));
        }
        loop {
            let input = self.reader.fill_buf()?;
            let eof = input.is_empty();
            let p = match self.dec.decode(input, buf) {
                Ok(p) => p,
                Err(e) => {
                    self.failed = true;
                    return Err(e.into());
                }
            };
            self.reader.consume(p.read);
            if p.written > 0 {
                return Ok(p.written);
            }
            if eof {
                return if p.at_frame_boundary {
                    Ok(0)
                } else {
                    self.failed = true;
                    Err(Error::Truncated.into())
                };
            }
        }
    }
}
