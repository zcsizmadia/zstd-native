# Changelog

All notable changes to zstd-native. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the crate
follows [Semantic Versioning](https://semver.org/) for its own API, with the
libzstd release it matches as build metadata (`0.1.0+zstd.1.5.7`). Before
1.0, a minor version may change the API; see the README's Versioning
section.

## [Unreleased]

Speed only: no API change, and the compressed output is still the same
bytes as libzstd 1.5.7. Numbers are for an AMD EPYC 7543 (Zen 3), one
core; see BENCHMARK.md.

### Changed
- **Short match offsets:** matches with an offset under 16 are decoded 16
  bytes per store, with a byte shuffle (SSSE3; NEON on AArch64), from
  meta/zstd#4718. Data with short repeating periods or runs of small
  integers decompresses 1.3x to 12x faster; Silesia is unchanged (#60,
  #70).
- **Levels 1-4 with dictionaries or frequent flushes:** faster `fast` and
  `dfast` loops for dictionaries (dictMatchState, extDict) and for
  streaming once the buffer wraps (extDict), with ports of meta/zstd#4764
  and #4866. 1-4 KiB dictionary frames are up to 14% faster, and flushes
  at levels 3-4 3-9% faster (#62, #71).
- **Small blocks:** less Huffman and FSE table work per block, and a
  context with an attached dictionary continues its indices instead of
  clearing its hash table, as libzstd does. 1-4 KiB frames, with or
  without a dictionary, and frequent flushes are 3-14% faster (#72, #74).
- **`dfast` extDict loop in assembly** (levels 3-4, streaming with flushes
  and copied dictionaries): 2-5% faster. **Optimal parser** (levels
  16-22): only the prices of changed codes are recomputed, 4-42% faster
  on 4-16 KiB frames; dickens at level 19 in 4 KiB frames is 19% faster
  (#75, #77).
- **Streaming decompression into the caller's buffer:** `StreamDecoder`
  decodes blocks straight into the output when they fit, and
  `Decoder::read_to_end` straight into the `Vec`. Whole-output streaming
  is 2-12% faster and `read_to_end` 7% to 3.5x faster (#12, #73).
- **Cold dictionaries:** the first block after a dictionary is installed
  uses libzstd's prefetching sequence loop. 4 KiB frames with a cold
  110 KiB dictionary decode 23-27% faster (#11, #76).
- **Dictionary decompression:** frames use a dictionary's tables in place
  instead of copying them (`Dictionary` clones now share them, like
  `ZSTD_DCtx_refDDict`), load a cold dictionary's tables up front, use
  its double-symbol Huffman table once warm, and copy matches in the
  dictionary in 16-byte chunks; a dictionary installed again stays warm.
  4 KiB frames with a 110 KiB dictionary take 28-31% less time warm (on
  par with libzstd to 7% less) and 28-34% less right after switching
  dictionaries (17-21% less than libzstd); Silesia in 4 KiB dictionary
  frames decodes 29-51% faster (#80).
- **Dictionary parsing and fresh decompressors:** a `Dictionary` keeps
  only the tables frames use (18 KB instead of 34.5 KB, built in place),
  FSE tables with low-probability symbols are built without libzstd's
  slow spread, a decompressor's literal buffer comes with it, and a
  dictionary's first frame doesn't build its double-symbol table for
  small literal sections. With a 110 KiB dictionary, parsing is 3-8%
  faster, and 4 KiB frames through a fresh decompressor take 14-15% less
  time (8-29% less than libzstd); parsing a dictionary per frame is on par
  with libzstd. Silesia decodes about 1% faster.

## [0.1.1+zstd.1.5.7]

Speed only: no API change, and the compressed output is the same bytes as
0.1.0 (and libzstd 1.5.7). Numbers are for an AMD EPYC 7543 (Zen 3), one
core, Silesia; see BENCHMARK.md.

### Changed
- **Levels 5-12:** 3.7-5.6% faster compression (Silesia geomean), from a
  port of meta/zstd#4841 to the row match finder: the head slot is masked
  out once, the first 16 bytes of a candidate are compared without
  branches, and the BMI2 search is inlined into the parser (#61).
- **Steadier encoder speed:** speed no longer depends on where the code
  lands in memory or on the size of the environment (#34, #66).
  - The row finder's update loop reads the next row head before the
    current stores, which also makes repetitive inputs faster at levels
    5-12. For example, nci at level 5 went from 182-221 to 232-239 MiB/s
    across binary copies.
  - The level 1-2 asm loop is aligned to 64 bytes.
  - The optimal parser's hottest loop is pinned to one alignment.
- **FSE tables** are built in one fused pass (port of meta/zstd#4836),
  for both decoding and encoding. Small frames (1-64 KB) decompress
  1.3-3.8% faster (#59).
- **Huffman asm decoding loops** use `tzcnt` instead of `bsf`: 0.7-1.8%
  faster where those loops run (#57).

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
  - frame options: content checksum, content size and dictionary ID
    (`set_checksum`, `set_content_size_flag`, `set_dict_id_flag`)
- **`no_std` + `alloc`**, with the `std` feature on by default
- **Speed:** at or above libzstd's speed on one core (see BENCHMARK.md)
  - x86-64 assembly for the Huffman decoding loops and for the level 1-4
    match finders
  - BMI2 code paths, selected at run time
  - on Linux x86-64, match finder tables of 8 MiB and more on
    transparent huge pages (`madvise`)
  - multithreaded compression at or above libzstd's speed with 1-16
    workers
  - the `portable` feature turns all of these off, to test the fallbacks
- **Fuzzing:** cargo-fuzz targets differential against libzstd (`fuzz/`)
- **CI:**
  - tests on Linux, Windows and macOS (x64, arm64, i686)
  - cross builds (`no_std`, wasm32)
  - MSRV 1.89
  - fuzzing, Miri, cargo-deny and OpenSSF Scorecard

[Unreleased]: https://github.com/zcsizmadia/zstd-native/compare/v0.1.1...HEAD
[0.1.1+zstd.1.5.7]: https://github.com/zcsizmadia/zstd-native/releases/tag/v0.1.1
[0.1.0+zstd.1.5.7]: https://github.com/zcsizmadia/zstd-native/releases/tag/v0.1.0
