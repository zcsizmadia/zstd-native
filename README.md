# zstd-native

Pure Rust [Zstandard](https://facebook.github.io/zstd/) (RFC 8878), built
to match or beat C libzstd 1.5.7 in speed. No C code, `no_std` + `alloc`.

**Status: early development.** The decoder is complete for single-call use
(all frame and block types, dictionaries, checksums, concatenated and
skippable frames) and tested against libzstd; streaming and the encoder
are next (see [docs/PROPOSAL.md](docs/PROPOSAL.md)).

```rust
let data = zstd_native::decompress(&compressed)?;

// Reuse tables and buffers across calls, optionally with a dictionary:
let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(&dict_bytes)?);
let n = d.decompress_to_slice(&compressed, &mut buffer)?;
```

## Speed

Decompression on one core of an AMD EPYC 7543 (Zen 3), MiB/s of output,
every library reusing its context and decoding into a preallocated buffer.
libzstd is the `zstd` crate's build (libzstd 1.5.7, `-O3`, run-time BMI2,
the x86-64 asm Huffman decoder).

| Data | Level | zstd-native | libzstd | structured-zstd | ruzstd | zstd-native / libzstd |
|---|---:|---:|---:|---:|---:|---:|
| source | 1 | 1352 | 1414 | 1301 | 354 | 0.96x |
| source | 3 | 1248 | 1327 | 1225 | 388 | 0.94x |
| source | 9 | 1409 | 1503 | 1429 | 426 | 0.94x |
| source | 19 | 1385 | 1477 | 1385 | 467 | 0.94x |
| log | 1 | 1232 | 1313 | 1194 | 354 | 0.94x |
| log | 3 | 1242 | 1335 | 1202 | 398 | 0.93x |
| log | 9 | 1431 | 1538 | 1382 | 444 | 0.93x |
| log | 19 | 1575 | 1716 | 1524 | 477 | 0.92x |
| numeric | 1 | 768 | 802 | 689 | 214 | 0.96x |
| numeric | 3 | 676 | 710 | 626 | 185 | 0.95x |
| numeric | 9 | 756 | 788 | 686 | 205 | 0.96x |
| numeric | 19 | 780 | 809 | 706 | 254 | 0.96x |
| random | 1 | 16964 | 16929 | 16923 | 6404 | 1.00x |
| random | 3 | 10469 | 10460 | 10460 | 6329 | 1.00x |
| random | 9 | 10473 | 10458 | 10453 | 6322 | 1.00x |
| random | 19 | 10457 | 10457 | 10458 | 6443 | 1.00x |

Datasets: `source` = the zstd C sources, `log` = synthetic server logs, `numeric` = integer series, `random` = incompressible (raw blocks: memcpy speed). zstd-native is at 92-96% of libzstd on compressible data, ahead of structured-zstd on 10 of the 12 compressible cases (1.5% behind on source L9, tied on source L19), and about 3x ruzstd. Closing the gap to libzstd is the current work.

How: the hot loops (sequence decoding and execution, 4-stream Huffman
decoding with single- and double-symbol tables) are compiled generic and
with BMI2 and picked at run time; the sequence loop is organized so its
state stays in registers (one combined FSE table, thin pointers, cold
dictionary path). Details in [docs/PROPOSAL.md](docs/PROPOSAL.md).

## Correctness

- Differential tests: everything libzstd produces at levels -5 to 22, with
  long-distance matching, dictionaries, checksums, every block-size edge,
  decodes to identical bytes.
- Corrupt input returns an error, never panics; anything zstd-native
  accepts, libzstd accepts with the same output (tested on hundreds of
  thousands of mutated frames with debug assertions on).
- `unsafe` is confined to the hot paths, each with a SAFETY argument and
  debug assertions; the `portable` feature runs the generic code paths.

## License

BSD-3-Clause, like zstd (this is a port of parts of it).
