//! Shared by the fuzz targets: structure-aware parameters (`arbitrary`),
//! the C libzstd oracle, dictionaries.
//!
//! The targets never panic on libzstd's errors: an input libzstd rejects
//! (a corrupt dictionary, an impossible parameter) is just uninteresting.
//! They panic on zstd-native panics (caught by the fuzzer as crashes), on
//! output that differs from libzstd's, and on zstd-native accepting what
//! libzstd rejects.

use std::borrow::Cow;
use std::sync::LazyLock;

use arbitrary::Arbitrary;
use zstd::zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd::zstd_safe::{CCtx, CDict, CParameter, DCtx, DDict, DParameter, InBuffer, OutBuffer};
use zstd_native::EndOp;

/// Output cap when decoding arbitrary bytes: an RLE block expands 3 bytes
/// to 128 KiB, so a 64 KiB input can legitimately mean gigabytes.
pub const OUT_CAP: usize = 8 << 20;

/// Longest dictionary taken from the fuzz input.
pub const DICT_MAX: usize = 4 << 10;

/// Deterministic xorshift generator.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Synthetic log-like samples: what the trained dictionary is good for.
pub fn samples(seed: u64, n: usize) -> Vec<Vec<u8>> {
    let mut r = Rng::new(seed);
    let words = [
        "GET",
        "POST",
        "/api/v1/users",
        "/api/v1/orders",
        "/static/app.js",
        "/health",
        "200",
        "404",
        "500",
        "Mozilla/5.0",
        "curl/8.5.0",
        "ms",
        "id",
        "name",
        "tags",
        "value",
        "true",
        "false",
        "null",
        "error",
        "warning",
        "info",
    ];
    (0..n)
        .map(|_| {
            let mut s = String::new();
            for _ in 0..8 + r.below(40) {
                s.push_str(words[r.below(words.len() as u64) as usize]);
                s.push(if r.below(3) == 0 { '\n' } else { ' ' });
                if r.below(4) == 0 {
                    s.push_str(&r.below(100_000).to_string());
                }
            }
            s.into_bytes()
        })
        .collect()
}

/// A dictionary trained with `ZDICT_trainFromBuffer` (zstd-format, ~8 KiB).
pub fn trained_dict() -> &'static [u8] {
    static DICT: LazyLock<Vec<u8>> =
        LazyLock::new(|| zstd::dict::from_samples(&samples(1, 400), 8 << 10).expect("ZDICT"));
    &DICT
}

/// Where a dictionary comes from.
#[derive(Arbitrary, Debug, Clone)]
pub enum DictSpec {
    None,
    /// Bytes from the fuzz input (raw content, or a zstd dictionary when
    /// they start with the magic number: both libraries must then agree).
    Bytes(Vec<u8>),
    /// The trained dictionary.
    Trained,
    /// The trained dictionary with bytes overwritten: the loaders' checks.
    Mutated(Vec<(u16, u8)>),
}

impl DictSpec {
    /// The dictionary bytes, if any.
    pub fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            DictSpec::None => None,
            DictSpec::Bytes(b) => Some(Cow::Borrowed(&b[..b.len().min(DICT_MAX)])),
            DictSpec::Trained => Some(Cow::Borrowed(trained_dict())),
            DictSpec::Mutated(edits) => {
                let mut d = trained_dict().to_vec();
                for &(pos, v) in edits.iter().take(8) {
                    let p = pos as usize % d.len();
                    d[p] = v;
                }
                Some(Cow::Owned(d))
            }
        }
    }
}

/// `n` as a compression level, -7 to 22.
pub fn level_from(n: u8) -> i32 {
    (n % 30) as i32 - 7
}

/// `n` as a window log override: 0 (the level's) or 10 to 23 (small enough
/// to keep every iteration's memory small).
pub fn window_log_from(n: u8) -> u32 {
    match n % 15 {
        0 => 0,
        k => 9 + k as u32,
    }
}

