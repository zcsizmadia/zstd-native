//! Dictionaries (RFC 8878 section 5): zstd-format dictionaries with
//! entropy tables, and raw content dictionaries.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::block::Entropy;
use crate::error::{Error, Result};
use crate::fse::{self, LL_KIND, ML_KIND, OF_KIND};

const DICT_MAGIC: u32 = 0xEC30_A437;

/// A decompression dictionary, parsed once and reused for many frames.
#[derive(Clone)]
pub struct Dictionary {
    pub(crate) id: u32,
    pub(crate) content: Vec<u8>,
    /// Entropy tables of a zstd-format dictionary; `None` for raw content.
    pub(crate) entropy: Option<Box<Entropy>>,
}

impl core::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dictionary")
            .field("id", &self.id)
            .field("content_len", &self.content.len())
            .field("has_entropy", &self.entropy.is_some())
            .finish()
    }
}

impl Dictionary {
    /// Parses a dictionary like libzstd's `ZSTD_dct_auto`: data that starts
    /// with the dictionary magic number is a zstd dictionary (ID, entropy
    /// tables, repeat offsets, content); anything else is raw content.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidDictionary`] if a zstd-format dictionary is corrupt.
    pub fn new(data: &[u8]) -> Result<Dictionary> {
        if data.len() < 8 || u32::from_le_bytes(data[..4].try_into().unwrap()) != DICT_MAGIC {
            return Ok(Self::raw(data));
        }
        let id = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let mut ent = Box::new(Entropy::new());
        let mut p = 8;
        p += ent
            .huf
            .read(&data[p..])
            .map_err(|_| Error::InvalidDictionary)?;
        ent.huf_ok = true;
        for kind in [OF_KIND, ML_KIND, LL_KIND] {
            let mut norm = [0i16; fse::MAX_ML + 1];
            let (max_sym, log, used) = fse::read_ncount(&data[p..], kind.max, &mut norm)
                .map_err(|_| Error::InvalidDictionary)?;
            if log > kind.max_log {
                return Err(Error::InvalidDictionary);
            }
            ent.seq.build(kind, &norm, max_sym + 1, log);
            p += used;
        }
        ent.fse_ok = true;
        if p + 12 > data.len() {
            return Err(Error::InvalidDictionary);
        }
        let content_len = data.len() - (p + 12);
        for i in 0..3 {
            let r = u32::from_le_bytes(data[p..p + 4].try_into().unwrap());
            p += 4;
            if r == 0 || r as usize > content_len {
                return Err(Error::InvalidDictionary);
            }
            ent.rep[i] = r;
        }
        Ok(Dictionary {
            id,
            content: data[p..].to_vec(),
            entropy: Some(ent),
        })
    }

    /// A raw content dictionary (ID 0): `data` is only history for matches.
    pub fn raw(data: &[u8]) -> Dictionary {
        Dictionary {
            id: 0,
            content: data.to_vec(),
            entropy: None,
        }
    }

    /// The dictionary ID (0 for raw content).
    pub fn id(&self) -> u32 {
        self.id
    }
}
