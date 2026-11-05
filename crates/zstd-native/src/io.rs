//! `std::io` adapters: [`Decoder`] (`Read`) and [`Encoder`] (`Write`).

use std::boxed::Box;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::vec;

use crate::dict::Dictionary;
use crate::enc::{EncoderDictionary, EndOp, StreamEncoder};
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

/// The output buffer size of [`Encoder`]: libzstd's `ZSTD_CStreamOutSize()`
/// (a full block's compressed bound, a block header and a checksum).
const ENCODER_BUF_SIZE: usize = crate::compress_bound(128 << 10) + 3 + 4;

/// Compresses into a writer (`std::io::Write`), like the `zstd` crate's
/// `Encoder`: one zstd frame, ended by [`finish`](Encoder::finish). The
/// output is byte-identical to libzstd 1.5.7's streaming compressor's
/// (`ZSTD_compressStream2`) for the same level, writes and flushes.
///
/// ```
/// use std::io::Write;
/// let mut enc = zstd_native::Encoder::new(Vec::new(), 3);
/// enc.write_all(b"hello hello hello hello")?;
/// let compressed = enc.finish()?;
/// assert_eq!(zstd_native::decompress(&compressed).unwrap(), b"hello hello hello hello");
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// [`flush`](Write::flush) compresses the data written so far and writes
/// it out (`ZSTD_e_flush`), without ending the frame. Dropping the encoder
/// without calling [`finish`](Encoder::finish) leaves the frame
/// incomplete. Errors are [`io::ErrorKind::InvalidInput`] for a pledged
/// size that doesn't match, [`io::ErrorKind::InvalidData`] for the others
/// (the source is an [`Error`]).
#[derive(Debug)]
pub struct Encoder<W: Write> {
    writer: W,
    enc: StreamEncoder,
    buf: Box<[u8]>,
}

impl<W: Write> Encoder<W> {
    /// Compresses at `level` (see [`compress`](crate::compress)) into `writer`.
    pub fn new(writer: W, level: i32) -> Self {
        Encoder {
            writer,
            enc: StreamEncoder::new(level),
            buf: vec![0u8; ENCODER_BUF_SIZE].into_boxed_slice(),
        }
    }

    /// Compresses at `level` with the dictionary `dict` (see
    /// [`Compressor::set_dictionary`](crate::Compressor::set_dictionary))
    /// into `writer`, like the `zstd` crate's `Encoder::with_dictionary`.
    ///
    /// ```
    /// use std::io::Write;
    /// let dict = b"hello world, hello there";
    /// let mut enc = zstd_native::Encoder::with_dictionary(Vec::new(), 3, dict)?;
    /// enc.write_all(b"hello there, hello world")?;
    /// let compressed = enc.finish()?;
    /// let dict = zstd_native::Dictionary::new(dict).unwrap();
    /// let mut d = zstd_native::Decompressor::with_dictionary(dict);
    /// assert_eq!(d.decompress(&compressed).unwrap(), b"hello there, hello world");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidData`] if a zstd-format dictionary is
    /// corrupt.
    pub fn with_dictionary(writer: W, level: i32, dict: &[u8]) -> io::Result<Self> {
        Ok(Encoder {
            writer,
            enc: StreamEncoder::with_dictionary(level, dict).map_err(enc_err)?,
            buf: vec![0u8; ENCODER_BUF_SIZE].into_boxed_slice(),
        })
    }

    /// Compresses with a prepared dictionary, at its level, into `writer`
    /// (the `zstd` crate's `Encoder::with_prepared_dictionary`).
    pub fn with_prepared_dictionary(writer: W, dict: &EncoderDictionary) -> Self {
        Encoder {
            writer,
            enc: StreamEncoder::with_prepared_dictionary(dict),
            buf: vec![0u8; ENCODER_BUF_SIZE].into_boxed_slice(),
        }
    }

