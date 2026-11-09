//! Long distance matching (LDM): a port of libzstd 1.5.7's
//! `compress/zstd_ldm.c` and `zstd_ldm_geartab.h`, and of how
//! `zstd_compress.c` drives it.
//!
//! libzstd enables it by itself (`ZSTD_resolveEnableLdm`) for `btopt` and
//! above with `windowLog >= 27` (level 22 on inputs over 64 MiB, or of
//! unknown size); a hidden test hook forces it on for any parameters.
//!
//! # How it works
//!
//! A gear rolling hash ([`Gear`]) cuts the input at content-defined split
//! points, on average every `1 << hashRateLog` bytes. The `minMatchLength`
//! bytes before each split point are hashed with XXH64: the low bits pick
//! a bucket of the hash table ([`LdmState::hash_table`], `1 << bucketSizeLog`
//! entries per bucket, filled round-robin), the high 32 bits are a checksum.
//! A split point whose bucket holds an entry with the same checksum, and
//! whose bytes match it over at least `minMatchLength` bytes (counted
//! forward, then backward), gives a long match ([`RawSeq`]).
//!
//! # Per block (`ZSTD_buildSeqStore`)
//!
//! For each block, [`LdmState::generate_sequences`] finds the block's long
//! matches (`ZSTD_ldm_generateSequences`, with the LDM window's overflow
//! correction and maximum distance). Then [`ldm_block_compress`]
//! (`ZSTD_ldm_blockCompress`):
//! - the optimal parser (`btopt` and above) runs once on the block with the
//!   long matches as extra candidates (`ZSTD_optLdm_*`, see the `opt`
//!   module);
//! - the other strategies take every long match as is, and run their block
//!   compressor on the literals between them (filling the `fast` and `dfast`
//!   tables first, `ZSTD_ldm_fillFastTables`).
//!
//! # The window
//!
//! The LDM state has its own window ([`Window`], same positions as the
//! match state's: the caller's buffer, see the `fast` module), updated with
//! every input segment (`ZSTD_compressContinue_internal`). A raw-content
//! dictionary loaded into the context's tables (not attached or copied from
//! a CDict) is also loaded into the LDM table (`ZSTD_loadDictionaryContent`);
//! it then stays in the [`CDict`]'s buffer, the extDict of the LDM window.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::cdict::CDict;
use super::fast::{
    CURRENT_MAX, HASH_READ_SIZE, MIN_SEQ_STORE_BLOCK, MatchState, TableLoadMethod,
    WINDOW_START_INDEX, Window,
};
use super::params::{CParams, HASHLOG_MIN, Strategy, WINDOWLOG_MAX};
use super::seq_store::{SeqStore, offset_to_offbase};
use crate::error::{Error, Result};
use crate::xxhash::xxh64;

/// `LDM_BUCKET_SIZE_LOG`
const LDM_BUCKET_SIZE_LOG: u32 = 4;
/// `LDM_MIN_MATCH_LENGTH`
const LDM_MIN_MATCH_LENGTH: u32 = 64;
/// `LDM_BATCH_SIZE`: split points processed at a time.
const LDM_BATCH_SIZE: usize = 64;
/// `ZSTD_LDM_BUCKETSIZELOG_MAX`
const LDM_BUCKETSIZELOG_MAX: u32 = 8;
/// `ZSTD_HASHLOG_MAX`
const HASHLOG_MAX: u32 = if WINDOWLOG_MAX < 30 {
    WINDOWLOG_MAX
} else {
    30
};
/// `ZSTD_LDM_DEFAULT_WINDOW_LOG` (`ZSTD_WINDOWLOG_LIMIT_DEFAULT`): the
/// window log of a frame with LDM enabled explicitly, unless set.
pub(crate) const LDM_DEFAULT_WINDOW_LOG: u32 = 27;

