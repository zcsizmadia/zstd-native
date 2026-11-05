# zstd-native

**Zstandard (libzstd 1.5.7), rewritten in Rust:** the same compressed bytes
as libzstd at every level, the same strictness on bad input, at or above
libzstd's speed, without the C, the build scripts or the memory-safety
risk. Sibling of [xz-native](https://github.com/zcsizmadia/xz-native).

[![CI](https://github.com/zcsizmadia/zstd-native/actions/workflows/ci.yml/badge.svg)](https://github.com/zcsizmadia/zstd-native/actions/workflows/ci.yml)
[![Fuzz](https://github.com/zcsizmadia/zstd-native/actions/workflows/fuzz.yml/badge.svg)](https://github.com/zcsizmadia/zstd-native/actions/workflows/fuzz.yml)
[![Miri](https://github.com/zcsizmadia/zstd-native/actions/workflows/miri.yml/badge.svg)](https://github.com/zcsizmadia/zstd-native/actions/workflows/miri.yml)
[![crates.io](https://img.shields.io/crates/v/zstd-native)](https://crates.io/crates/zstd-native)
[![docs.rs](https://img.shields.io/docsrs/zstd-native)](https://docs.rs/zstd-native)
[![MSRV](https://img.shields.io/crates/msrv/zstd-native)](https://crates.io/crates/zstd-native)
[![License](https://img.shields.io/crates/l/zstd-native)](LICENSE)
[![OpenSSF Scorecard](https://api.scorecard.dev/projects/github.com/zcsizmadia/zstd-native/badge)](https://scorecard.dev/viewer/?uri=github.com/zcsizmadia/zstd-native)

> **Status:** 0.1, the first release. Decompression and compression are
> complete and heavily tested; the encoder output is byte-identical to
> libzstd 1.5.7 for the same parameters. The API may still change before
> 1.0 (see [Versioning](#versioning)). See the
> [roadmap](docs/PROPOSAL.md) and the
> [issues](https://github.com/zcsizmadia/zstd-native/issues).

## Why zstd-native

- **A supply chain you can audit.** No C code, no build scripts and no
  binary blobs in the library: one dependency-free crate. C libzstd is
  used only by the benchmark and fuzz crates, as a speed and behaviour
  reference.
- **Memory safety on untrusted input.** Decompressors parse hostile data.
  `unsafe` is limited to benchmarked hot loops (bit readers, Huffman and
  sequence decoding, match finders) and the x86-64 assembly, each with a
  SAFETY argument, debug assertions and a safe fallback that the
  `portable` feature selects. It's fuzzed against libzstd and checked
  with Miri.
- **As fast as libzstd, or faster, and the fastest Rust zstd.** One core
  of an AMD EPYC 7543, geometric mean over the
  [Silesia corpus](https://sun.aei.polsl.pl/~sdeor/index.php?page=silesia),
  against the `zstd` crate's libzstd build (`-O3`, BMI2, asm Huffman
  decoder). Machines, methods and per-file tables are in
  [BENCHMARK.md](BENCHMARK.md):

  | zstd-native speed (1.25x = 25% faster) | vs C libzstd 1.5.7 | vs [structured-zstd](https://crates.io/crates/structured-zstd) 0.2.6 |
  |---|---:|---:|
  | Compression, level 1 | 1.02x | 1.06x |
  | Compression, level 3 (default) | 1.00x | 1.33x |
  | Compression, level 9 | 1.03x | 1.18x |
  | Compression, level 19 | 1.30x | 1.11x |
  | Decompression (frames of levels 1-19) | 1.05-1.09x | 1.20-1.29x |

  structured-zstd is the fastest other pure-Rust zstd. Its compressed
  output differs from libzstd's. [ruzstd](https://crates.io/crates/ruzstd)
  decompresses 3-7x slower than zstd-native.
- **Drop-in compatible.** It compresses to the same bytes as libzstd for
  the same parameters. That covers every level, streaming with flushes,
  dictionaries, long distance matching and multithreading. Anything it
  decompresses, libzstd decompresses to the same output.
- **Builds anywhere.** `cargo add` and done: no C compiler, simple
  cross-compilation, wasm, and `no_std` + `alloc`.

## Usage

```toml
[dependencies]
zstd-native = "0.1"
```

The full API reference is on [docs.rs](https://docs.rs/zstd-native);
changes are in [CHANGELOG.md](CHANGELOG.md).

```rust
let compressed = zstd_native::compress(&data, 3)?; // same bytes as libzstd level 3
let data = zstd_native::decompress(&compressed)?;
```

Reuse tables and buffers across calls, and write into your own buffer:

```rust
let mut c = zstd_native::Compressor::new(3);
let compressed = c.compress(&message)?;

let mut d = zstd_native::Decompressor::new();
let n = d.decompress_to_slice(&compressed, &mut buffer)?;
```

Streaming with `std::io`. The `Encoder` writes the same bytes as libzstd's
`ZSTD_compressStream2` for the same writes and flushes:

```rust
let mut out = Vec::new();
std::io::copy(&mut zstd_native::Decoder::new(file), &mut out)?;

let mut enc = zstd_native::Encoder::new(std::fs::File::create("out.zst")?, 3);
std::io::copy(&mut input, &mut enc)?;
enc.finish()?;
```

Dictionaries, zstd-format (`zstd --train`) or raw content. They can be
loaded into a compressor, or prepared once and shared (libzstd's
`ZSTD_CDict`):

```rust
let mut c = zstd_native::Compressor::with_dictionary(3, &dict_bytes)?;
let compressed = c.compress(&message)?;

let prepared = zstd_native::EncoderDictionary::new(&dict_bytes, 3)?;
let enc = zstd_native::Encoder::with_prepared_dictionary(Vec::new(), &prepared);

let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(&dict_bytes)?);
```

Multithreaded compression, with `std`. The output is the same as libzstd's
with `ZSTD_c_nbWorkers` set to the same number:

```rust
let mut c = zstd_native::Compressor::new(3);
c.set_workers(4);
let compressed = c.compress(&large_input)?;
```

Frame options follow libzstd's defaults, and can be changed on
`Compressor`, `StreamEncoder` and `Encoder`:
- the content checksum is off by default (`ZSTD_c_checksumFlag`);
- the content size and the dictionary ID are written (`ZSTD_c_contentSizeFlag`,
  `ZSTD_c_dictIDFlag`).

```rust
let mut c = zstd_native::Compressor::new(3);
c.set_checksum(true); // like `zstd` the CLI, which writes one
let compressed = c.compress(&data)?;
```

Without `std`, the push-style
[`StreamDecoder`](https://docs.rs/zstd-native/latest/zstd_native/struct.StreamDecoder.html)
and
[`StreamEncoder`](https://docs.rs/zstd-native/latest/zstd_native/struct.StreamEncoder.html)
take input and output in pieces of any size, like `ZSTD_decompressStream`
and `ZSTD_compressStream2`.

### Byte identity

For the same input and parameters, zstd-native writes the same bytes as
libzstd 1.5.7. That holds one-shot and streaming, for every write and
flush pattern, and covers:

- every level from the negative ones to 22 and every strategy, with
  libzstd's block splitters;
- dictionaries, attached, copied or reloaded as libzstd would;
- long distance matching, including where libzstd turns it on by itself
  (levels 16 and up, with large inputs);
- multithreaded compression, for the same worker count, job size and
  overlap.

It's tested on every dataset and size edge, and fuzzed with arbitrary
parameters (see [How it's tested](#how-its-tested)). Not ported yet:
prefix dictionaries (`ZSTD_CCtx_refPrefix`). Another libzstd release
can change libzstd's output, which is why the version names the release
it matches.

### Untrusted input

Corrupt input returns an `Error` and never panics. zstd-native is never
more lenient than libzstd: anything it accepts, libzstd 1.5.7 accepts
with the same output. Where libzstd's own decoders disagree, it follows
the stricter one. Streaming decompression refuses frames that need a
window over 2^27 bytes (128 MiB), libzstd's default; `set_window_log_max`
changes the limit.

## Supported

| Feature | Decode | Encode |
|---|---|---|
| Zstandard frames (RFC 8878): every block type, checksums, concatenated and skippable frames | yes | yes (byte-identical to libzstd) |
| Levels -131072 to 22, every strategy (`fast` to `btultra2`), block splitters | - | yes |
| Dictionaries: zstd format and raw content, prepared (`ZSTD_DDict` / `ZSTD_CDict`) | yes | yes |
| Long distance matching | yes | yes |
| Multithreaded compression (`ZSTD_c_nbWorkers`) | - | yes |
| Streaming: `std::io::Read` / `Write`, push API without `std` | yes | yes |
| `no_std` + `alloc` | yes | yes |
| Prefix dictionaries (`refPrefix`) | yes, as a raw-content `Dictionary` | not yet |
| Legacy formats (zstd 0.x) | no | no |

## How it's tested

The ports follow libzstd 1.5.7's sources, kept read-only in `third_party/zstd`.

| Layer | What it checks |
|---|---|
| **Encoder byte identity** | Compressed output is compared byte for byte with C libzstd 1.5.7 on every dataset and size edge. It covers every level and strategy, streaming with flushes and pledged sizes, reused contexts, dictionaries (loaded and prepared), long distance matching and multithreading. Every frame must decode with both libraries. |
| **Differential decoding** | Everything libzstd produces at levels -5 to 22, with long distance matching, dictionaries, checksums and every block-size edge, must decode to identical bytes. That holds through every API: one-shot, streaming with 1-byte buffers, and `io::Read`. |
| **Corruption** | Hundreds of thousands of mutated frames (bit flips, truncation, header damage), with debug assertions on. Each must return an error or the same output as libzstd, never panic, and never be accepted when libzstd rejects it. |
| **Fuzzing** | cargo-fuzz with libFuzzer, differential against libzstd. Four targets: arbitrary bytes into every decoder, streaming with arbitrary piece sizes and dictionaries, libzstd's output with arbitrary parameters, and arbitrary data and parameters through the encoder. They run on every push and PR and nightly. Before the release they ran for 3 hours on 56 cores, under AddressSanitizer and with the portable fallbacks. |
| **SIMD and assembly paths** | The whole suite also runs with `--features portable`, which turns off the BMI2 and assembly paths, so the safe fallbacks are tested on every platform. Hot-path `unsafe` has debug assertions, which the CI-profile tests and the fuzzers run with. |
| **Miri** | On x86-64 and big-endian s390x, over the portable code paths: a fast set on every push and PR, and every test weekly. |

**CI** (GitHub Actions, every PR):

- fmt, clippy with `-D warnings` (also for aarch64 and the fuzz crate),
  docs with `-D warnings`
- tests (run-time CPU selection and `portable`) on Linux x64/arm64,
  Windows x64 and macOS arm64, plus 32-bit Windows (i686)
- builds for `no_std` (thumbv7em), wasm32, i686 and aarch64
- MSRV (Rust 1.89)
- the four fuzz targets and Miri
- cargo-deny (RustSec advisories, licenses, sources)

The OpenSSF Scorecard watches the workflows themselves: actions are
pinned by SHA and tokens are read-only.

## Versioning

The crate follows SemVer for its own API. The build metadata names the
libzstd release whose output and behaviour it matches: `0.1.0+zstd.1.5.7`
is release 0.1.0, compatible with libzstd 1.5.7. Depend on it as
`zstd-native = "0.1"`; Cargo ignores the metadata.

Before 1.0, a minor release (0.2) may break code, and a patch release
(0.1.1) adds API or fixes bugs. Following a new libzstd release that
changes the compressed output is at least a minor release, with new
metadata (for example `0.2.0+zstd.1.5.8`), never a patch. The minimum
Rust version (1.89) is raised only in minor releases. Git tags are
`vX.Y.Z`.

## Development

```sh
git clone --recurse-submodules https://github.com/zcsizmadia/zstd-native
cargo test --workspace --profile ci                               # all tests
cargo test -p zstd-bench --profile ci --features zstd-native/portable # without the BMI2/asm paths
cargo bench -p zstd-bench --bench decode                          # vs libzstd, structured-zstd, ruzstd
cargo +nightly fuzz run decode                                    # in fuzz/
```

`third_party/zstd` is the libzstd 1.5.7 source, the reference for
porting. It is never compiled into this crate. C libzstd (the `zstd`
crate) is used only by the benchmark and fuzz crates. Design notes and
the roadmap are in [docs/PROPOSAL.md](docs/PROPOSAL.md).

## Sibling project

[xz-native](https://github.com/zcsizmadia/xz-native)
([crates.io](https://crates.io/crates/xz-native)) applies the same approach
to xz/liblzma 5.8.4:
- pure Rust;
- `.xz`, `.lzma` and `.lz` decoding, and `.xz` and `.lzma` encoding
  byte-identical to liblzma;
- the same strictness on bad input;
- at or above liblzma's speed.

The two crates share their design: run-time CPU dispatch with a `portable`
fallback, differential tests and fuzzing against the C library, and
versions that name the upstream release they match.

## License

[BSD-3-Clause](LICENSE), like zstd. zstd-native is a port of parts of it.
