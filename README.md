# zstd-native

Pure Rust [Zstandard](https://facebook.github.io/zstd/) (RFC 8878), built
to match or beat C libzstd 1.5.7 in speed. No C code, `no_std` + `alloc`.

**Status: early development.** The decoder is complete: one-shot and
streaming (`std::io::Read` and a `no_std` push API), all frame and block
types, dictionaries, checksums, concatenated and skippable frames, tested
against libzstd.

The encoder is in progress: its output is byte-identical to libzstd's for
the same input and level. Levels 1-3 (3 is the default),
level 4 (except for inputs up to 16 KB or of 128-256 KB) and the
negative levels (the `fast` and `dfast` strategies) work today, at about
0.9-0.96x libzstd's speed; the other strategies follow (issues #8-#10).

```rust
let compressed = zstd_native::compress(&data, 1)?; // same bytes as libzstd
let data = zstd_native::decompress(&compressed)?;

// Reuse tables and buffers across calls, optionally with a dictionary:
let mut d = zstd_native::Decompressor::with_dictionary(zstd_native::Dictionary::new(&dict_bytes)?);
let n = d.decompress_to_slice(&compressed, &mut buffer)?;

// Streaming from any reader:
let mut out = Vec::new();
std::io::copy(&mut zstd_native::Decoder::new(file), &mut out)?;
```

## Speed

Decompression on one core of an AMD EPYC 7543 (Zen 3), MiB/s of output,
every library reusing its context and decoding into a preallocated buffer.
libzstd is the `zstd` crate's build (libzstd 1.5.7, `-O3`, run-time BMI2,
the x86-64 asm Huffman decoder).

| Data | Level | zstd-native | libzstd | structured-zstd | ruzstd | zstd-native / libzstd |
|---|---:|---:|---:|---:|---:|---:|
| source | 1 | 1367 | 1416 | 1303 | 354 | 0.96x |
| source | 3 | 1262 | 1327 | 1225 | 388 | 0.95x |
| source | 9 | 1425 | 1509 | 1427 | 426 | 0.94x |
| source | 19 | 1398 | 1479 | 1381 | 468 | 0.95x |
| log | 1 | 1249 | 1309 | 1196 | 354 | 0.95x |
| log | 3 | 1243 | 1337 | 1203 | 398 | 0.93x |
| log | 9 | 1432 | 1548 | 1386 | 446 | 0.93x |
| log | 19 | 1572 | 1713 | 1528 | 479 | 0.92x |
| numeric | 1 | 784 | 803 | 695 | 214 | 0.98x |
| numeric | 3 | 689 | 713 | 629 | 185 | 0.97x |
| numeric | 9 | 772 | 791 | 687 | 205 | 0.98x |
| numeric | 19 | 800 | 811 | 706 | 248 | 0.99x |
| skewed | 1 | 1462 | 1405 | 1108 | 217 | 1.04x |
| skewed | 3 | 1462 | 1405 | 1108 | 215 | 1.04x |
| skewed | 9 | 1460 | 1389 | 1104 | 216 | 1.05x |
| skewed | 19 | 1429 | 1363 | 1077 | 220 | 1.05x |
| random | 1 | 16930 | 16884 | 16906 | 6368 | 1.00x |
| random | 3 | 10444 | 10403 | 10433 | 6374 | 1.00x |
| random | 9 | 10442 | 10422 | 10434 | 6298 | 1.00x |
| random | 19 | 10442 | 10418 | 10432 | 6217 | 1.00x |

Datasets: `source` = the zstd C sources, `log` = synthetic server logs, `numeric` = integer series, `skewed` = skewed random bytes (nearly all Huffman-coded literals), `random` = incompressible (raw blocks: memcpy speed).

zstd-native is 4-5% faster than libzstd on literal-heavy data (`skewed`), 97-99% on `numeric` and 92-96% on `source` and `log`, where the sequence loop is the remaining gap. It is ahead of or level with structured-zstd everywhere (within noise on `source` L9) and 3-7x faster than ruzstd.

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