/// `ZSTD_ldm_gearTab`
#[rustfmt::skip]
static GEAR_TAB: [u64; 256] = [
    0xf5b8f72c5f77775c,
    0x84935f266b7ac412,
    0xb647ada9ca730ccc,
    0xb065bb4b114fb1de,
    0x34584e7e8c3a9fd0,
    0x4e97e17c6ae26b05,
    0x3a03d743bc99a604,
    0xcecd042422c4044f,
    0x76de76c58524259e,
    0x9c8528f65badeaca,
    0x86563706e2097529,
    0x2902475fa375d889,
    0xafb32a9739a5ebe6,
    0xce2714da3883e639,
    0x21eaf821722e69e,
    0x37b628620b628,
    0x49a8d455d88caf5,
    0x8556d711e6958140,
    0x4f7ae74fc605c1f,
    0x829f0c3468bd3a20,
    0x4ffdc885c625179e,
    0x8473de048a3daf1b,
    0x51008822b05646b2,
    0x69d75d12b2d1cc5f,
    0x8c9d4a19159154bc,
    0xc3cc10f4abbd4003,
    0xd06ddc1cecb97391,
    0xbe48e6e7ed80302e,
    0x3481db31cee03547,
    0xacc3f67cdaa1d210,
    0x65cb771d8c7f96cc,
    0x8eb27177055723dd,
    0xc789950d44cd94be,
    0x934feadc3700b12b,
    0x5e485f11edbdf182,
    0x1e2e2a46fd64767a,
    0x2969ca71d82efa7c,
    0x9d46e9935ebbba2e,
    0xe056b67e05e6822b,
    0x94d73f55739d03a0,
    0xcd7010bdb69b5a03,
    0x455ef9fcd79b82f4,
    0x869cb54a8749c161,
    0x38d1a4fa6185d225,
    0xb475166f94bbe9bb,
    0xa4143548720959f1,
    0x7aed4780ba6b26ba,
    0xd0ce264439e02312,
    0x84366d746078d508,
    0xa8ce973c72ed17be,
    0x21c323a29a430b01,
    0x9962d617e3af80ee,
    0xab0ce91d9c8cf75b,
    0x530e8ee6d19a4dbc,
    0x2ef68c0cf53f5d72,
    0xc03a681640a85506,
    0x496e4e9f9c310967,
    0x78580472b59b14a0,
    0x273824c23b388577,
    0x66bf923ad45cb553,
    0x47ae1a5a2492ba86,
    0x35e304569e229659,
    0x4765182a46870b6f,
    0x6cbab625e9099412,
    0xddac9a2e598522c1,
    0x7172086e666624f2,
    0xdf5003ca503b7837,
    0x88c0c1db78563d09,
    0x58d51865acfc289d,
    0x177671aec65224f1,
    0xfb79d8a241e967d7,
    0x2be1e101cad9a49a,
    0x6625682f6e29186b,
    0x399553457ac06e50,
    0x35dffb4c23abb74,
    0x429db2591f54aade,
    0xc52802a8037d1009,
    0x6acb27381f0b25f3,
    0xf45e2551ee4f823b,
    0x8b0ea2d99580c2f7,
    0x3bed519cbcb4e1e1,
    0xff452823dbb010a,
    0x9d42ed614f3dd267,
    0x5b9313c06257c57b,
    0xa114b8008b5e1442,
    0xc1fe311c11c13d4b,
    0x66e8763ea34c5568,
    0x8b982af1c262f05d,
    0xee8876faaa75fbb7,
    0x8a62a4d0d172bb2a,
    0xc13d94a3b7449a97,
    0x6dbbba9dc15d037c,
    0xc786101f1d92e0f1,
    0xd78681a907a0b79b,
    0xf61aaf2962c9abb9,
    0x2cfd16fcd3cb7ad9,
    0x868c5b6744624d21,
    0x25e650899c74ddd7,
    0xba042af4a7c37463,
    0x4eb1a539465a3eca,
    0xbe09dbf03b05d5ca,
    0x774e5a362b5472ba,
    0x47a1221229d183cd,
    0x504b0ca18ef5a2df,
    0xdffbdfbde2456eb9,
    0x46cd2b2fbee34634,
    0xf2aef8fe819d98c3,
    0x357f5276d4599d61,
    0x24a5483879c453e3,
    0x88026889192b4b9,
    0x28da96671782dbec,
    0x4ef37c40588e9aaa,
    0x8837b90651bc9fb3,
    0xc164f741d3f0e5d6,
    0xbc135a0a704b70ba,
    0x69cd868f7622ada,
    0xbc37ba89e0b9c0ab,
    0x47c14a01323552f6,
    0x4f00794bacee98bb,
    0x7107de7d637a69d5,
    0x88af793bb6f2255e,
    0xf3c6466b8799b598,
    0xc288c616aa7f3b59,
    0x81ca63cf42fca3fd,
    0x88d85ace36a2674b,
    0xd056bd3792389e7,
    0xe55c396c4e9dd32d,
    0xbefb504571e6c0a6,
    0x96ab32115e91e8cc,
    0xbf8acb18de8f38d1,
    0x66dae58801672606,
    0x833b6017872317fb,
    0xb87c16f2d1c92864,
    0xdb766a74e58b669c,
    0x89659f85c61417be,
    0xc8daad856011ea0c,
    0x76a4b565b6fe7eae,
    0xa469d085f6237312,
    0xaaf0365683a3e96c,
    0x4dbb746f8424f7b8,
    0x638755af4e4acc1,
    0x3d7807f5bde64486,
    0x17be6d8f5bbb7639,
    0x903f0cd44dc35dc,
    0x67b672eafdf1196c,
    0xa676ff93ed4c82f1,
    0x521d1004c5053d9d,
    0x37ba9ad09ccc9202,
    0x84e54d297aacfb51,
    0xa0b4b776a143445,
    0x820d471e20b348e,
    0x1874383cb83d46dc,
    0x97edeec7a1efe11c,
    0xb330e50b1bdc42aa,
    0x1dd91955ce70e032,
    0xa514cdb88f2939d5,
    0x2791233fd90db9d3,
    0x7b670a4cc50f7a9b,
    0x77c07d2a05c6dfa5,
    0xe3778b6646d0a6fa,
    0xb39c8eda47b56749,
    0x933ed448addbef28,
    0xaf846af6ab7d0bf4,
    0xe5af208eb666e49,
    0x5e6622f73534cd6a,
    0x297daeca42ef5b6e,
    0x862daef3d35539a6,
    0xe68722498f8e1ea9,
    0x981c53093dc0d572,
    0xfa09b0bfbf86fbf5,
    0x30b1e96166219f15,
    0x70e7d466bdc4fb83,
    0x5a66736e35f2a8e9,
    0xcddb59d2b7c1baef,
    0xd6c7d247d26d8996,
    0xea4e39eac8de1ba3,
    0x539c8bb19fa3aff2,
    0x9f90e4c5fd508d8,
    0xa34e5956fbaf3385,
    0x2e2f8e151d3ef375,
    0x173691e9b83faec1,
    0xb85a8d56bf016379,
    0x8382381267408ae3,
    0xb90f901bbdc0096d,
    0x7c6ad32933bcec65,
    0x76bb5e2f2c8ad595,
    0x390f851a6cf46d28,
    0xc3e6064da1c2da72,
    0xc52a0c101cfa5389,
    0xd78eaf84a3fbc530,
    0x3781b9e2288b997e,
    0x73c2f6dea83d05c4,
    0x4228e364c5b5ed7,
    0x9d7a3edf0da43911,
    0x8edcfeda24686756,
    0x5e7667a7b7a9b3a1,
    0x4c4f389fa143791d,
    0xb08bc1023da7cddc,
    0x7ab4be3ae529b1cc,
    0x754e6132dbe74ff9,
    0x71635442a839df45,
    0x2f6fb1643fbe52de,
    0x961e0a42cf7a8177,
    0xf3b45d83d89ef2ea,
    0xee3de4cf4a6e3e9b,
    0xcd6848542c3295e7,
    0xe4cee1664c78662f,
    0x9947548b474c68c4,
    0x25d73777a5ed8b0b,
    0xc915b1d636b7fc,
    0x21c2ba75d9b0d2da,
    0x5f6b5dcf608a64a1,
    0xdcf333255ff9570c,
    0x633b922418ced4ee,
    0xc136dde0b004b34a,
    0x58cc83b05d4b2f5a,
    0x5eb424dda28e42d2,
    0x62df47369739cd98,
    0xb4e0b42485e4ce17,
    0x16e1f0c1f9a8d1e7,
    0x8ec3916707560ebf,
    0x62ba6e2df2cc9db3,
    0xcbf9f4ff77d83a16,
    0x78d9d7d07d2bbcc4,
    0xef554ce1e02c41f4,
    0x8d7581127eccf94d,
    0xa9b53336cb3c8a05,
    0x38c42c0bf45c4f91,
    0x640893cdf4488863,
    0x80ec34bc575ea568,
    0x39f324f5b48eaa40,
    0xe9d9ed1f8eff527f,
    0x9224fc058cc5a214,
    0xbaba00b04cfe7741,
    0x309a9f120fcf52af,
    0xa558f3ec65626212,
    0x424bec8b7adabe2f,
    0x41622513a6aea433,
    0xb88da2d5324ca798,
    0xd287733b245528a4,
    0x9a44697e6d68aec3,
    0x7b1093be2f49bb28,
    0x50bbec632e3d8aad,
    0x6cd90723e1ea8283,
    0x897b9e7431b02bf3,
    0x219efdcb338a7047,
    0x3b0311f0a27c0656,
    0xdb17bf91c0db96e7,
    0x8cd4fd6b4e85a5b2,
    0xfab071054ba6409d,
    0x40d6fe831fa9dfd9,
    0xaf358debad7d791e,
    0xeb8d0e25a65e3e58,
    0xbbcbd3df14e08580,
    0xcf751f27ecdab2b,
    0x2b4da14f2613d8f4,
];