/// A chunk size list from the fuzz input: never empty, never zero.
pub fn chunk_sizes(v: &[u16]) -> Vec<usize> {
    let mut out: Vec<usize> = v.iter().take(32).map(|&n| n.max(1) as usize).collect();
    if out.is_empty() {
        out.push(4096);
    }
    out
}

pub fn directive(op: EndOp) -> ZSTD_EndDirective {
    match op {
        EndOp::Continue => ZSTD_EndDirective::ZSTD_e_continue,
        EndOp::Flush => ZSTD_EndDirective::ZSTD_e_flush,
        EndOp::End => ZSTD_EndDirective::ZSTD_e_end,
    }
}

pub fn c_error(code: usize) -> &'static str {
    zstd::zstd_safe::get_error_name(code)
}

/// One-shot libzstd decompression (`ZSTD_decompressDCtx`) into at most
/// `cap` bytes, with an optional dictionary: `Ok(bytes)`, or the error
/// name.
pub fn c_decompress(src: &[u8], cap: usize, dict: Option<&[u8]>) -> Result<Vec<u8>, &'static str> {
    let mut d = DCtx::create();
    if let Some(dict) = dict {
        d.load_dictionary(dict).map_err(c_error)?;
    }
    let mut out = Vec::with_capacity(cap);
    d.decompress(&mut out, src).map_err(c_error)?;
    Ok(out)
}

/// What a streaming decode did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamResult {
    pub output: Vec<u8>,
    /// All input was consumed and the decoder is between frames.
    pub complete: bool,
    /// The error name, if the decoder rejected the input.
    pub error: Option<String>,
    /// Stopped because the output reached the cap (inconclusive).
    pub capped: bool,
}

/// libzstd's streaming decoder (`ZSTD_decompressStream`) fed `input` in
/// `in_sizes` pieces, with `out_sizes` output buffers, cycling through
/// both lists.
pub fn c_stream_decode(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    dict: Option<&[u8]>,
    window_log_max: u32,
    cap: usize,
) -> StreamResult {
    let mut res = StreamResult {
        output: Vec::new(),
        complete: false,
        error: None,
        capped: false,
    };
    let mut d = DCtx::create();
    if window_log_max != 0 {
        d.set_parameter(DParameter::WindowLogMax(window_log_max))
            .expect("ZSTD_d_windowLogMax");
    }
    if let Some(dict) = dict
        && let Err(e) = d.load_dictionary(dict)
    {
        res.error = Some(format!("load_dictionary: {}", c_error(e)));
        return res;
    }
    let mut buf = vec![0u8; *out_sizes.iter().max().unwrap()];
    let mut pos = 0;
    let mut i = 0;
    // 0 once a frame is fully decoded and flushed (libzstd's return value).
    let mut last_code = 1;
    // Every piece, then empty calls until no output is pending.
    loop {
        let n = in_sizes[i % in_sizes.len()].min(input.len() - pos);
        let mut inb = InBuffer::around(&input[pos..pos + n]);
        loop {
            let o = out_sizes[i % out_sizes.len()];
            i += 1;
            let before = inb.pos();
            let mut outb = OutBuffer::around(&mut buf[..o]);
            match d.decompress_stream(&mut outb, &mut inb) {
                Ok(code) => {
                    let w = outb.pos();
                    if n != 0 || w != 0 {
                        last_code = code;
                    }
                    res.output.extend_from_slice(&buf[..w]);
                    if res.output.len() >= cap {
                        res.capped = true;
                        return res;
                    }
                    if inb.pos() == n && (n != 0 || w == 0) {
                        break;
                    }
                    assert!(
                        w != 0 || inb.pos() != before,
                        "libzstd made no progress on {} input bytes",
                        n - before
                    );
                }
                Err(e) => {
                    res.error = Some(c_error(e).to_string());
                    return res;
                }
            }
        }
        pos += n;
        if n == 0 {
            break;
        }
    }
    res.complete = last_code == 0;
    res
}

