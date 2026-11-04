//! Frame headers (RFC 8878 section 3.1.1).

use crate::error::{Error, Result};

pub(crate) const MAGIC: u32 = 0xFD2F_B528;
pub(crate) const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
pub(crate) const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
pub(crate) const WINDOWLOG_MAX: u32 = 31;
const WINDOWLOG_ABSOLUTEMIN: u32 = 10;
/// Magic + frame header descriptor (libzstd's `ZSTD_startingInputLength`).
pub(crate) const STARTING_INPUT_LENGTH: usize = 5;
pub(crate) const FRAME_HEADER_SIZE_MIN: usize = 6;
pub(crate) const BLOCK_HEADER_SIZE: usize = 3;

/// A parsed zstd frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Decompressed size of the frame, when the header stores it.
    pub content_size: Option<u64>,
    /// Window size: how far back matches may reach.
    pub window_size: u64,
    /// Dictionary ID, 0 when none is stored.
    pub dict_id: u32,
    /// Whether the frame ends with a content checksum.
    pub has_checksum: bool,
    /// Size of the header in bytes, magic number included.
    pub header_size: usize,
}

impl FrameHeader {
    /// Parses the header of the zstd frame at the start of `src`.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownFrame`] if `src` doesn't start with a zstd frame
    /// (skippable frames included), [`Error::Truncated`] if the header is
    /// incomplete, and [`Error::UnsupportedFrameParameter`] /
    /// [`Error::WindowTooLarge`] for invalid headers.
    pub fn parse(src: &[u8]) -> Result<FrameHeader> {
        if src.len() < 4 {
            return Err(Error::Truncated);
        }
        let magic = u32::from_le_bytes(src[..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::UnknownFrame);
        }
        let Some(&fhd) = src.get(4) else {
            return Err(Error::Truncated);
        };
        let dict_code = fhd & 3;
        let checksum = (fhd >> 2) & 1 != 0;
        let single = (fhd >> 5) & 1 != 0;
        let fcs_id = fhd >> 6;
        let size = 5
            + usize::from(!single)
            + [0, 1, 2, 4][dict_code as usize]
            + [usize::from(single), 2, 4, 8][fcs_id as usize];
        if src.len() < size {
            return Err(Error::Truncated);
        }
        if fhd & 0x08 != 0 {
            return Err(Error::UnsupportedFrameParameter);
        }
        let mut pos = 5;
        let mut window_size = 0u64;
        if !single {
            let wl = src[pos];
            pos += 1;
            let log = u32::from(wl >> 3) + WINDOWLOG_ABSOLUTEMIN;
            if log > WINDOWLOG_MAX {
                return Err(Error::WindowTooLarge);
            }
            let base = 1u64 << log;
            window_size = base + (base / 8) * u64::from(wl & 7);
        }
        let le = |p: usize, n: usize| -> u64 {
            let mut b = [0u8; 8];
            b[..n].copy_from_slice(&src[p..p + n]);
            u64::from_le_bytes(b)
        };
        let dict_len = [0, 1, 2, 4][dict_code as usize];
        let dict_id = le(pos, dict_len) as u32;
        pos += dict_len;
        let content_size = match fcs_id {
            0 => single.then(|| u64::from(src[pos])),
            1 => Some(le(pos, 2) + 256),
            2 => Some(le(pos, 4)),
            _ => Some(le(pos, 8)),
        };
        if single {
            window_size = content_size.unwrap_or(0);
        }
        Ok(FrameHeader {
            content_size,
            window_size,
            dict_id,
            has_checksum: checksum,
            header_size: size,
        })
    }

    /// Largest block (libzstd's `blockSizeMax`).
    pub(crate) fn block_size_max(&self) -> usize {
        self.window_size.min(crate::block::BLOCK_SIZE_MAX as u64) as usize
    }
}

#[inline]
pub(crate) fn read_le32(src: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(src[pos..pos + 4].try_into().unwrap())
}

#[inline]
pub(crate) fn is_skippable(magic: u32) -> bool {
    magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC
}

/// Total frame header size (magic included) from the frame header
/// descriptor byte.
#[inline]
pub(crate) fn header_size(fhd: u8) -> usize {
    let single = (fhd >> 5) & 1 != 0;
    5 + usize::from(!single)
        + [0, 1, 2, 4][(fhd & 3) as usize]
        + [usize::from(single), 2, 4, 8][(fhd >> 6) as usize]
}