/// `ldmParams_t` (without `enableLdm`), as `ZSTD_ldm_adjustParameters`
/// derives it from the compression parameters (none set explicitly).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LdmParams {
    /// `hashLog`: the table has `1 << hash_log` entries.
    pub(crate) hash_log: u32,
    /// `bucketSizeLog`: entries per bucket.
    pub(crate) bucket_size_log: u32,
    /// `minMatchLength`
    pub(crate) min_match_length: u32,
    /// `hashRateLog`: a split point every `1 << hash_rate_log` bytes on
    /// average.
    pub(crate) hash_rate_log: u32,
    /// `windowLog`
    pub(crate) window_log: u32,
}

impl LdmParams {
    /// `ZSTD_ldm_adjustParameters()` from default (zero) LDM parameters.
    pub(crate) fn new(cp: &CParams) -> Self {
        let strategy = cp.strategy as u32;
        let window_log = cp.window_log;
        // mapping from [fast, rate7] to [btultra2, rate4]
        let hash_rate_log = 7 - strategy / 3;
        let hash_log = (window_log - hash_rate_log).clamp(HASHLOG_MIN, HASHLOG_MAX);
        let mut min_match_length = LDM_MIN_MATCH_LENGTH;
        if cp.strategy >= Strategy::BtUltra {
            min_match_length /= 2;
        }
        let bucket_size_log = strategy
            .clamp(LDM_BUCKET_SIZE_LOG, LDM_BUCKETSIZELOG_MAX)
            .min(hash_log);
        LdmParams {
            hash_log,
            bucket_size_log,
            min_match_length,
            hash_rate_log,
            window_log,
        }
    }

    /// `ZSTD_ldm_getMaxNbSeq()`: the most long matches in `max_chunk_size`
    /// bytes.
    pub(crate) fn max_nb_seq(&self, max_chunk_size: usize) -> usize {
        max_chunk_size / self.min_match_length as usize
    }
}

/// `ldmEntry_t`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LdmEntry {
    offset: u32,
    checksum: u32,
}

/// `rawSeq`: a long match, `lit_length` literals then `match_length` bytes
/// at `offset`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RawSeq {
    pub(crate) offset: u32,
    pub(crate) lit_length: u32,
    pub(crate) match_length: u32,
}

/// `RawSeqStore_t`: the long matches of a block (`seq.len()` is `size`)
/// and how far they have been consumed.
#[derive(Clone, Debug, Default)]
pub(crate) struct RawSeqStore {
    pub(crate) seq: Vec<RawSeq>,
    /// `pos`: the sequence being consumed.
    pub(crate) pos: usize,
    /// `posInSequence`: bytes of it consumed (literals, then match).
    pub(crate) pos_in_sequence: usize,
    /// `capacity`
    pub(crate) capacity: usize,
}

impl RawSeqStore {
    /// `ZSTD_ldm_skipSequences()`: skips `src_size` bytes, shortening the
    /// sequence it ends in (dropping its match if under `min_match`).
    pub(crate) fn skip_sequences(&mut self, mut src_size: usize, min_match: u32) {
        while src_size > 0 && self.pos < self.seq.len() {
            let p = self.pos;
            let seq = &mut self.seq[p];
            if src_size <= seq.lit_length as usize {
                // Skip past srcSize literals
                seq.lit_length -= src_size as u32;
                return;
            }
            src_size -= seq.lit_length as usize;
            seq.lit_length = 0;
            if src_size < seq.match_length as usize {
                // Skip past the first srcSize of the match
                seq.match_length -= src_size as u32;
                if seq.match_length < min_match {
                    // The match is too short, omit it
                    let ml = seq.match_length;
                    if p + 1 < self.seq.len() {
                        self.seq[p + 1].lit_length += ml;
                    }
                    self.pos += 1;
                }
                return;
            }
            src_size -= seq.match_length as usize;
            seq.match_length = 0;
            self.pos += 1;
        }
    }

    /// `maybeSplitSequence()`: the next sequence, cut to the `remaining`
    /// bytes of the block (`offset == 0`: the rest of the block is
    /// literals).
    fn maybe_split_sequence(&mut self, remaining: u32, min_match: u32) -> RawSeq {
        let mut sequence = self.seq[self.pos];
        debug_assert!(sequence.offset > 0);
        // Likely: No partial sequence
        if remaining >= sequence.lit_length + sequence.match_length {
            self.pos += 1;
            return sequence;
        }
        // Cut the sequence short (offset == 0 ==> rest is literals).
        if remaining <= sequence.lit_length {
            sequence.offset = 0;
        } else if remaining < sequence.lit_length + sequence.match_length {
            sequence.match_length = remaining - sequence.lit_length;
            if sequence.match_length < min_match {
                sequence.offset = 0;
            }
        }
        // Skip past `remaining` bytes for the future sequences.
        self.skip_sequences(remaining as usize, min_match);
        sequence
    }

    /// `ZSTD_ldm_skipRawSeqStoreBytes()` (and its copy
    /// `ZSTD_optLdm_skipRawSeqStoreBytes`): moves `nb_bytes` forward.
    pub(crate) fn skip_bytes(&mut self, nb_bytes: usize) {
        let mut curr_pos = (self.pos_in_sequence + nb_bytes) as u32;
        while curr_pos != 0 && self.pos < self.seq.len() {
            let s = self.seq[self.pos];
            if curr_pos >= s.lit_length + s.match_length {
                curr_pos -= s.lit_length + s.match_length;
                self.pos += 1;
            } else {
                self.pos_in_sequence = curr_pos as usize;
                break;
            }
        }
        if curr_pos == 0 || self.pos == self.seq.len() {
            self.pos_in_sequence = 0;
        }
    }
}

