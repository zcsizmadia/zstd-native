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
  src/enc/           the encoder (enc/mod.rs lists its modules;
                      enc/cdict.rs: compression dictionaries)
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
  keeping its state in registers. Then to ~99-100%: bounds checks as plain
  address comparisons (in `u64`, so they can't wrap on 32-bit) instead of
  `saturating_sub` (+3%), and one read for the three FSE state updates
  (up to 26 bits, split with shifts and masks) instead of three dependent
  reads (+1-3%). Tried and rejected: decoding batches of sequences before
  executing them (-15%), prefetching the match source just before the
  copy (-3%), reading all extra bits at once when they total under 31
  bits (+1-3% on logs and numeric data, -3% on text: the extra branch
  mispredicts), reading offset and match length bits together (-9%: the
  loop's register allocation is fragile; measure every change).
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
- **Small inputs** (per-frame setup; `examples/small_frames.rs`,
  `examples/huf_read.rs`): Huffman tree reads were 1.7x libzstd's cost.
  Fixed by building the X1 table from SSE2 per-weight symbol masks (counts
  by popcount, order by set bits) instead of a histogram and a counting
  sort, whose read-modify-write chains on equal weights serialized
  through store-to-load forwarding (portable fallback: 4 interleaved
  quarters); an inlined slow-path reload for short streams (weights,
  stream tails); FSE table builds without bounds checks and with LZCNT;
  and a per-thread cached decoder for the one-call functions (a fresh
  decoder zeroes ~40 KB of tables). 1.25-1.5x libzstd at 100 bytes,
  1.0-1.1x at 1-16 KB.
- **Huffman asm** (`huf_asm.rs`): libzstd's `huf_decompress_amd64.S` X1
  and X2 loops ported to `asm!` (same registers and instructions), run in
  bounded chunks computed by the same Rust code as the Rust loops. Which
  is faster depends on the microarchitecture: the asm is 12% faster on
  Intel Alder Lake and 6% slower on AMD Zen 3 (its `movb %ah` stores), so
  it runs on non-AMD CPUs with BMI2 and the Rust loops run on AMD. On
  literal-heavy data (`skewed`) zstd-native is 1.04x libzstd on Zen 3.

## Roadmap

1. Decoder speed to >= 1.0x libzstd everywhere (Huffman is there; the
   sequence loop is within 1-2%): a long-offset decoder with
   prefetching (libzstd's `ZSTD_decompressSequencesLong`) for large
   windows; literals placed in the output buffer like libzstd's split
   literal buffer; more measurement on Silesia (`ZSTD_BENCH_CORPUS`).
2. ~~Streaming decoder~~ (done: `stream.rs`, `io.rs`; window buffer of
   window + 2 blocks + 64 with the old segment as history after a wrap;
   0.98-1.21x libzstd's streaming decoder). Next: write into the
   caller's buffer directly when it can hold the frame.
3. Encoder, byte-identical with libzstd per level (the oracle for tests;
   it makes speed the only variable). Done: fast (levels 1-2 and
   negative; `enc/`, issue #6) and dfast (levels 3-4, #7), 0.90-0.96x
   libzstd's speed; the lazy family (greedy, lazy, lazy2 with the hash
   chain and row-based match finders, btlazy2: levels 4/5 to 15, #8), on
   par with libzstd in first measurements; the optimal parser (btopt,
   btultra, btultra2: levels 16-22, down to 11 for small inputs, with
   btultra2's two-pass first block, `enc/opt.rs`) and the post-block
   splitter (`enc/split.rs`, #9), 0.9-1.05x libzstd's speed in first
   measurements. Streaming compression (#10, `enc/stream.rs`,
   `io.rs`): a port of `ZSTD_compressStream2` in buffered mode (input
   ring buffer of window + block, libzstd's end-of-input shortcut, flush
   and end), with the extDict variants of every match finder for the
   wrapped ring; byte-identical with libzstd for the same calls.
   Dictionaries (#22, `enc/cdict.rs`): zstd-format and raw content
   (`ZSTD_compress_insertDictionary`, `ZSTD_loadCEntropy`,
   `ZSTD_loadDictionaryContent` with its size limits), CDicts with tagged
   `fast`/`dfast` tables (`EncoderDictionary`), and how a context uses
   one (`ZSTD_compressBegin_internal`): attached for small sources (the
   `_dictMatchState` variants of every match finder search its tables),
   copied above the per-strategy cutoffs (the dictionary is then the
   extDict, kept in its own buffer), or reloaded with the frame's
   parameters for large sources; the dictionary's entropy tables seed the
   first block and the optimal parser's statistics, its ID goes in the
   frame header. Byte-identical with libzstd for loaded
   (`ZSTD_CCtx_loadDictionary`) and referenced (`ZSTD_CCtx_refCDict`)
   dictionaries, one-shot, reused and streaming; on par with libzstd's
   speed in first measurements. Not ported: dedicated dictionary search
   (off by default), prefix dictionaries.
   Long distance matching (#24, `enc/ldm.rs`): a port of `zstd_ldm.c`
   (gear rolling hash, XXH64-bucketed table, `ZSTD_ldm_generateSequences`
   with its window, `ZSTD_ldm_blockCompress`) and of the optimal parser's
   LDM candidates (`ZSTD_optLdm_*`), as libzstd enables it by itself
   (`btopt` and above with `windowLog >= 27`: level 22 above 64 MiB or
   for unknown sizes), with raw-content dictionaries loaded into its
   table; byte-identical with libzstd, also forced on at every level
   (a hidden test hook). Not ported: the public LDM parameters
   (`ZSTD_c_enableLongDistanceMatching`, `ZSTD_c_ldm*`).
   Next: speed (#17), multithreading.
4. Multithreaded compression.
5. Fuzzing (cargo-fuzz, differential vs libzstd), Miri, CI matrix
   (x86-64/AArch64, Linux/Windows/macOS, 32-bit), AArch64 tuning.

## Benchmarks

Authoritative numbers come from the EPYC host (AMD EPYC 7543, Zen 3,
AVX2/BMI2), one pinned core (`taskset -c 8`), `cargo bench -p zstd-bench
--bench decode`. Datasets: the zstd C sources (text), synthetic logs,
numeric binary, random; add Silesia with `ZSTD_BENCH_CORPUS`.