/// A dictionary for the encoders.
#[derive(Arbitrary, Debug, Clone)]
pub enum EncDict {
    None,
    /// `ZSTD_CCtx_loadDictionary`
    Loaded(DictSpec),
    /// `ZSTD_createCDict(dict, level)` + `ZSTD_CCtx_refCDict`
    Prepared(DictSpec),
}

impl EncDict {
    pub fn spec(&self) -> Option<&DictSpec> {
        match self {
            EncDict::None => None,
            EncDict::Loaded(s) | EncDict::Prepared(s) => Some(s),
        }
    }
}

/// Compression parameters both encoders take (`ZSTD_CCtx_setParameter`).
#[derive(Arbitrary, Debug, Clone)]
pub struct EncParams {
    pub level_n: u8,
    pub window_log_n: u8,
    pub ldm: bool,
    pub workers_n: u8,
    pub job_size_n: u8,
    pub overlap_log_n: u8,
    pub dict: EncDict,
}

impl EncParams {
    pub fn level(&self) -> i32 {
        level_from(self.level_n)
    }
    /// The window log override; never the level's own for long distance
    /// matching or levels 20-22 with an unknown size (a 128 MiB input ring
    /// per iteration).
    pub fn window_log(&self) -> u32 {
        match window_log_from(self.window_log_n) {
            0 if self.ldm || self.level() >= 20 => 20,
            w => w,
        }
    }
    pub fn workers(&self) -> u32 {
        (self.workers_n % 4) as u32
    }
    /// `ZSTD_c_jobSize`: automatic, or the smallest allowed. Never
    /// automatic with long distance matching and workers: libzstd then
    /// sizes jobs by the chain log (512 MiB at level 22) and allocates
    /// `(workers + 3) * jobSize` for its round buffer.
    pub fn job_size(&self) -> usize {
        match self.job_size_n % 3 {
            0 if self.ldm && self.workers() > 0 => 1 << 20,
            0 => 0,
            1 => 512 << 10,
            _ => 1 << 20,
        }
    }
    pub fn overlap_log(&self) -> u32 {
        (self.overlap_log_n % 10) as u32
    }

    /// Longest input for the level (the optimal parser is slow).
    pub fn data_cap(&self) -> usize {
        if self.level() >= 16 {
            48 << 10
        } else {
            256 << 10
        }
    }
}

/// Whether libzstd accepts the dictionary `p` asks for. A loaded
/// dictionary (`ZSTD_CCtx_loadDictionary`) is only parsed when the first
/// frame starts, with `ZSTD_createCDict_advanced2`, and a corrupt one is
/// then reported as a memory allocation error; so the check is
/// `ZSTD_createCDict` for both kinds.
pub fn c_accepts_dict(p: &EncParams) -> bool {
    match p.dict.spec().and_then(|s| s.bytes()) {
        Some(d) => CDict::try_create(&d, p.level()).is_some(),
        None => true,
    }
}

/// The prepared libzstd dictionary `p` asks for, if any (the dictionary
/// must be accepted, see [`c_accepts_dict`]).
pub fn c_cdict(p: &EncParams) -> Option<CDict<'static>> {
    match &p.dict {
        EncDict::Prepared(s) => s.bytes().map(|d| CDict::create(&d, p.level())),
        _ => None,
    }
}