/// `ldmRollingHashState_t` and the `ZSTD_ldm_gear_*` functions.
#[derive(Clone, Copy, Debug)]
struct Gear {
    rolling: u64,
    stop_mask: u64,
}

impl Gear {
    /// `ZSTD_ldm_gear_init()`.
    fn new(p: &LdmParams) -> Self {
        let max_bits_in_mask = p.min_match_length.min(64);
        let hash_rate_log = p.hash_rate_log;
        // The splitting criterion has to trigger on average every
        // 2^hashRateLog bytes, and ideally depend on a window of
        // minMatchLength bytes. In the gear hash, bit n depends on the last
        // n bytes: use the hashRateLog highest bits that respect the second
        // condition.
        let stop_mask = if hash_rate_log > 0 && hash_rate_log <= max_bits_in_mask {
            ((1u64 << hash_rate_log) - 1) << (max_bits_in_mask - hash_rate_log)
        } else {
            // In this degenerate case we simply honor the hash rate.
            (1u64 << hash_rate_log) - 1
        };
        Gear {
            rolling: u64::from(u32::MAX),
            stop_mask,
        }
    }

    /// `ZSTD_ldm_gear_reset()`: meant to feed `data` into the hash without
    /// registering splits, but libzstd 1.5.7 never stores the hash it
    /// computes: the state is left unchanged. So it is here (the calls are
    /// kept where libzstd makes them).
    fn reset(&mut self, data: &[u8]) {
        let _ = data;
    }

    /// `ZSTD_ldm_gear_feed()`: appends to `splits` the split points (the
    /// number of bytes after which the hash matches the mask) found in
    /// `data`, until `splits` is full. Returns the number of bytes hashed.
    #[inline]
    fn feed(
        &mut self,
        data: &[u8],
        splits: &mut [usize; LDM_BATCH_SIZE],
        n_splits: &mut usize,
    ) -> usize {
        debug_assert!(*n_splits < LDM_BATCH_SIZE);
        let mut hash = self.rolling;
        let mask = self.stop_mask;
        for (n, &b) in data.iter().enumerate() {
            hash = (hash << 1).wrapping_add(GEAR_TAB[b as usize]);
            if hash & mask == 0 {
                splits[*n_splits] = n + 1;
                *n_splits += 1;
                if *n_splits == LDM_BATCH_SIZE {
                    self.rolling = hash;
                    return n + 1;
                }
            }
        }
        self.rolling = hash;
        data.len()
    }
}

