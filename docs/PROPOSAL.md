# zstd-native: design and roadmap

Sibling project of [xz-native](https://github.com/zcsizmadia/xz-native).

## Goals

In priority order:

1. **Performance.** On par with or faster than C libzstd 1.5.7 (built as
   distributions build it: `-O3`, run-time BMI2 dispatch, the x86-64 asm
   Huffman decoder), and so faster than every Rust zstd crate. Measured
   continuously against libzstd, ruzstd and structured-zstd.
2. **Pure Rust.** No C in the library, `no_std` + `alloc`.
3. **Compatibility with libzstd 1.5.7.** Everything libzstd produces
   decodes to the same bytes; corrupt input is rejected (never a panic).
   zstd-native may be stricter than libzstd, never more lenient.
4. **Full testing.** Differential tests against libzstd, corruption tests
   with debug assertions, fuzzing, a cross-platform CI matrix.

`third_party/zstd` (git submodule, v1.5.7) is the reference for the
algorithms and is never compiled into the library.

## Existing crates

| Crate | Type | Decode speed vs libzstd (EPYC 7543, first measurements) |
|---|---|---|
| `zstd` | Bindings to C libzstd | the reference |
| `structured-zstd` | Pure Rust | ~0.9x |
| `ruzstd` | Pure Rust | ~0.3x |

## Layout

```
crates/zstd-native/   the library (no_std + alloc; `std` for CPU detection)
  src/bits.rs         backward bit reader (raw pointers, libzstd's BIT_DStream)
  src/fse.rs          FSE: normalized counts, Huffman-weight decoding,
                      sequence tables (one combined LL/OF/ML table)
  src/huf.rs          Huffman: X1 and X2 tables, 4-stream fast loops
  src/block.rs        literals, sequences header, fused decode+execute loop
  src/frame.rs        frame headers
  src/dict.rs         zstd-format and raw content dictionaries
  src/decoder.rs      Decompressor, one-shot API (Vec / slice)
  src/xxhash.rs       XXH64 content checksum
  src/cpu.rs          run-time CPU feature detection
crates/zstd-bench/    criterion benchmarks, differential and corruption
                      tests (C libzstd via the `zstd` crate), profiling driver
third_party/zstd/     zstd 1.5.7 source (reference only)
```

## Implementation notes (decoder)

- **Output** goes through raw pointers so one code path serves a `Vec`'s
  spare capacity, a caller's slice and (later) the streaming window. The
  fast path copies in 16-byte chunks that overrun by up to 32 bytes, so it
  runs only while the output has that room; the last sequences of a
  buffer take an exact path, as in libzstd. `decompress()` reserves
  content size + 32 so every sequence takes the fast path.
- **Literals** decode into a separate buffer with 32 bytes of slack; raw
  literals with 32 bytes of slack after them in the input are used in place.
- **Sequence loop**: one combined FSE table (LL at 0, OF at 512, ML at 768,
  region offsets folded into `next_state`), so the loop needs one table
  pointer; repeat offsets as scalars; dictionary matches in a cold
  function. These cut the loop from 87% to ~95% of libzstd's speed by
  keeping its state in registers. Tried and rejected: decoding batches of
  sequences before executing them (-15%), prefetching the match source
  just before the copy (-3%).
- **Huffman**: X1 (one symbol per lookup) and X2 (up to two), chosen per
  block with libzstd's `HUF_selectDecoder` timing table. The X2 table is
  derived from the X1 table (cell `i` = X1 at `i`, plus X1 at
  `(i << n1) & 2047` when that code fits), built from one template per code
  length. Fast loops decode 5 lookups per stream per iteration with no
  bounds checks (iteration count computed up front); stream tails always
  use X1, and every stream must be consumed exactly, so X1 and X2 accept
  exactly the same inputs. (libzstd's fast path skips that end check.)
- **CPU features**: hot loops are compiled generic and with BMI2
  (`#[target_feature]`), picked at run time; `portable` forces generic.

## Roadmap

1. Decoder speed to >= 1.0x libzstd everywhere: port
   `huf_decompress_amd64.S` to `asm!`; a long-offset decoder with
   prefetching (libzstd's `ZSTD_decompressSequencesLong`) for large
   windows; literals placed in the output buffer like libzstd's split
   literal buffer; more measurement on Silesia (`ZSTD_BENCH_CORPUS`).
2. Streaming decoder: push API (`no_std`) and `std::io::Read`/`BufRead`
   adapters, window buffer with the dictionary-as-history mechanism the
   block decoder already supports; memory/window limits.
3. Encoder: levels 1-22 (fast, dfast, greedy, lazy, lazy2, btlazy2, btopt,
   btultra, btultra2), aiming at byte-identical output with libzstd per
   level (the oracle for tests, and it makes speed the only variable).
4. Multithreaded compression, dictionaries for the encoder, long distance
   matching.
5. Fuzzing (cargo-fuzz, differential vs libzstd), Miri, CI matrix
   (x86-64/AArch64, Linux/Windows/macOS, 32-bit), AArch64 tuning.

## Benchmarks

Authoritative numbers come from the EPYC host (AMD EPYC 7543, Zen 3,
AVX2/BMI2), one pinned core (`taskset -c 8`), `cargo bench -p zstd-bench
--bench decode`. Datasets: the zstd C sources (text), synthetic logs,
numeric binary, random; add Silesia with `ZSTD_BENCH_CORPUS`.