/// A libzstd context with `p` applied (`cdict` from [`c_cdict`]); the
/// dictionary must be accepted (see [`c_accepts_dict`]).
pub fn c_context<'a>(p: &EncParams, cdict: Option<&'a CDict<'static>>) -> CCtx<'a> {
    let mut c = CCtx::create();
    c.set_parameter(CParameter::CompressionLevel(p.level()))
        .unwrap();
    c.set_parameter(CParameter::WindowLog(p.window_log()))
        .unwrap();
    c.set_parameter(CParameter::EnableLongDistanceMatching(p.ldm))
        .unwrap();
    c.set_parameter(CParameter::NbWorkers(p.workers())).unwrap();
    c.set_parameter(CParameter::JobSize(p.job_size() as u32))
        .unwrap();
    c.set_parameter(CParameter::OverlapSizeLog(p.overlap_log()))
        .unwrap();
    match &p.dict {
        EncDict::None => {}
        EncDict::Loaded(s) => {
            if let Some(d) = s.bytes() {
                c.load_dictionary(&d).expect("ZSTD_CCtx_loadDictionary");
            }
        }
        EncDict::Prepared(_) => {
            if let Some(cd) = cdict {
                c.ref_cdict(cd).expect("ZSTD_CCtx_refCDict");
            }
        }
    }
    c
}

/// Whether libzstd accepts `dict` for decompression.
pub fn c_accepts_ddict(dict: &[u8]) -> bool {
    DDict::try_create(dict).is_some()
}

/// Runs a streaming compression script on libzstd: `ops` are
/// `(input length, op, output buffer size)`.
pub fn c_stream_compress(
    c: &mut CCtx<'_>,
    data: &[u8],
    ops: &[(usize, EndOp, usize)],
    pledged: Option<u64>,
) -> Vec<u8> {
    if let Some(p) = pledged {
        c.set_pledged_src_size(Some(p)).unwrap();
    }
    let mut out = Vec::new();
    let mut pos = 0;
    let mut buf = Vec::new();
    for &(len, op, out_size) in ops {
        let piece = &data[pos..pos + len];
        pos += len;
        let mut input = InBuffer::around(piece);
        buf.resize(out_size, 0);
        loop {
            let mut output = OutBuffer::around(&mut buf[..]);
            let remaining = c
                .compress_stream2(&mut output, &mut input, directive(op))
                .unwrap_or_else(|e| panic!("libzstd: {}", c_error(e)));
            let n = output.pos();
            out.extend_from_slice(&buf[..n]);
            if input.pos() == piece.len() && (op == EndOp::Continue || remaining == 0) {
                break;
            }
        }
    }
    out
}

/// Builds a zstd-native streaming encoder with `p` applied: `None` if the
/// dictionary is rejected.
pub fn native_encoder(p: &EncParams) -> Option<zstd_native::StreamEncoder> {
    let mut e = zstd_native::StreamEncoder::new(p.level());
    match &p.dict {
        EncDict::None => {}
        EncDict::Loaded(s) => {
            if let Some(d) = s.bytes() {
                e.set_dictionary(&d).ok()?;
            }
        }
        EncDict::Prepared(s) => {
            if let Some(d) = s.bytes() {
                let cd = zstd_native::EncoderDictionary::new(&d, p.level()).ok()?;
                e.set_prepared_dictionary(&cd).unwrap();
            }
        }
    }
    e.set_window_log(p.window_log());
    e.set_long_distance_matching(p.ldm);
    e.set_workers(p.workers()).unwrap();
    e.set_job_size(p.job_size());
    e.set_overlap_log(p.overlap_log());
    Some(e)
}

/// Runs a streaming compression script on zstd-native (see
/// [`c_stream_compress`]).
pub fn native_stream_compress(
    e: &mut zstd_native::StreamEncoder,
    data: &[u8],
    ops: &[(usize, EndOp, usize)],
    pledged: Option<u64>,
) -> Vec<u8> {
    if let Some(p) = pledged {
        e.set_pledged_src_size(Some(p)).unwrap();
    }
    let mut out = Vec::new();
    let mut pos = 0;
    let mut buf = Vec::new();
    for &(len, op, out_size) in ops {
        let mut piece = &data[pos..pos + len];
        pos += len;
        buf.resize(out_size, 0);
        loop {
            let p = e
                .compress(piece, &mut buf[..], op)
                .unwrap_or_else(|e| panic!("zstd-native: {e:?}"));
            piece = &piece[p.read..];
            out.extend_from_slice(&buf[..p.written]);
            if piece.is_empty() && (op == EndOp::Continue || p.remaining == 0) {
                break;
            }
        }
    }
    out
}

