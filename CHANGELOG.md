# Changelog

All notable changes to zstd-native. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/) for its own API, with the
libzstd release it matches as build metadata (`0.1.0+zstd.1.5.7`). Before
1.0, a minor version may change the API; see the README's Versioning
section.

## [Unreleased]

## [0.1.0+zstd.1.5.7]

The first release. It is compatible with libzstd 1.5.7: the same compressed
bytes for the same parameters, and the same or stricter acceptance of
compressed input.

### Added
- **Decompression**
  - one-shot (`decompress`, `decompress_to_slice`, `Decompressor`)
  - streaming (`Decoder` over `std::io::Read`, and the push-style
    `StreamDecoder` without `std`)
  - every frame and block type, checksums, concatenated and skippable
    frames
  - dictionaries (`Dictionary`)
  - a window limit for streaming
- **Compression**, byte-identical to libzstd 1.5.7
  - every level (negative to 22) and strategy, with libzstd's pre- and
    post-block splitters
  - one-shot (`compress`, `compress_to_slice`, `Compressor`)
  - streaming (`Encoder` over `std::io::Write`, and the push-style
    `StreamEncoder`, call for call like `ZSTD_compressStream2`, flushes
    included)
  - dictionaries, loaded or prepared (`EncoderDictionary`, libzstd's
    `ZSTD_CDict`), attached, copied or reloaded as libzstd decides
  - long distance matching
  - multithreaded compression (`set_workers`)
- **`no_std` + `alloc`**, with the `std` feature on by default
- **Speed:** at or above libzstd's speed on one core (see BENCHMARK.md)
  - x86-64 assembly for the Huffman decoding loops and for the level 1-4
    match finders
  - BMI2 code paths, selected at run time
  - the `portable` feature turns all of these off, to test the fallbacks
- **Fuzzing:** cargo-fuzz targets differential against libzstd (`fuzz/`)
- **CI:**
  - tests on Linux, Windows and macOS (x64, arm64, i686)
  - cross builds (`no_std`, wasm32)
  - MSRV 1.89
  - fuzzing, Miri, cargo-deny and OpenSSF Scorecard

[Unreleased]: https://github.com/zcsizmadia/zstd-native/compare/v0.1.0...HEAD
[0.1.0+zstd.1.5.7]: https://github.com/zcsizmadia/zstd-native/releases/tag/v0.1.0
