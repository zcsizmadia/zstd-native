//! Dictionaries (RFC 8878 section 5): zstd-format dictionaries with
//! entropy tables, and raw content dictionaries.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::block::Entropy;
use crate::error::{Error, Result};
use crate::fse::{self, LL_KIND, ML_KIND, OF_KIND};

const DICT_MAGIC: u32 = 0xEC30_A437;

/// A decompression dictionary, parsed once and reused for many frames
/// (libzstd's `ZSTD_DDict`).
///
/// Cloning is cheap: clones share the parsed content and tables (like
/// `ZSTD_DCtx_refDDict`), and a decompressor uses the tables in place
/// rather than copying them for each frame.
#[derive(Clone)]
pub struct Dictionary {
    inner: Arc<Inner>,
}

struct Inner {
    id: u32,
    content: Vec<u8>,
    /// Entropy tables of a zstd-format dictionary; `None` for raw content.
    entropy: Option<Entropy>,
}

impl core::fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dictionary")
            .field("id", &self.inner.id)
            .field("content_len", &self.inner.content.len())
            .field("has_entropy", &self.inner.entropy.is_some())
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
        // The tables are built in place in the shared allocation.
        let mut inner = Arc::new(Inner {
            id,
            content: Vec::new(),
            entropy: Some(Entropy::new()),
        });
        let Some(this) = Arc::get_mut(&mut inner) else {
            unreachable!()
        };
        let Some(ent) = this.entropy.as_mut() else {
            unreachable!()
        };
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
            ent.seq.build(
                kind,
                &norm,
                max_sym + 1,
                log,
                crate::cpu::Features::detect(),
            );
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
        this.content = data[p..].to_vec();
        Ok(Dictionary { inner })
    }

    /// A raw content dictionary (ID 0): `data` is only history for matches.
    pub fn raw(data: &[u8]) -> Dictionary {
        Dictionary {
            inner: Arc::new(Inner {
                id: 0,
                content: data.to_vec(),
                entropy: None,
            }),
        }
    }

    /// The dictionary ID (0 for raw content).
    pub fn id(&self) -> u32 {
        self.inner.id
    }

    /// Whether `other` is a clone of this dictionary.
    pub(crate) fn same(&self, other: &Dictionary) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) fn content(&self) -> &[u8] {
        &self.inner.content
    }

    /// The entropy tables and repeat offsets frames start with.
    pub(crate) fn entropy(&self) -> Option<&Entropy> {
        self.inner.entropy.as_ref()
    }
}