    /// Declares the size of the data to be written (see
    /// [`StreamEncoder::set_pledged_src_size`]): the frame header then
    /// holds it. Must be called before the first write.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] after the first write.
    pub fn set_pledged_src_size(&mut self, size: Option<u64>) -> io::Result<()> {
        self.enc
            .set_pledged_src_size(size)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Ends the frame with a content checksum (see
    /// [`StreamEncoder::set_checksum`]; the `zstd` crate's
    /// `Encoder::include_checksum`). Must be called before the first write.
    ///
    /// ```
    /// use std::io::Write;
    /// let mut enc = zstd_native::Encoder::new(Vec::new(), 3);
    /// enc.set_checksum(true)?;
    /// enc.write_all(b"checked, checked, checked")?;
    /// let compressed = enc.finish()?;
    /// assert_eq!(zstd_native::decompress(&compressed).unwrap(), b"checked, checked, checked");
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] after the first write.
    pub fn set_checksum(&mut self, on: bool) -> io::Result<()> {
        self.enc
            .set_checksum(on)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Writes the content size into the frame header when it is pledged
    /// (see [`StreamEncoder::set_content_size_flag`]; the `zstd` crate's
    /// `Encoder::include_contentsize`). Must be called before the first
    /// write.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] after the first write.
    pub fn set_content_size_flag(&mut self, on: bool) -> io::Result<()> {
        self.enc
            .set_content_size_flag(on)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Writes the dictionary's ID into the frame header (see
    /// [`StreamEncoder::set_dict_id_flag`]; the `zstd` crate's
    /// `Encoder::include_dictid`). Must be called before the first write.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] after the first write.
    pub fn set_dict_id_flag(&mut self, on: bool) -> io::Result<()> {
        self.enc
            .set_dict_id_flag(on)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Compresses with `n` worker threads (see
    /// [`StreamEncoder::set_workers`]; the `zstd` crate's
    /// `Encoder::multithread`), with libzstd's output for the same worker
    /// count. Must be called before the first write.
    ///
    /// ```
    /// use std::io::Write;
    /// let data = b"hello hello hello hello ".repeat(100_000);
    /// let mut enc = zstd_native::Encoder::new(Vec::new(), 3);
    /// enc.set_workers(4)?;
    /// enc.write_all(&data)?;
    /// let compressed = enc.finish()?;
    /// assert_eq!(zstd_native::decompress(&compressed).unwrap(), data);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] after the first write.
    pub fn set_workers(&mut self, n: u32) -> io::Result<()> {
        self.enc
            .set_workers(n)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// The underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// The underlying writer, mutably (writing to it directly corrupts the
    /// compressed stream).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Ends the frame (`ZSTD_e_end`), writes out everything and returns the
    /// writer.
    ///
    /// # Errors
    ///
    /// As the writer's, or as [`StreamEncoder::compress`].
    pub fn finish(mut self) -> io::Result<W> {
        self.run(EndOp::End)?;
        self.writer.flush()?;
        Ok(self.writer)
    }

    /// Runs `op` (flush or end) with no more input until it completes.
    fn run(&mut self, op: EndOp) -> io::Result<()> {
        loop {
            let p = self.enc.compress(&[], &mut self.buf, op).map_err(enc_err)?;
            self.writer.write_all(&self.buf[..p.written])?;
            if p.remaining == 0 {
                return Ok(());
            }
        }
    }
}

/// Maps a compression error to an I/O error.
fn enc_err(e: Error) -> io::Error {
    let kind = match e {
        Error::SrcSizeWrong | Error::StageWrong => io::ErrorKind::InvalidInput,
        Error::MemoryAllocation => io::ErrorKind::OutOfMemory,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, e)
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        loop {
            let p = self
                .enc
                .compress(data, &mut self.buf, EndOp::Continue)
                .map_err(enc_err)?;
            self.writer.write_all(&self.buf[..p.written])?;
            if p.read > 0 {
                return Ok(p.read);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.run(EndOp::Flush)?;
        self.writer.flush()
    }
}