/// A streaming script from the fuzz input's `raw` `(length, op, output
/// size)` triples (at most 12): input pieces, ops and output sizes, ending
/// the last frame with whatever is left of the data.
pub fn script_from(
    raw: &[(u32, u8, u16)],
    final_out: u16,
    data_len: usize,
) -> Vec<(usize, EndOp, usize)> {
    let mut ops = Vec::with_capacity(raw.len().min(12) + 1);
    let mut left = data_len;
    for &(len, kind, out) in raw.iter().take(12) {
        let len = (len as usize % (data_len + 1)).min(left);
        left -= len;
        let op = match kind % 5 {
            0 => EndOp::Flush,
            1 => EndOp::End,
            _ => EndOp::Continue,
        };
        ops.push((len, op, out.max(1) as usize));
    }
    ops.push((left, EndOp::End, final_out.max(1) as usize));
    ops
}

/// zstd-native's streaming decoder fed `input` in `in_sizes` pieces with
/// `out_sizes` output buffers (see [`c_stream_decode`]). Panics if a call
/// makes no progress.
pub fn native_stream_decode(
    input: &[u8],
    in_sizes: &[usize],
    out_sizes: &[usize],
    dict: Option<zstd_native::Dictionary>,
    window_log_max: u32,
    cap: usize,
) -> StreamResult {
    let mut res = StreamResult {
        output: Vec::new(),
        complete: false,
        error: None,
        capped: false,
    };
    let mut d = match dict {
        Some(d) => zstd_native::StreamDecoder::with_dictionary(d),
        None => zstd_native::StreamDecoder::new(),
    };
    if window_log_max != 0 {
        d.set_window_log_max(window_log_max);
    }
    let mut buf = vec![0u8; *out_sizes.iter().max().unwrap()];
    let mut pos = 0;
    let mut i = 0;
    // Every piece, then empty calls until no output is pending.
    loop {
        let n = in_sizes[i % in_sizes.len()].min(input.len() - pos);
        let mut piece = &input[pos..pos + n];
        loop {
            let o = out_sizes[i % out_sizes.len()];
            i += 1;
            match d.decode(piece, &mut buf[..o]) {
                Ok(p) => {
                    res.output.extend_from_slice(&buf[..p.written]);
                    if res.output.len() >= cap {
                        res.capped = true;
                        return res;
                    }
                    piece = &piece[p.read..];
                    if piece.is_empty() && (n != 0 || p.written == 0) {
                        break;
                    }
                    assert!(
                        p.written != 0 || p.read != 0,
                        "zstd-native made no progress on {} input bytes",
                        piece.len()
                    );
                }
                Err(e) => {
                    res.error = Some(format!("{e:?}"));
                    return res;
                }
            }
        }
        pos += n;
        if n == 0 {
            break;
        }
    }
    res.complete = d.at_frame_boundary();
    res
}

/// The content of the first frame of `ops` (its pledged size).
pub fn first_frame_len(ops: &[(usize, EndOp, usize)]) -> u64 {
    let mut n = 0u64;
    for &(len, op, _) in ops {
        n += len as u64;
        if op == EndOp::End {
            break;
        }
    }
    n
}

/// Hex dump of the first bytes, for assertion messages.
pub fn head(v: &[u8]) -> String {
    v.iter()
        .take(32)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where two byte strings first differ, for assertion messages.
pub fn first_diff(a: &[u8], b: &[u8]) -> String {
    match a.iter().zip(b).position(|(x, y)| x != y) {
        Some(i) => format!(
            "first difference at {i} (lengths {} / {})",
            a.len(),
            b.len()
        ),
        None => format!("lengths {} / {}", a.len(), b.len()),
    }
}
