use core::fmt;

/// Errors returned by the decoder.
///
/// Corrupt input never panics or reads out of bounds; it returns one of
/// these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Error {
    /// The input doesn't start with a zstd (or skippable) frame magic number.
    UnknownFrame,
    /// The input ends before the frame does.
    Truncated,
    /// The compressed data is corrupt.
    Corrupted,
    /// The frame's content checksum doesn't match the decompressed data.
    ChecksumMismatch,
    /// The frame header uses a reserved bit or an unsupported parameter.
    UnsupportedFrameParameter,
    /// The frame's window is larger than the decoder's limit.
    WindowTooLarge,
    /// The frame needs a dictionary that wasn't given, or a different one.
    DictionaryMismatch,
    /// A dictionary given to the decoder is invalid.
    InvalidDictionary,
    /// The output buffer is too small for the decompressed data.
    OutputTooSmall,
    /// The decompressed size is larger than this platform can hold in memory.
    OutputTooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::UnknownFrame => "unknown frame magic number (not zstd data)",
            Error::Truncated => "compressed data is truncated",
            Error::Corrupted => "compressed data is corrupt",
            Error::ChecksumMismatch => "content checksum mismatch",
            Error::UnsupportedFrameParameter => "unsupported frame parameter",
            Error::WindowTooLarge => "frame window is larger than the decoder limit",
            Error::DictionaryMismatch => "frame requires a different dictionary",
            Error::InvalidDictionary => "invalid dictionary",
            Error::OutputTooSmall => "output buffer is too small",
            Error::OutputTooLarge => "decompressed size is too large",
        })
    }
}

impl core::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        let kind = match e {
            Error::Truncated => std::io::ErrorKind::UnexpectedEof,
            _ => std::io::ErrorKind::InvalidData,
        };
        std::io::Error::new(kind, e)
    }
}

pub(crate) type Result<T> = core::result::Result<T, Error>;
