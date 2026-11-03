//! XXH64, the content checksum of zstd frames (the low 32 bits are stored).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline(always)]
fn read64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

#[inline(always)]
fn read32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

#[inline(always)]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

#[inline(always)]
fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

/// Streaming XXH64 with seed 0.
#[derive(Clone)]
pub(crate) struct Xxh64 {
    v: [u64; 4],
    buf: [u8; 32],
    buf_len: usize,
    total: u64,
}

impl core::fmt::Debug for Xxh64 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Xxh64")
            .field("total", &self.total)
            .finish_non_exhaustive()
    }
}

impl Default for Xxh64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Xxh64 {
    pub(crate) const fn new() -> Self {
        Xxh64 {
            v: [P1.wrapping_add(P2), P2, 0, 0u64.wrapping_sub(P1)],
            buf: [0; 32],
            buf_len: 0,
            total: 0,
        }
    }

    #[inline]
    fn stripes(v: &mut [u64; 4], data: &[u8]) {
        let (mut a, mut b, mut c, mut d) = (v[0], v[1], v[2], v[3]);
        for s in data.as_chunks::<32>().0 {
            a = round(a, read64(&s[0..]));
            b = round(b, read64(&s[8..]));
            c = round(c, read64(&s[16..]));
            d = round(d, read64(&s[24..]));
        }
        *v = [a, b, c, d];
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buf_len > 0 {
            let take = (32 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 32 {
                return;
            }
            let buf = self.buf;
            Self::stripes(&mut self.v, &buf);
            self.buf_len = 0;
        }
        let full = data.len() & !31;
        Self::stripes(&mut self.v, &data[..full]);
        let rest = &data[full..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.buf_len = rest.len();
    }

    pub(crate) fn digest(&self) -> u64 {
        let mut h = if self.total >= 32 {
            let [a, b, c, d] = self.v;
            let mut h = a
                .rotate_left(1)
                .wrapping_add(b.rotate_left(7))
                .wrapping_add(c.rotate_left(12))
                .wrapping_add(d.rotate_left(18));
            h = merge_round(h, a);
            h = merge_round(h, b);
            h = merge_round(h, c);
            merge_round(h, d)
        } else {
            self.v[2].wrapping_add(P5)
        };
        h = h.wrapping_add(self.total);
        finalize(h, &self.buf[..self.buf_len])
    }
}

fn finalize(mut h: u64, mut rest: &[u8]) -> u64 {
    while rest.len() >= 8 {
        h ^= round(0, read64(rest));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        rest = &rest[8..];
    }
    if rest.len() >= 4 {
        h ^= u64::from(read32(rest)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        rest = &rest[4..];
    }
    for &b in rest {
        h ^= u64::from(b).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// XXH64 of `data` with seed 0.
pub fn xxh64(data: &[u8]) -> u64 {
    let mut s = Xxh64::new();
    s.update(data);
    s.digest()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(xxh64(b""), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a"), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc"), 0x44BC_2CF5_AD77_0999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition"),
            0xFBCE_A83C_8A37_8BF1
        );
    }

    #[test]
    fn streaming_matches_oneshot() {
        let data: alloc::vec::Vec<u8> = (0..1000u32).map(|i| (i * 7 + i / 13) as u8).collect();
        for split in [0, 1, 5, 31, 32, 33, 64, 100, 999, 1000] {
            let mut s = Xxh64::new();
            s.update(&data[..split]);
            s.update(&data[split..]);
            assert_eq!(s.digest(), xxh64(&data), "split {split}");
        }
    }
}
