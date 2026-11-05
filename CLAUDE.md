# zstd-native

Pure Rust Zstandard (zstd) library, compatible with libzstd 1.5.7. Sibling
of [xz-native](https://github.com/zcsizmadia/xz-native). Design, decisions
and roadmap: [docs/PROPOSAL.md](docs/PROPOSAL.md).

## Priorities

1. Performance: on par with or faster than C libzstd 1.5.7 (and so every
   Rust zstd crate). Measure before and after every hot-path change, on
   the EPYC benchmark host (`bd-au1-jz001g3v`, see docs/PROPOSAL.md);
   the Windows laptop's C build has no asm Huffman decoder and no
   dynamic BMI2, so local numbers flatter zstd-native.
2. Pure Rust library: no C in `crates/zstd-native`. C libzstd (the `zstd`
   crate) is allowed only in `crates/zstd-bench` and `fuzz/`.
3. libzstd 1.5.7 behavior: port from `third_party/zstd` (read-only
   reference). Anything zstd-native accepts, libzstd must accept with the
   same output (zstd-native may be stricter on corrupt input).
4. Tests for every change: differential against libzstd, corruption tests.

## Commands

- `cargo test --workspace --profile ci` (all tests, debug assertions on)
- `cargo test -p zstd-bench --profile ci --features zstd-native/portable`
  (generic code paths)
- `ZSTD_NATIVE_CORRUPTION_ITERS=100000 cargo test -p zstd-bench --profile ci --test corruption`
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all`
- `cargo bench -p zstd-bench --bench decode` (`ZSTD_BENCH_LEVELS`,
  `ZSTD_BENCH_ONLY`, `ZSTD_BENCH_CORPUS`, see the bench's header)
- `cargo run --profile bench -p zstd-bench --example profile_decode -- source 3 native 5`
  for `perf record` / quick A/B against C (`c` instead of `native`)

On this Windows machine, cargo is in `~/.cargo/bin` (not on PATH in Bash).

## Rules

- `unsafe` is denied by default and allowed only in hot paths, each block
  with a SAFETY comment and debug assertions: `bits.rs` (raw-pointer bit
  reader, `FastBits` of the sequence loop), `huf.rs` fast loops,
  `huf_asm.rs` (asm! port of libzstd's Huffman loops), `block.rs`
  sequence execution and copies, the encoder's match finders and
  `enc/block_asm.rs` (asm! block loops of `fast` and `dfast`), the
  encoder's bit writers (`enc/bitwriter.rs`, `enc/huf_enc.rs`: 8-byte
  flushes) and its run-time dispatch to `#[target_feature]` loops
  (`enc/seq_store.rs`, the BMI2 lazy parsers and searches in
  `enc/lazy.rs`), the `madvise` syscall that puts large match finder
  tables on huge pages (`enc/table.rs`), the output writes in
  `decoder.rs`, and the `align_hot_loop!` macro in `lib.rs` (an `asm!`
  alignment directive before the hot loops). New `unsafe` needs a
  benchmark showing the gain.
- CPU-specific code goes behind run-time detection (`cpu.rs`), compiled
  with `#[target_feature]` (BMI2 today), with the generic version selected
  by the `portable` feature for testing.
- Assembly (`asm!`, like xz-native's `lzma/x86_asm.rs`) is allowed when
  it measures faster, with the Rust loop kept as the fallback. Measure on
  both Intel and AMD: results differ (see `cpu.rs`, `huf_asm.rs`).
- Versioning: SemVer with the matched zstd version as build metadata
  (`0.1.0+zstd.1.5.7`).

## Git and GitHub

- Repository: https://github.com/zcsizmadia/zstd-native. Commits must use
  `zcsizmadia@gmail.com` (set in the repo config; the global config holds
  the work address).
- Work through issues and PRs once the initial import is in.