/// The length of the common prefix of `a` and `b` (`ZSTD_count`).
#[inline]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let d = x ^ y;
        if d != 0 {
            return i + (d.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// `ZSTD_ldm_countBackwardsMatch()`: the bytes matching backwards before
/// `p_in` (not below `p_anchor`) in `ibuf` and `p_match` (not below
/// `p_match_base`) in `mbuf`.
#[inline]
fn count_backwards(
    ibuf: &[u8],
    mut p_in: usize,
    p_anchor: usize,
    mbuf: &[u8],
    mut p_match: usize,
    p_match_base: usize,
) -> usize {
    let mut match_length = 0;
    while p_in > p_anchor && p_match > p_match_base && ibuf[p_in - 1] == mbuf[p_match - 1] {
        p_in -= 1;
        p_match -= 1;
        match_length += 1;
    }
    match_length
}

/// The LDM state of a context (`ldmState_t`, with its parameters and its
/// sequence buffer, `cctx->ldmSequences`).
pub(crate) struct LdmState {
    pub(crate) params: LdmParams,
    /// `window`
    pub(crate) window: Window,
    /// `loadedDictEnd`
    loaded_dict_end: u32,
    /// `hashTable`: `1 << hash_log` entries in buckets.
    hash_table: super::table::Table<LdmEntry>,
    /// `bucketOffsets`: the next entry to replace in each bucket.
    bucket_offsets: Vec<u8>,
    /// The dictionary loaded into the window (its extDict while
    /// `window.dict_base_in_dict`).
    dict: Option<Arc<CDict>>,
    /// The block's long matches (`cctx->ldmSequences`, capacity
    /// `maxNbLdmSequences`).
    pub(crate) seqs: RawSeqStore,
    /// `splitIndices`
    splits: [usize; LDM_BATCH_SIZE],
    /// `matchCandidates`: (split, hash, checksum).
    candidates: [(usize, u32, u32); LDM_BATCH_SIZE],
}

impl core::fmt::Debug for LdmState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LdmState")
            .field("params", &self.params)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl LdmState {
    /// A state for the (resolved) parameters `cp` and blocks of at most
    /// `block_size` bytes.
    pub(crate) fn new(cp: &CParams, block_size: usize) -> Self {
        let mut s = LdmState {
            params: LdmParams::new(cp),
            window: Window::new(),
            loaded_dict_end: 0,
            hash_table: super::table::Table::new(),
            bucket_offsets: Vec::new(),
            dict: None,
            seqs: RawSeqStore::default(),
            splits: [0; LDM_BATCH_SIZE],
            candidates: [(0, 0, 0); LDM_BATCH_SIZE],
        };
        s.reset(cp, block_size);
        s
    }

    /// The LDM part of `ZSTD_resetCCtx_internal`: parameters for `cp`
    /// (`ZSTD_ldm_adjustParameters`), zeroed tables, a fresh window.
    pub(crate) fn reset(&mut self, cp: &CParams, block_size: usize) {
        let p = LdmParams::new(cp);
        self.params = p;
        let h_size = 1usize << p.hash_log;
        self.hash_table.reset_zeroed(h_size);
        let nb_buckets = 1usize << (p.hash_log - p.bucket_size_log);
        self.bucket_offsets.clear();
        self.bucket_offsets.resize(nb_buckets, 0);
        self.window = Window::new();
        self.loaded_dict_end = 0;
        self.dict = None;
        self.seqs.seq.clear();
        self.seqs.capacity = p.max_nb_seq(block_size);
        self.seqs.seq.reserve(self.seqs.capacity);
    }

    /// The LDM window update of `ZSTD_compressContinue_internal`.
    pub(crate) fn update_window(&mut self, src: usize, src_size: usize) {
        self.window.update(src, src_size, false);
    }

    /// The multithreaded compressor's serial LDM step for a job
    /// (`ZSTDMT_serialState_genSequences`): appends `buf[start..end]` to the
    /// window and returns its long matches (at most
    /// [`reset`](Self::reset)'s `block_size / minMatchLength`, the job
    /// size there).
    ///
    /// # Errors
    ///
    /// As [`generate_sequences`](Self::generate_sequences) (not possible
    /// for a job of at most the job size).
    pub(crate) fn job_sequences(
        &mut self,
        buf: &[u8],
        start: usize,
        end: usize,
    ) -> Result<RawSeqStore> {
        self.window.update(start, end - start, false);
        self.generate_sequences(buf, start, end)?;
        let capacity = self.seqs.capacity;
        let mut seqs = core::mem::take(&mut self.seqs);
        self.seqs.capacity = capacity;
        seqs.pos = 0;
        seqs.pos_in_sequence = 0;
        Ok(seqs)
    }

    /// `ZSTD_ldm_insertEntry()`.
    #[inline]
    fn insert_entry(&mut self, hash: u32, entry: LdmEntry) {
        let bucket_size_log = self.params.bucket_size_log;
        let h = hash as usize;
        let offset = self.bucket_offsets[h];
        self.hash_table[(h << bucket_size_log) + offset as usize] = entry;
        self.bucket_offsets[h] = ((u32::from(offset) + 1) & ((1u32 << bucket_size_log) - 1)) as u8;
    }

    /// The hash (bucket) and checksum of the `minMatchLength` bytes at
    /// `split` of `buf`.
    #[inline]
    fn hash_split(&self, buf: &[u8], split: usize) -> (u32, u32) {
        let p = &self.params;
        let h_bits = p.hash_log - p.bucket_size_log;
        let xxhash = xxh64(&buf[split..split + p.min_match_length as usize]);
        let hash = (xxhash as u32) & ((1u32 << h_bits) - 1);
        (hash, (xxhash >> 32) as u32)
    }

    /// The LDM part of `ZSTD_loadDictionaryContent()` for a raw-content
    /// dictionary loaded into a context: the content (at most the last
    /// `ZSTD_CURRENT_MAX - 2` bytes) becomes the window's first segment and
    /// fills the table (`ZSTD_ldm_fillHashTable`).
    pub(crate) fn load_dictionary(&mut self, cd: &Arc<CDict>) {
        let buf = &cd.content[..];
        let end = buf.len();
        let max_dict_size = (CURRENT_MAX - WINDOW_START_INDEX) as usize;
        let start = end.saturating_sub(max_dict_size);
        debug_assert!(self.window.is_empty());
        self.window.update(start, end - start, false);
        // the window's positions are now in the dictionary's buffer
        self.window.base_in_dict = true;
        self.dict = Some(cd.clone());
        self.loaded_dict_end = self.window.index(end);
        self.fill_hash_table(buf, start, end);
    }

    /// `ZSTD_ldm_fillHashTable()`: inserts the split points of
    /// `buf[ip..iend]` (in the window).
    fn fill_hash_table(&mut self, buf: &[u8], mut ip: usize, iend: usize) {
        let min_match_length = self.params.min_match_length as usize;
        let istart = ip;
        let mut gear = Gear::new(&self.params);
        let mut splits = self.splits;
        while ip < iend {
            let mut n_splits = 0;
            let hashed = gear.feed(&buf[ip..iend], &mut splits, &mut n_splits);
            for &s in &splits[..n_splits] {
                if ip + s >= istart + min_match_length {
                    let split = ip + s - min_match_length;
                    let (hash, checksum) = self.hash_split(buf, split);
                    let entry = LdmEntry {
                        offset: self.window.index(split),
                        checksum,
                    };
                    self.insert_entry(hash, entry);
                }
            }
            ip += hashed;
        }
    }

    /// `ZSTD_ldm_reduceTable()`.
    fn reduce_table(&mut self, reducer_value: u32) {
        for e in self.hash_table.iter_mut() {
            e.offset = e.offset.saturating_sub(reducer_value);
        }
    }

    /// `ZSTD_ldm_generateSequences()`: fills [`seqs`](Self::seqs) with the
    /// long matches of `buf[start..end]` (in the window), by chunks of
    /// 1 MiB (one for a block), each after the LDM window's overflow
    /// correction and maximum distance.
    ///
    /// # Errors
    ///
    /// [`Error::OutputTooSmall`] if the sequences overflow the store
    /// (libzstd's `dstSize_tooSmall`; not possible for a block).
    pub(crate) fn generate_sequences(
        &mut self,
        buf: &[u8],
        start: usize,
        end: usize,
    ) -> Result<()> {
        const K_MAX_CHUNK_SIZE: usize = 1 << 20;
        let max_dist = 1u32 << self.params.window_log;
        let src_size = end - start;
        let nb_chunks = src_size.div_ceil(K_MAX_CHUNK_SIZE);
        let mut leftover_size = 0usize;
        self.seqs.seq.clear();
        self.seqs.pos = 0;
        self.seqs.pos_in_sequence = 0;
        let mut chunk = 0;
        while chunk < nb_chunks && self.seqs.seq.len() < self.seqs.capacity {
            let chunk_start = start + chunk * K_MAX_CHUNK_SIZE;
            let chunk_end = end.min(chunk_start + K_MAX_CHUNK_SIZE);
            let chunk_size = chunk_end - chunk_start;
            let prev_size = self.seqs.seq.len();
            // 1. Perform overflow correction if necessary.
            if self.window.need_overflow_correction(chunk_end) {
                let correction = self.window.correct_overflow(0, max_dist, chunk_start);
                self.reduce_table(correction);
                // invalidate dictionaries on overflow correction
                self.loaded_dict_end = 0;
            }
            // 2. We enforce the maximum offset allowed (any offset used must
            //    be valid at the END of the sequence, which may be split).
            let mut no_dms = false;
            self.window.enforce_max_dist(
                chunk_end,
                max_dist,
                &mut self.loaded_dict_end,
                &mut no_dms,
            );
            // 3. Generate the sequences for the chunk.
            let new_leftover_size =
                self.generate_sequences_internal(buf, chunk_start, chunk_end)?;
            // 4. Add the leftover literals from previous iterations to the
            //    first newly generated sequence, or add the
            //    `newLeftoverSize` if none are generated.
            if prev_size < self.seqs.seq.len() {
                self.seqs.seq[prev_size].lit_length += leftover_size as u32;
                leftover_size = new_leftover_size;
            } else {
                debug_assert!(new_leftover_size == chunk_size);
                leftover_size += chunk_size;
            }
            chunk += 1;
        }
        Ok(())
    }

    /// `ZSTD_ldm_generateSequences_internal()`: the long matches of
    /// `buf[istart..iend]`; returns the number of literals after the last.
    fn generate_sequences_internal(
        &mut self,
        buf: &[u8],
        istart: usize,
        iend: usize,
    ) -> Result<usize> {
        let p = self.params;
        let w = self.window;
        // the extDict's buffer: the dictionary's, or `buf` (a wrapped ring)
        let dict = self.dict.clone();
        let ext: &[u8] = match &dict {
            Some(d) if w.dict_base_in_dict => &d.content,
            _ => buf,
        };
        let ext_dict = w.has_ext_dict();
        let min_match_length = p.min_match_length as usize;
        let ents_per_bucket = 1usize << p.bucket_size_log;
        // Prefix and extDict parameters (positions in `buf` and `ext`)
        let dict_limit = w.dict_limit;
        let lowest_index = if ext_dict { w.low_limit } else { dict_limit };
        let dict_start = w.dict_base.wrapping_add(lowest_index as usize);
        let dict_end = w.dict_base.wrapping_add(dict_limit as usize);
        let low_prefix = w.base.wrapping_add(dict_limit as usize);
        // Input bounds
        let ilimit = iend.saturating_sub(HASH_READ_SIZE);
        // Input positions
        let mut anchor = istart;
        let mut ip = istart;

        if iend - istart < min_match_length {
            return Ok(iend - anchor);
        }
        debug_assert!(low_prefix <= istart && iend <= buf.len());

        // Initialize the rolling hash state with the first minMatchLength
        // bytes
        let mut gear = Gear::new(&p);
        gear.reset(&buf[ip..ip + min_match_length]);
        ip += min_match_length;

        let mut splits = self.splits;
        let mut candidates = self.candidates;
        while ip < ilimit {
            let mut n_splits = 0;
            let hashed = gear.feed(&buf[ip..ilimit], &mut splits, &mut n_splits);

            for n in 0..n_splits {
                let split = ip + splits[n] - min_match_length;
                let (hash, checksum) = self.hash_split(buf, split);
                candidates[n] = (split, hash, checksum);
            }

            for &(split, hash, checksum) in &candidates[..n_splits] {
                let new_entry = LdmEntry {
                    offset: w.index(split),
                    checksum,
                };

                // If a split point would generate a sequence overlapping
                // with the previous one, we merely register it in the hash
                // table and move on
                if split < anchor {
                    self.insert_entry(hash, new_entry);
                    continue;
                }

                // (forward length, backward length, entry offset)
                let mut best: Option<(usize, usize, u32)> = None;
                let mut best_match_length = 0;
                let bucket = (hash as usize) << p.bucket_size_log;
                for cur in &self.hash_table[bucket..bucket + ents_per_bucket] {
                    if cur.checksum != checksum || cur.offset <= lowest_index {
                        continue;
                    }
                    let (cur_forward, cur_backward) = if ext_dict && cur.offset < dict_limit {
                        // In the extDict: count on into the prefix from the
                        // extDict's end (ZSTD_count_2segments), and
                        // backwards within the extDict.
                        let p_match = w.dict_base.wrapping_add(cur.offset as usize);
                        let v_end = iend.min(split + (dict_end - p_match));
                        let mut fwd = common_prefix(&buf[split..v_end], &ext[p_match..dict_end]);
                        if p_match + fwd == dict_end {
                            fwd += common_prefix(&buf[split + fwd..iend], &buf[low_prefix..iend]);
                        }
                        if fwd < min_match_length {
                            continue;
                        }
                        let bwd = count_backwards(buf, split, anchor, ext, p_match, dict_start);
                        (fwd, bwd)
                    } else {
                        let p_match = w.base.wrapping_add(cur.offset as usize);
                        debug_assert!(p_match < split);
                        let fwd = common_prefix(&buf[split..iend], &buf[p_match..iend]);
                        if fwd < min_match_length {
                            continue;
                        }
                        let mut bwd = count_backwards(buf, split, anchor, buf, p_match, low_prefix);
                        // ZSTD_ldm_countBackwardsMatch_2segments: on
                        // reaching the prefix start, go on from the
                        // extDict's end
                        if ext_dict && p_match - bwd == low_prefix {
                            bwd += count_backwards(
                                buf,
                                split - bwd,
                                anchor,
                                ext,
                                dict_end,
                                dict_start,
                            );
                        }
                        (fwd, bwd)
                    };
                    let cur_total = cur_forward + cur_backward;
                    if cur_total > best_match_length {
                        best_match_length = cur_total;
                        best = Some((cur_forward, cur_backward, cur.offset));
                    }
                }

                // No match found -- insert an entry into the hash table and
                // process the next candidate match
                let Some((forward_match_length, backward_match_length, best_offset)) = best else {
                    self.insert_entry(hash, new_entry);
                    continue;
                };

                // Match found
                let offset = w.index(split) - best_offset;
                let m_length = forward_match_length + backward_match_length;
                if self.seqs.seq.len() == self.seqs.capacity {
                    // Out of sequence storage
                    return Err(Error::OutputTooSmall);
                }
                self.seqs.seq.push(RawSeq {
                    offset,
                    lit_length: (split - backward_match_length - anchor) as u32,
                    match_length: m_length as u32,
                });

                // Insert the current entry into the hash table --- it must
                // be done after the previous block to avoid clobbering
                // bestEntry
                self.insert_entry(hash, new_entry);

                anchor = split + forward_match_length;

                // A match that ends after the data hashed is a repeating,
                // overlapping pattern (e.g. all zeros): all its repetitions
                // would match, only the first one is needed. Skip over it.
                if anchor > ip + hashed {
                    gear.reset(&buf[anchor - min_match_length..anchor]);
                    // Continue the outer loop at anchor (ip + hashed == anchor).
                    ip = anchor - hashed;
                    break;
                }
            }

            ip += hashed;
        }

        Ok(iend - anchor)
    }
}

/// `ZSTD_ldm_limitTableUpdate()`: after a long match, only update the
/// tables a limited amount.
fn limit_table_update(ms: &mut MatchState, anchor: usize) {
    let curr = ms.window.index(anchor);
    if curr > ms.next_to_update + 1024 {
        ms.next_to_update = curr - 512u32.min(curr - ms.next_to_update - 1024);
    }
}

/// `ZSTD_ldm_fillFastTables()`: fills the `fast` and `dfast` tables up to
/// `end` (the other strategies fill theirs in their block compressors).
fn fill_fast_tables(ms: &mut MatchState, buf: &[u8], end: usize) {
    match ms.cparams.strategy {
        Strategy::Fast => ms.fill_hash_table(buf, end, TableLoadMethod::Fast),
        Strategy::DFast if end >= HASH_READ_SIZE => {
            super::dfast::fill_double_hash_table(ms, buf, end, TableLoadMethod::Fast);
        }
        _ => {}
    }
}

/// `ZSTD_buildSeqStore()` with LDM: as
/// [`build_seq_store`](super::fast::build_seq_store), with the block's long
/// matches, generated here into `ldm`.
///
/// # Errors
///
/// As [`LdmState::generate_sequences`].
pub(crate) fn build_seq_store_ldm(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    ldm: &mut LdmState,
    buf: &[u8],
    start: usize,
    end: usize,
) -> Result<bool> {
    debug_assert!(end - start <= super::params::BLOCKSIZE_MAX);
    if end - start < MIN_SEQ_STORE_BLOCK {
        // (libzstd skips the block in its external sequences, which are
        // empty with LDM)
        return Ok(false);
    }
    ss.reset();
    // limited update after a very long match
    let curr = ms.window.index(start);
    if curr > ms.next_to_update + 384 {
        ms.next_to_update = curr - 192u32.min(curr - ms.next_to_update - 384);
    }
    ldm.generate_sequences(buf, start, end)?;
    let last_ll_size = ldm_block_compress(&mut ldm.seqs, ms, ss, rep, buf, start, end);
    debug_assert!(ldm.seqs.pos == ldm.seqs.seq.len());
    ss.store_last_literals(&buf[end - last_ll_size..end]);
    Ok(true)
}

/// `ZSTD_buildSeqStore()` with external sequences (`cctx->externSeqStore`,
/// `ZSTD_referenceExternalSequences`): the long matches of a whole job of
/// the multithreaded compressor, generated serially before the job (see
/// the `mt` module), consumed block by block. Once they are all consumed,
/// as [`build_seq_store`](super::fast::build_seq_store). `view` is scratch
/// space.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_seq_store_ext(
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    raw: &mut RawSeqStore,
    view: &mut Vec<RawSeq>,
    buf: &[u8],
    start: usize,
    end: usize,
) -> bool {
    let size = end - start;
    let opt = ms.cparams.strategy >= Strategy::BtOpt;
    if size < MIN_SEQ_STORE_BLOCK {
        if opt {
            raw.skip_bytes(size);
        } else {
            raw.skip_sequences(size, ms.cparams.min_match);
        }
        return false;
    }
    if raw.pos >= raw.seq.len() {
        return super::fast::build_seq_store(ms, ss, rep, buf, start, end);
    }
    ss.reset();
    // limited update after a very long match
    let curr = ms.window.index(start);
    if curr > ms.next_to_update + 384 {
        ms.next_to_update = curr - 192u32.min(curr - ms.next_to_update - 384);
    }
    let last_ll_size = if opt {
        // The parser reads a copy of the store from `pos` and
        // `posInSequence`: the sequences reaching into the block, the first
        // one without its `posInSequence` bytes (the same candidates).
        view.clear();
        let mut covered = 0usize;
        for (i, &s) in raw.seq[raw.pos..].iter().enumerate() {
            let mut s = s;
            if i == 0 {
                let pis = raw.pos_in_sequence as u32;
                let lits = s.lit_length.min(pis);
                s.lit_length -= lits;
                s.match_length -= pis - lits;
            }
            covered += (s.lit_length + s.match_length) as usize;
            view.push(s);
            if covered > size {
                break;
            }
        }
        let n = super::opt::compress_block_opt_ldm(ms, ss, rep, buf, start, end, view);
        raw.skip_bytes(size);
        n
    } else {
        ldm_block_compress(raw, ms, ss, rep, buf, start, end)
    };
    ss.store_last_literals(&buf[end - last_ll_size..end]);
    true
}

/// `ZSTD_ldm_blockCompress()`: compresses the block `buf[start..end]` with
/// the long matches of `raw`; returns the number of trailing literals.
fn ldm_block_compress(
    raw: &mut RawSeqStore,
    ms: &mut MatchState,
    ss: &mut SeqStore,
    rep: &mut [u32; 3],
    buf: &[u8],
    start: usize,
    end: usize,
) -> usize {
    let min_match = ms.cparams.min_match;
    // If using opt parser, use LDMs only as candidates rather than always
    // accepting them
    if ms.cparams.strategy >= Strategy::BtOpt {
        debug_assert!(raw.pos == 0 && raw.pos_in_sequence == 0);
        let last_ll_size =
            super::opt::compress_block_opt_ldm(ms, ss, rep, buf, start, end, &raw.seq);
        raw.skip_bytes(end - start);
        return last_ll_size;
    }

    let block_compressor = super::fast::select_block_compressor(ms);
    let mut ip = start;
    // Loop through each sequence and apply the block compressor to the
    // literals
    while raw.pos < raw.seq.len() && ip < end {
        // maybe_split_sequence updates raw.pos
        let sequence = raw.maybe_split_sequence((end - ip) as u32, min_match);
        // End signal
        if sequence.offset == 0 {
            break;
        }
        debug_assert!(ip + (sequence.lit_length + sequence.match_length) as usize <= end);

        // Fill tables for block compressor
        limit_table_update(ms, ip);
        fill_fast_tables(ms, buf, ip);
        // Run the block compressor
        let lit_end = ip + sequence.lit_length as usize;
        let new_lit_length = block_compressor(ms, ss, rep, buf, ip, lit_end);
        ip = lit_end;
        // Update the repcodes
        rep[2] = rep[1];
        rep[1] = rep[0];
        rep[0] = sequence.offset;
        // Store the sequence
        ss.store_seq(
            buf,
            ip - new_lit_length,
            new_lit_length,
            end,
            offset_to_offbase(sequence.offset),
            sequence.match_length as usize,
        );
        ip += sequence.match_length as usize;
    }
    // Fill the tables for the block compressor
    limit_table_update(ms, ip);
    fill_fast_tables(ms, buf, ip);
    // Compress the last literals
    block_compressor(ms, ss, rep, buf, ip, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::fast::tests::{MIRI, miri_pick, random, texty};
    use crate::enc::params::one_shot_cparams;
    use crate::{EndOp, StreamEncoder};
    use alloc::vec::Vec;

    #[test]
    fn params() {
        // level 22, 128 MiB window: btultra2
        let p = LdmParams::new(&one_shot_cparams(22, 100 << 20));
        assert_eq!(
            p,
            LdmParams {
                hash_log: 23,
                bucket_size_log: 8,
                min_match_length: 32,
                hash_rate_log: 4,
                window_log: 27,
            }
        );
        // fast with a small window: the minimum hash log, bucket size 16
        let mut cp = one_shot_cparams(1, 1 << 20);
        cp.window_log = 10;
        let p = LdmParams::new(&cp);
        assert_eq!(
            (p.hash_log, p.bucket_size_log, p.min_match_length),
            (6, 4, 64)
        );
        assert_eq!(p.hash_rate_log, 7);
        // btultra2 with a small window: buckets cut to the table
        let mut cp = one_shot_cparams(19, 1 << 20);
        cp.window_log = 10;
        let p = LdmParams::new(&cp);
        assert_eq!((p.hash_log, p.bucket_size_log), (6, 6));
        assert_eq!(p.max_nb_seq(1 << 17), 4096);
    }

    #[test]
    fn gear_mask() {
        let p = LdmParams::new(&one_shot_cparams(22, 100 << 20));
        let g = Gear::new(&p);
        // 4 bits at the top of a 32-bit window
        assert_eq!(g.stop_mask, 0xF << 28);
        assert_eq!(g.rolling, 0xFFFF_FFFF);
        // splits come every 16 bytes on average
        let data = random(1 << 16, 3);
        let mut g = Gear::new(&p);
        let mut splits = [0; LDM_BATCH_SIZE];
        let (mut total, mut pos) = (0, 0);
        while pos < data.len() {
            let mut n = 0;
            pos += g.feed(&data[pos..], &mut splits, &mut n);
            total += n;
        }
        assert!((3000..5200).contains(&total), "{total} splits");
    }

    fn store(seqs: &[(u32, u32)]) -> RawSeqStore {
        RawSeqStore {
            seq: seqs
                .iter()
                .map(|&(lit_length, match_length)| RawSeq {
                    offset: 100,
                    lit_length,
                    match_length,
                })
                .collect(),
            pos: 0,
            pos_in_sequence: 0,
            capacity: seqs.len(),
        }
    }

    #[test]
    fn skip_sequences() {
        let mut s = store(&[(10, 50), (5, 40)]);
        s.skip_sequences(4, 32);
        assert_eq!((s.pos, s.seq[0].lit_length), (0, 6));
        // into the match: 50 - 20 = 30 < 32 bytes left, dropped (its rest
        // becomes literals of the next one)
        s.skip_sequences(26, 32);
        assert_eq!((s.pos, s.seq[1].lit_length), (1, 35));
        s.skip_sequences(1000, 32);
        assert_eq!(s.pos, 2);
    }

    #[test]
    fn split_sequence() {
        let mut s = store(&[(10, 50), (5, 40)]);
        // whole
        assert_eq!(s.maybe_split_sequence(60, 32).match_length, 50);
        assert_eq!(s.pos, 1);
        // cut in the match, long enough
        let q = s.maybe_split_sequence(40, 32);
        assert_eq!((q.offset, q.lit_length, q.match_length), (100, 5, 35));
        // (the 5 bytes left of the match are too short: dropped)
        assert_eq!((s.pos, s.seq[1].match_length), (2, 5));
        // cut in the literals
        let mut s = store(&[(10, 50)]);
        assert_eq!(s.maybe_split_sequence(10, 32).offset, 0);
        assert_eq!(s.seq[0].lit_length, 0);
    }

    #[test]
    fn skip_bytes() {
        let mut s = store(&[(10, 50), (5, 40)]);
        s.skip_bytes(30);
        assert_eq!((s.pos, s.pos_in_sequence), (0, 30));
        s.skip_bytes(30);
        assert_eq!((s.pos, s.pos_in_sequence), (1, 0));
        s.skip_bytes(44);
        assert_eq!((s.pos, s.pos_in_sequence), (1, 44));
        s.skip_bytes(1);
        assert_eq!((s.pos, s.pos_in_sequence), (2, 0));
    }

    /// Text with copies from up to `reach` bytes back.
    fn input(n: usize, reach: usize) -> Vec<u8> {
        let mut v = texty(50_000.min(n / 4), 1);
        let mut k = 0u64;
        while v.len() < n {
            k += 1;
            let len = 3000 + (k as usize * 7919) % 20_000;
            if k.is_multiple_of(3) {
                v.extend_from_slice(&texty(len, k));
            } else {
                let back = (len + (k as usize * 104_729) % reach).min(v.len());
                let at = v.len() - back;
                v.extend_from_within(at..at + len.min(back));
            }
        }
        v.truncate(n);
        v
    }

    /// Every level with LDM forced on: one-shot, and streamed through a
    /// wrapping input buffer (extDict); the frames round trip and LDM
    /// helps the fast levels (not checked under Miri, where the input
    /// and the windows are smaller, and fewer levels run).
    #[test]
    fn round_trip_forced() {
        let data = if MIRI {
            input(6_000, 3_000)
        } else {
            input(1 << 20, 400_000)
        };
        // Miri: one window per level, each across the levels (the 2 KiB
        // window's input buffer wraps)
        let (levels, windows): (&[i32], &[(u32, usize)]) = if MIRI {
            (
                &[1, 3, 5, 8, 13, 16, 19, 22],
                &[(17, data.len()), (11, 1500)],
            )
        } else {
            (
                &[-3, 1, 2, 3, 4, 5, 6, 8, 13, 15, 16, 17, 18, 19, 22],
                &[(20, data.len()), (17, 70_000)],
            )
        };
        for (i, &level) in levels.iter().enumerate() {
            for &(wlog, piece) in miri_pick(windows, i) {
                let mut enc = StreamEncoder::new(level);
                enc.set_long_distance_matching(true);
                enc.set_window_log(wlog);
                let mut out = alloc::vec![0u8; crate::compress_bound(data.len())];
                let mut written = 0;
                for (k, p) in data.chunks(piece).enumerate() {
                    let last = (k + 1) * piece >= data.len();
                    let op = if last { EndOp::End } else { EndOp::Continue };
                    let r = enc.compress(p, &mut out[written..], op).unwrap();
                    assert_eq!(r.read, p.len());
                    written += r.written;
                }
                let c = &out[..written];
                assert!(
                    crate::decompress(c).unwrap() == data,
                    "L{level} wlog {wlog}"
                );
                if level <= 2 && wlog == 20 {
                    let plain = crate::compress(&data, level).unwrap();
                    assert!(
                        c.len() < plain.len(),
                        "L{level}: {} vs {}",
                        c.len(),
                        plain.len()
                    );
                }
            }
        }
    }
}
