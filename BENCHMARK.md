# Benchmarks

zstd-native 0.1.2 (main `61e24a1`; the
dictionary compression table at main `6f19057`) against C libzstd 1.5.7 and
[structured-zstd](https://crates.io/crates/structured-zstd) 0.2.6, the
fastest other pure-Rust zstd crate. [ruzstd](https://crates.io/crates/ruzstd)
decompresses 3-7x slower than zstd-native, so it isn't in the tables.
Other levels and multithreading, measured against C only, are in
[More levels and multithreading](#more-levels-and-multithreading); small
frames, flushes, dictionaries and streaming decompression are in
[Small frames, flushes, dictionaries, streaming](#small-frames-flushes-dictionaries-streaming).

## Setup

- **Machine:** AMD EPYC 7543 (Zen 3), Ubuntu, one core per run (`taskset`).
- **libzstd:** the [`zstd`](https://crates.io/crates/zstd) crate's build
  (libzstd 1.5.7, `-O3`, run-time BMI2 dispatch, the x86-64 asm Huffman
  decoder): what Rust users of C zstd get.
- **Data:** the [Silesia corpus](https://sun.aei.polsl.pl/~sdeor/index.php?page=silesia),
  each file compressed or decompressed whole, in memory.
- **Contexts:** every library reuses its context. zstd-native and libzstd
  write into a preallocated buffer. structured-zstd's
  `compress_slice_to_vec` returns a new `Vec`, which costs it 1-2% at most.
- **Decompression input:** frames that libzstd compressed at the given
  level, so all three libraries decode the same frames.
- **Runs:** each file, level and library ran at least 1.5 s (8 s at
  level 19, 1 s for decompression) from each of three copies of the
  binary (made with `cp`, at paths of different lengths), in two rounds:
  six runs per cell. Each table (one level, one direction) ran on one
  core, its runs in a new random order each round. Each figure is the
  best of the six runs. The median of the six gives the same geomeans
  within 0.01x (level 9: 1.21x against libzstd), and no cell changes side
  of 1.00x. Cells whose copies differed by more than 5%, or whose best
  and median fell on different sides of 1.00x, got 2 more rounds.
- **One table per socket:** two tables ran at a time, with nothing else
  running: the compression tables and the flushes on CPU 8 (socket 0),
  the decompression tables and the other small-frame tables on CPU 40
  (socket 1), each with its memory on its own node (`numactl --membind`).
  Before the run, sao, osdb, nci and samba at levels 2 and 3 (both
  libraries, 3 copies, 3 rounds) ran alone on CPU 8, alone on CPU 40, on
  both at once, and alone on CPU 8 again: the cells run in parallel were
  within 0.9% of the same cells run alone (geomean 0.997-1.001), the same
  spread as the repeat alone (0.989-1.005).
- **Against the previous run:** a build of `96cdb56`, the main of the
  previous tables, ran interleaved in the same rounds at levels 1-4, in
  decompression, and with flushes at levels 1 and 3. The dictionary
  compression table, re-measured at `6f19057`, had a `61e24a1` build
  interleaved instead.
- **Why three copies:** speed can move several percent with where the
  binary's data and stack land in memory (see
  [#34](https://github.com/zcsizmadia/zstd-native/issues/34)). libzstd
  is bimodal at level 2 (and on some files at levels 5-12): often one
  run in three or four is 12-21% slower, with the same instruction count
  and more cycles (`perf stat`, nci at level 2: 7.86 against 6.35-6.46 billion
  cycles, 14.0 billion instructions each). The best of six runs takes
  that noise out of the comparison.
- **Reproduce:**
  `cargo run --release -p zstd-bench --example profile_encode -- <file> <level> <native|c|structured> <seconds>`
  (and `profile_decode`), with `ZSTD_BENCH_CORPUS=<silesia dir>`.

Speeds are MiB/s of uncompressed data. Above 1.00x, zstd-native is faster.
**zstd-native's output is byte-identical to libzstd's**, so their ratios
are the same; the compressed sizes were checked equal in every
compression cell of this file. structured-zstd's output differs from
libzstd's (at level 19 on every file checked), and its ratios differ by up
to 2.6% (x-ray at level 3), in either direction.

## Summary (geometric mean over the 12 files)

| | vs libzstd 1.5.7 | vs structured-zstd 0.2.6 |
|---|---:|---:|
| Compression, level 1 | 1.07x (1.03-1.20) | 1.11x (1.04-1.25) |
| Compression, level 3 | 1.07x (1.04-1.11) | 1.37x (1.24-1.53) |
| Compression, level 9 | 1.19x (1.15-1.28) | 1.37x (1.27-1.42) |
| Compression, level 19 | 1.41x (1.24-1.70) | 1.18x (1.12-1.31) |
| Decompression, level 1 frames | 1.08x (1.03-1.16) | 1.21x (1.03-1.46) |
| Decompression, level 3 frames | 1.11x (1.06-1.19) | 1.26x (1.16-1.42) |
| Decompression, level 9 frames | 1.12x (1.07-1.18) | 1.28x (1.11-1.43) |
| Decompression, level 19 frames | 1.12x (1.06-1.18) | 1.30x (1.13-1.44) |

The ranges in brackets are the slowest and fastest file. zstd-native is
faster than libzstd and structured-zstd on every file, level and
direction.

The previous table ([#86](https://github.com/zcsizmadia/zstd-native/pull/86),
main `96cdb56`) is unchanged within 0.01x, except for 0.01x up in
decompression. The `96cdb56` build, run interleaved with this one,
compresses at the same speed at levels 1-4 (geomean 0.998-1.001x by the
median, every file within 1.3%) and decodes 0.5-0.8% slower (levels
1-19, geomean by the median; the FSE table build of
[#87](https://github.com/zcsizmadia/zstd-native/pull/87)).

Level 19 compression speeds have one decimal (2.3-6.3 MiB/s), so
single-file ratios there are ±2-4%.

## More levels and multithreading

Measured against C libzstd only, at main `61e24a1`.

### Levels 1-4

One level per core, against the `zstd` crate: best of the runs, and in
brackets the median (2 rounds, 4 for cells whose copies differed by over
5%):

| File | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| dickens | 1.04x (1.04x) | 1.04x (1.04x) | 1.04x (1.05x) | 1.04x (1.05x) |
| mozilla | 1.06x (1.06x) | 1.07x (1.08x) | 1.07x (1.09x) | 1.07x (1.08x) |
| mr | 1.07x (1.07x) | 1.08x (1.08x) | 1.08x (1.08x) | 1.06x (1.07x) |
| nci | 1.10x (1.10x) | 1.12x (1.12x) | 1.07x (1.07x) | 1.06x (1.06x) |
| ooffice | 1.05x (1.05x) | 1.08x (1.07x) | 1.09x (1.10x) | 1.08x (1.09x) |
| osdb | 1.04x (1.04x) | 1.08x (1.07x) | 1.07x (1.07x) | 1.07x (1.07x) |
| reymont | 1.03x (1.04x) | 1.05x (1.05x) | 1.05x (1.05x) | 1.05x (1.05x) |
| samba | 1.04x (1.04x) | 1.08x (1.07x) | 1.06x (1.06x) | 1.05x (1.05x) |
| sao | 1.05x (1.05x) | 1.07x (1.07x) | 1.10x (1.09x) | 1.09x (1.09x) |
| webster | 1.04x (1.03x) | 1.04x (1.05x) | 1.05x (1.05x) | 1.04x (1.04x) |
| x-ray | 1.20x (1.20x) | 1.14x (1.13x) | 1.11x (1.12x) | 1.09x (1.10x) |
| xml | 1.07x (1.07x) | 1.10x (1.11x) | 1.06x (1.06x) | 1.06x (1.06x) |
| **geometric mean** | **1.07x (1.07x)** | **1.08x (1.08x)** | **1.07x (1.07x)** | **1.06x (1.07x)** |

No file is below 1.00x; the slowest is reymont at L1 (1.03x).

### Levels 5-15

One level per core, all 12 files, median of the runs (3 copies of each
binary, 2 rounds, 4 for cells whose copies differed by over 5%), geomean,
zstd-native's speed divided by each implementation's:

| | `zstd` crate | libzstd, gcc -O3 | CLI clang | CLI gcc `-march=native` |
|---|---:|---:|---:|---:|
| L5 | 1.15x | 1.18x | 1.14x | 1.10x |
| L7 | 1.15x | 1.15x | 1.16x | 1.07x |
| L9 | 1.21x | 1.19x | 1.24x | 1.13x |
| L11 | 1.17x | 1.18x | 1.26x | 1.17x |
| L12 | 1.17x | 1.16x | 1.21x | 1.14x |
| L15 | 1.16x | 1.16x | 1.15x | 1.13x |

No single file is below 1.03x against any build (the slowest: mr at L7
and dickens at L5 against `-march=native`, 1.04x). The row finder port
([#68](https://github.com/zcsizmadia/zstd-native/pull/68)) and the steadier
code placement ([#66](https://github.com/zcsizmadia/zstd-native/pull/66))
closed the gaps of [#53](https://github.com/zcsizmadia/zstd-native/pull/53)
(nci at L5 and L7 against `-march=native`, xml at L12).

libzstd is bimodal on some files at levels 5-9, as at level 2: nci at L5
ran at 181-228 MiB/s from run to run with the `zstd` crate's build, and
the gcc -O3 and clang builds vary up to 27% on nci and xml at L5-L9. Which
mode a round lands in moves their medians: against #86, the `zstd`
crate's L5 geomean is 6% faster and the gcc -O3 build's 3% slower, with
zstd-native's within 0.2%. By the best of the runs, zstd-native is 1.13x
faster than the `zstd` crate at L5 and L7 and 1.16-1.19x at L9-L15.

The C builds, from upstream's 1.5.7 sources with gcc 11.4 and clang 14:
- **libzstd, gcc -O3:** `make libzstd.a` in `lib/`, linked into the same
  bench binary through zstd-sys's pkg-config path
  (`ZSTD_SYS_USE_PKG_CONFIG`), so it runs the same loop as the `zstd`
  crate. In #53 this build was ~4% faster than the `zstd` crate's; now
  the two are within 2%, either way (by the best of the runs: gcc -O3
  faster at L9, the `zstd` crate at the other levels). #53 didn't record its
  build steps, so this may not be the same build.
- **CLI:** `make zstd` in `programs/` with `CC=clang`, or with
  `MOREFLAGS=-march=native` for gcc, run as `zstd -q -b<level>
  --single-thread -i2 <file>`. It reports its fastest round, which
  slightly favours C. Its compressed sizes equal zstd-native's too.

C gains as much from huge pages as zstd-native does, but only when the
user opts in (`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`).

### Multithreaded compression

Silesia concatenated (211 MB), CPUs and memory of one socket
(`numactl --cpunodebind=0 --membind=0`), nothing else running, 3 copies,
2 rounds (6 for L1 and for L3 with 8-16 workers), at least 3 s a run (8 s
at L19), MiB/s, best of the runs. Each cell is zstd-native / libzstd
(ratio; by the median of the runs where it differs by more than 0.02x):

| | One-shot | Streaming (128 KB writes) |
|---|---|---|
| L1, 4 workers | 1816 / 1624 (1.12x) | 1618 / 1463 (1.11x) |
| L1, 8 workers | 3513 / 3003 (1.17x) | 2372 / 1974 (1.20x; median 1.08x) |
| L1, 16 workers | 6560 / 5273 (1.24x) | 2340 / 1926 (1.22x) |
| L3, 4 workers | 901 / 786 (1.15x) | 776 / 671 (1.16x; median 1.13x) |
| L3, 8 workers | 1603 / 1333 (1.20x) | 1148 / 1113 (1.03x) |
| L3, 16 workers | 2296 / 1794 (1.28x) | 1480 / 1051 (1.41x; median 1.11x) |
| L9, 4 workers | 268 / 228 (1.17x; median 1.20x) | 241 / 209 (1.15x) |
| L9, 8 workers | 403 / 320 (1.26x) | 337 / 292 (1.15x) |
| L9, 16 workers | 659 / 553 (1.19x) | 464 / 413 (1.12x) |
| L19, 8 workers | 18.6 / 13.9 (1.33x) | 18.1 / 13.9 (1.30x) |
| L19, 16 workers | 18.3 / 14.0 (1.31x) | 18.4 / 13.8 (1.34x) |

zstd-native's streaming at L1 and L3 with 8-16 workers varies by 7-34%
from run to run (L3, 16 workers: 1107-1480 MiB/s, median 1134), libzstd's
by 2-6%; the extra rounds didn't change that, so those cells give both
figures. The lowest, L3 with 8 workers (1.02x by the median), was 1.04x
by the median in #86. The compressed sizes were equal in every cell.

No cell is below 1.00x. The fixes behind these are in
[#52](https://github.com/zcsizmadia/zstd-native/pull/52), with the full
1-16 worker matrix:
- **Copy aliasing:** the calling thread's input copy no longer hits
  glibc's 4K-aliasing `memcpy` path.
- **Job table:** with 8+ workers, the job table is twice libzstd's.

## Small frames, flushes, dictionaries, streaming

Against C libzstd (the `zstd` crate) at main `61e24a1` (dictionary
compression at `6f19057`), with structured-zstd where it has the same
API. Median of the runs: 3 copies of the binary, 2 rounds (2 more for
cells whose copies differed by over 5%, or whose best and median fell on
different sides of 1.00x), all runs of one table on one core (the
flushes and dictionary compression at levels 1, 2 and 19 on CPU 8, the
rest on CPU 40) in random order, at least 1.5 s each (1 s for
decompression). zstd-native / libzstd; above
1.00x, zstd-native is faster. The compressed sizes were equal in every
cell. The best of the runs is on the same side of 1.00x as the median
in every cell but one (dictionary compression of raw xml at L2 in 4 KiB
frames: 1.001x by the best run).

### 4 KiB frames

The first 1 MiB of each file, compressed in independent 4 KiB frames with
a reused context
(`profile_encode -- <file> <level> <native|c|structured> 1.5 4096`):

| File | L1 vs libzstd | L1 vs structured | L3 vs libzstd | L3 vs structured | L19 vs libzstd | L19 vs structured |
|---|---:|---:|---:|---:|---:|---:|
| dickens | 1.08x | 1.61x | 1.11x | 1.74x | 1.13x | 1.21x |
| mozilla | 1.03x | 1.27x | 1.06x | 1.41x | 1.41x | 0.86x |
| mr | 1.07x | 1.53x | 1.11x | 1.75x | 1.19x | 1.12x |
| nci | 1.07x | 1.60x | 1.08x | 1.88x | 1.56x | 1.08x |
| ooffice | 1.03x | 1.64x | 1.07x | 1.84x | 1.12x | 1.17x |
| osdb | 1.04x | 1.67x | 1.06x | 1.94x | 1.10x | 1.10x |
| reymont | 1.09x | 1.47x | 1.09x | 1.71x | 1.18x | 1.20x |
| samba | 1.05x | 1.54x | 1.07x | 1.78x | 1.41x | 1.10x |
| sao | 1.01x | 1.73x | 1.10x | 1.88x | 1.02x | 1.13x |
| webster | 1.06x | 1.57x | 1.08x | 1.79x | 1.27x | 1.19x |
| x-ray | 1.05x | 1.74x | 1.10x | 1.56x | 1.07x | 1.12x |
| xml | 1.06x | 1.55x | 1.07x | 1.76x | 1.60x | 1.11x |
| **geometric mean** | **1.05x** | **1.57x** | **1.08x** | **1.75x** | **1.24x** | **1.11x** |

The one cell below 1.00x is mozilla at L19 against structured-zstd
(0.86x; its output differs).

### Streaming compression with flushes

One streaming frame (`StreamEncoder` / `ZSTD_compressStream2`) of the
whole file, with a flush after every 1, 4, 16 or 64 KiB, default window
(`profile_encode -- <file> <level> <native-flush|c-flush> 1.5 <chunk>`):

| File, flush every | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| dickens, 1 KiB | 1.03x | 1.07x | 1.01x | 1.02x |
| dickens, 4 KiB | 1.01x | 1.06x | 1.05x | 1.07x |
| dickens, 16 KiB | 1.01x | 1.07x | 1.03x | 1.05x |
| dickens, 64 KiB | 1.02x | 1.05x | 1.03x | 1.05x |
| samba, 1 KiB | 1.07x | 1.04x | 1.02x | 1.05x |
| samba, 4 KiB | 1.03x | 1.04x | 1.05x | 1.09x |
| samba, 16 KiB | 1.04x | 1.05x | 1.05x | 1.11x |
| samba, 64 KiB | 1.04x | 1.05x | 1.05x | 1.11x |
| xml, 1 KiB | 1.08x | 1.07x | 1.06x | 1.05x |
| xml, 4 KiB | 1.04x | 1.06x | 1.06x | 1.06x |
| xml, 16 KiB | 1.06x | 1.08x | 1.09x | 1.08x |
| xml, 64 KiB | 1.06x | 1.09x | 1.09x | 1.08x |
| **geometric mean** | **1.04x** | **1.06x** | **1.05x** | **1.07x** |

No cell is below 1.00x.

### Dictionaries

Frames of 1, 4 or 16 KiB from up to 4 MiB of the file, each compressed
with a prepared 16 KiB dictionary (`EncoderDictionary` /
`ZSTD_createCDict`): raw content (the file's first 16 KiB) or trained
(ZDICT on 4 KiB pieces of the file's first 8 MiB)
(`ZSTD_BENCH_DICT=raw:16384|trained:16384 profile_encode -- <file> <level> <native-dict|c-dict> 1.5 <frame size>`):

| Dictionary, file, frame size | L1 | L2 | L3 | L4 | L19 |
|---|---:|---:|---:|---:|---:|
| raw, dickens, 1 KiB | 1.07x | 1.03x | 1.05x | 1.06x | 1.11x |
| raw, dickens, 4 KiB | 1.02x | 1.05x | 1.07x | 1.07x | 1.11x |
| raw, dickens, 16 KiB | 1.10x | 1.14x | 1.07x | 1.02x | 1.18x |
| raw, samba, 1 KiB | 1.06x | 1.02x | 1.05x | 1.04x | 1.25x |
| raw, samba, 4 KiB | 1.00x | 1.02x | 1.03x | 1.04x | 1.35x |
| raw, samba, 16 KiB | 1.12x | 1.12x | 1.02x | 1.01x | 1.44x |
| raw, xml, 1 KiB | 1.06x | 1.03x | 1.04x | 1.04x | 1.47x |
| raw, xml, 4 KiB | 0.99x | 1.00x | 0.99x | 1.00x | 1.69x |
| raw, xml, 16 KiB | 1.14x | 1.03x | 0.99x | 0.98x | 1.70x |
| trained, dickens, 1 KiB | 1.04x | 1.03x | 1.04x | 1.05x | 1.14x |
| trained, dickens, 4 KiB | 1.04x | 1.05x | 1.06x | 1.09x | 1.12x |
| trained, dickens, 16 KiB | 1.10x | 1.12x | 1.06x | 1.03x | 1.19x |
| trained, samba, 1 KiB | 1.04x | 1.02x | 1.05x | 1.05x | 1.32x |
| trained, samba, 4 KiB | 1.04x | 1.05x | 1.05x | 1.06x | 1.37x |
| trained, samba, 16 KiB | 1.10x | 1.12x | 1.03x | 1.01x | 1.46x |
| trained, xml, 1 KiB | 1.04x | 1.02x | 1.04x | 1.05x | 1.61x |
| trained, xml, 4 KiB | 1.02x | 1.03x | 1.01x | 1.04x | 1.63x |
| trained, xml, 16 KiB | 1.15x | 1.06x | 1.02x | 1.00x | 1.72x |
| **raw, geomean** | **1.06x** | **1.05x** | **1.03x** | **1.03x** | **1.35x** |
| **trained, geomean** | **1.06x** | **1.06x** | **1.04x** | **1.04x** | **1.38x** |

A regression from [#85](https://github.com/zcsizmadia/zstd-native/pull/85)
in dictionary compression at levels 3 and 4 (3% and 1%) was found by
this re-measure and fixed in
[#90](https://github.com/zcsizmadia/zstd-native/pull/90); against an
interleaved `61e24a1` build, `6f19057` is 1.03x at L3 (1.02-1.04x by
cell), 1.01x at L4 and within 0.5% at L1, L2 and L19.

Below 1.00x: raw xml in 4 KiB frames at L1, L2, L3 and L4 (0.986x,
0.996x, 0.987x and 0.997x) and in 16 KiB frames at L3 and L4 (0.989x and
0.982x).

### Streaming decompression

The libzstd-made frame of each file, decoded through the streaming APIs
(`profile_decode -- <file> <level> <native|c>-<mode> 1`). Geomean over the
12 files, slowest and fastest file in brackets:

| Mode | L3 frames | L19 frames |
|---|---:|---:|
| One-shot, for reference | 1.12x (1.06-1.18) | 1.12x (1.06-1.18) |
| Push (`StreamDecoder` / `ZSTD_decompressStream`), whole output | 1.12x (1.06-1.18) | 1.12x (1.04-1.18) |
| Push, 1 MiB output buffer | 1.11x (1.06-1.19) | 1.12x (1.04-1.18) |
| Push, 128 KiB output buffer | 1.12x (1.06-1.19) | 1.12x (1.03-1.18) |
| `io::Read` from a 128 KiB `BufReader`, 1 MiB reads | 1.11x (1.05-1.19) | 1.11x (1.04-1.17) |
| `io::Read`, 128 KiB reads | 1.11x (1.06-1.18) | 1.12x (1.04-1.17) |
| `read_to_end`, input in memory | 1.26x (1.11-2.19) | 1.40x (1.08-3.03) |
| `read_to_end`, through a 128 KiB `BufReader` | 1.25x (1.09-2.19) | 1.40x (1.08-3.00) |

Push into the whole output runs at one-shot speed (1287 against 1286
MiB/s geomean at L3), and `read_to_end` decodes straight into the `Vec`
([#73](https://github.com/zcsizmadia/zstd-native/pull/73)), up to 3x
libzstd's speed. No cell is below 1.00x. The lowest is nci at L19
(1.03-1.04x except one-shot), where both libraries are bimodal from run
to run (zstd-native one-shot: 3176-3481 MiB/s).

### Dictionary decompression

4 KiB frames compressed with a 110 KiB trained dictionary, decoded by
each library with the dictionary installed in different ways
(`cold_dict -- <native|c> <level> <mode>`). Each run takes its best of 7
rounds of 2048 frames; ns per frame, median of the runs, zstd-native /
libzstd (speed ratio):

| Mode | L1 | L3 | L19 |
|---|---:|---:|---:|
| `warm`: one dictionary, installed once | 4096 / 4325 (1.06x) | 3620 / 3908 (1.08x) | 3125 / 3110 (1.00x) |
| `reinstall`: the same dictionary installed before each frame | 4096 / 4328 (1.06x) | 3625 / 3910 (1.08x) | 3116 / 3101 (1.00x) |
| `switch`: one decompressor, the next of 2048 prepared (cold) dictionaries each frame | 15804 / 20402 (1.29x) | 15185 / 21066 (1.39x) | 14238 / 19276 (1.35x) |
| `fresh`: a cold decompressor with its own cold dictionary each frame, both sides | 18972 / 26184 (1.38x) | 18490 / 25920 (1.40x) | 17886 / 25146 (1.41x) |
| `ctx`: as `fresh`, but libzstd reuses one warm `DCtx` | 19126 / 20748 (1.08x) | 18320 / 20792 (1.13x) | 17956 / 19434 (1.08x) |
| `create`: parse a cold copy of the dictionary bytes, install, decode, free (the next one may land at the same address, warm for both) | 23102 / 22677 (0.98x) | 22644 / 22314 (0.99x) | 22110 / 21506 (0.97x) |
| `create-keep`: as `create`, the previous dictionary freed after the next is parsed (every one cold) | 24307 / 24760 (1.02x) | 23393 / 24617 (1.05x) | 23154 / 23254 (1.00x) |
| `parse`: only the parse of `create` | 18085 / 18492 (1.02x) | 18264 / 18824 (1.03x) | 18279 / 18338 (1.00x) |
| `parse-warm`: `parse` from one in-cache copy of the bytes | 7494 / 7700 (1.03x) | 7578 / 7640 (1.01x) | 7640 / 7670 (1.00x) |

[#87](https://github.com/zcsizmadia/zstd-native/pull/87) (a dictionary
keeps only the tables frames use, a fast FSE build for low-probability
symbols, the literal buffer made with the decompressor) cut `fresh` and
`ctx` by 14-15%, `switch` by 7-8% and `parse` by 3-5% since the previous
table; `create` changed definition (it now frees each dictionary, as
libzstd does; the old order is `create-keep`). `ctx`, a cold
decompressor against libzstd's warm context, went from 0.94-0.98x to
1.08-1.13x. Below 1.00x are `create` (0.97-0.99x) and `warm` and
`reinstall` at L19 (within 0.5%). The `ctx` cells at L1 and L3 vary 6-9%
between binary copies.

The whole Silesia corpus catenated, first 4 MiB in 4 KiB frames, each
compressed with a 110 KiB dictionary trained on them, all decoded by one
decompressor holding the dictionary (`ZSTD_BENCH_DICT=4096 profile_decode
-- cat <level> <native|c> 2`, `ZSTD_BENCH_CAT_MB=4`): 708 against 606 MiB/s
at L1 (1.17x), 743 against 620 at L3 (1.20x), 702 against 612 at L19
(1.15x).

## Per file

### Compression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 316.3 | 303.9 | 291.8 | 1.04x | 1.08x | 2.392 | 2.392 |
| mozilla | 458.2 | 432.2 | 401.7 | 1.06x | 1.14x | 2.565 | 2.555 |
| mr | 426.1 | 398.7 | 388.6 | 1.07x | 1.10x | 2.615 | 2.615 |
| nci | 943.4 | 854.3 | 787.1 | 1.10x | 1.20x | 11.779 | 11.788 |
| ooffice | 392.9 | 373.5 | 367.0 | 1.05x | 1.07x | 1.712 | 1.712 |
| osdb | 459.4 | 441.2 | 435.9 | 1.04x | 1.05x | 2.704 | 2.706 |
| reymont | 315.5 | 305.2 | 285.1 | 1.03x | 1.11x | 3.084 | 3.084 |
| samba | 536.2 | 514.3 | 488.8 | 1.04x | 1.10x | 3.937 | 3.917 |
| sao | 379.4 | 361.8 | 363.6 | 1.05x | 1.04x | 1.160 | 1.160 |
| webster | 358.8 | 346.3 | 331.2 | 1.04x | 1.08x | 3.035 | 3.036 |
| x-ray | 848.8 | 707.4 | 678.3 | 1.20x | 1.25x | 1.251 | 1.251 |
| xml | 762.9 | 714.7 | 664.0 | 1.07x | 1.15x | 7.710 | 7.713 |
| **geometric mean** | | | | **1.07x** | **1.11x** | | |

### Compression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 163.8 | 157.2 | 124.3 | 1.04x | 1.32x | 2.782 | 2.785 |
| mozilla | 247.6 | 231.4 | 163.4 | 1.07x | 1.52x | 2.810 | 2.809 |
| mr | 201.6 | 187.1 | 150.2 | 1.08x | 1.34x | 2.811 | 2.784 |
| nci | 697.4 | 654.8 | 454.9 | 1.07x | 1.53x | 11.840 | 11.888 |
| ooffice | 178.9 | 164.2 | 132.7 | 1.09x | 1.35x | 1.968 | 1.970 |
| osdb | 245.6 | 228.7 | 190.8 | 1.07x | 1.29x | 2.880 | 2.882 |
| reymont | 212.3 | 202.9 | 160.5 | 1.05x | 1.32x | 3.420 | 3.419 |
| samba | 333.5 | 315.1 | 235.0 | 1.06x | 1.42x | 4.360 | 4.376 |
| sao | 139.0 | 126.7 | 107.4 | 1.10x | 1.29x | 1.312 | 1.312 |
| webster | 200.9 | 191.1 | 145.4 | 1.05x | 1.38x | 3.427 | 3.430 |
| x-ray | 135.1 | 122.1 | 108.7 | 1.11x | 1.24x | 1.393 | 1.357 |
| xml | 531.1 | 501.3 | 365.8 | 1.06x | 1.45x | 8.414 | 8.416 |
| **geometric mean** | | | | **1.07x** | **1.37x** | | |

### Compression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 49.2 | 42.7 | 34.7 | 1.15x | 1.42x | 3.105 | 3.088 |
| mozilla | 77.2 | 65.7 | 56.5 | 1.18x | 1.37x | 3.073 | 3.055 |
| mr | 60.9 | 52.8 | 44.6 | 1.15x | 1.36x | 3.006 | 2.987 |
| nci | 133.5 | 109.9 | 96.2 | 1.21x | 1.39x | 14.935 | 14.970 |
| ooffice | 63.7 | 52.8 | 48.5 | 1.21x | 1.31x | 2.155 | 2.139 |
| osdb | 75.7 | 63.6 | 55.0 | 1.19x | 1.37x | 3.017 | 3.011 |
| reymont | 56.8 | 49.5 | 40.1 | 1.15x | 1.41x | 3.967 | 3.952 |
| samba | 97.0 | 81.7 | 69.1 | 1.19x | 1.40x | 4.967 | 4.932 |
| sao | 62.6 | 48.8 | 46.7 | 1.28x | 1.34x | 1.390 | 1.393 |
| webster | 60.4 | 52.6 | 43.7 | 1.15x | 1.38x | 4.041 | 4.014 |
| x-ray | 60.2 | 47.7 | 47.5 | 1.26x | 1.27x | 1.581 | 1.559 |
| xml | 125.7 | 104.0 | 90.6 | 1.21x | 1.39x | 10.340 | 10.342 |
| **geometric mean** | | | | **1.19x** | **1.37x** | | |

### Compression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 3.2 | 2.6 | 2.7 | 1.24x | 1.16x | 3.576 | 3.576 |
| mozilla | 5.1 | 3.6 | 4.4 | 1.42x | 1.16x | 3.400 | 3.392 |
| mr | 5.2 | 4.1 | 4.5 | 1.27x | 1.16x | 3.209 | 3.209 |
| nci | 3.8 | 2.3 | 3.4 | 1.66x | 1.12x | 20.152 | 20.134 |
| ooffice | 6.3 | 4.6 | 5.0 | 1.37x | 1.26x | 2.371 | 2.371 |
| osdb | 4.8 | 3.3 | 4.0 | 1.46x | 1.20x | 3.253 | 3.253 |
| reymont | 3.3 | 2.6 | 2.9 | 1.26x | 1.15x | 4.915 | 4.915 |
| samba | 5.2 | 3.6 | 4.7 | 1.45x | 1.12x | 5.543 | 5.533 |
| sao | 6.1 | 4.7 | 4.9 | 1.31x | 1.25x | 1.450 | 1.450 |
| webster | 3.2 | 2.4 | 2.8 | 1.32x | 1.14x | 4.777 | 4.775 |
| x-ray | 6.3 | 4.3 | 4.8 | 1.49x | 1.31x | 1.652 | 1.652 |
| xml | 5.5 | 3.3 | 4.8 | 1.70x | 1.16x | 11.801 | 11.803 |
| **geometric mean** | | | | **1.41x** | **1.18x** | | |

### Decompression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1357 | 1193 | 929 | 1.14x | 1.46x |
| mozilla | 1202 | 1122 | 1007 | 1.07x | 1.19x |
| mr | 1404 | 1320 | 1107 | 1.06x | 1.27x |
| nci | 2384 | 2203 | 2026 | 1.08x | 1.18x |
| ooffice | 1023 | 983 | 882 | 1.04x | 1.16x |
| osdb | 1499 | 1399 | 1305 | 1.07x | 1.15x |
| reymont | 1403 | 1209 | 1017 | 1.16x | 1.38x |
| samba | 1858 | 1706 | 1511 | 1.09x | 1.23x |
| sao | 1014 | 982 | 928 | 1.03x | 1.09x |
| webster | 1428 | 1286 | 1092 | 1.11x | 1.31x |
| x-ray | 1124 | 1062 | 1086 | 1.06x | 1.03x |
| xml | 2290 | 2126 | 1941 | 1.08x | 1.18x |
| **geometric mean** | | | | **1.08x** | **1.21x** |

### Decompression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1105 | 932 | 776 | 1.19x | 1.42x |
| mozilla | 1134 | 1047 | 918 | 1.08x | 1.24x |
| mr | 1229 | 1051 | 892 | 1.17x | 1.38x |
| nci | 2316 | 2137 | 1994 | 1.08x | 1.16x |
| ooffice | 944 | 872 | 762 | 1.08x | 1.24x |
| osdb | 1525 | 1386 | 1273 | 1.10x | 1.20x |
| reymont | 1264 | 1087 | 926 | 1.16x | 1.37x |
| samba | 1791 | 1601 | 1413 | 1.12x | 1.27x |
| sao | 840 | 792 | 694 | 1.06x | 1.21x |
| webster | 1205 | 1070 | 929 | 1.13x | 1.30x |
| x-ray | 855 | 769 | 714 | 1.11x | 1.20x |
| xml | 2249 | 2053 | 1907 | 1.10x | 1.18x |
| **geometric mean** | | | | **1.11x** | **1.26x** |

### Decompression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1218 | 1034 | 852 | 1.18x | 1.43x |
| mozilla | 1123 | 1027 | 898 | 1.09x | 1.25x |
| mr | 1237 | 1059 | 873 | 1.17x | 1.42x |
| nci | 3064 | 2864 | 2750 | 1.07x | 1.11x |
| ooffice | 943 | 855 | 731 | 1.10x | 1.29x |
| osdb | 1567 | 1413 | 1286 | 1.11x | 1.22x |
| reymont | 1468 | 1287 | 1112 | 1.14x | 1.32x |
| samba | 1955 | 1769 | 1571 | 1.11x | 1.24x |
| sao | 978 | 895 | 778 | 1.09x | 1.26x |
| webster | 1411 | 1242 | 1075 | 1.14x | 1.31x |
| x-ray | 744 | 642 | 522 | 1.16x | 1.43x |
| xml | 2850 | 2609 | 2420 | 1.09x | 1.18x |
| **geometric mean** | | | | **1.12x** | **1.28x** |

### Decompression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1269 | 1089 | 881 | 1.17x | 1.44x |
| mozilla | 925 | 842 | 768 | 1.10x | 1.20x |
| mr | 1171 | 1033 | 840 | 1.13x | 1.39x |
| nci | 3490 | 3297 | 3094 | 1.06x | 1.13x |
| ooffice | 767 | 683 | 581 | 1.12x | 1.32x |
| osdb | 1568 | 1388 | 1232 | 1.13x | 1.27x |
| reymont | 1536 | 1376 | 1190 | 1.12x | 1.29x |
| samba | 1847 | 1666 | 1488 | 1.11x | 1.24x |
| sao | 916 | 809 | 674 | 1.13x | 1.36x |
| webster | 1462 | 1286 | 1100 | 1.14x | 1.33x |
| x-ray | 680 | 578 | 472 | 1.18x | 1.44x |
| xml | 2989 | 2740 | 2521 | 1.09x | 1.19x |
| **geometric mean** | | | | **1.12x** | **1.30x** |


## Other implementations

A one-time snapshot at main `fecceab`, before the speedups of #52 and #53:
the other implementations aren't re-measured routinely. The same machine
and corpus, on CPU 8. The figures are zstd-native's speed divided by each
implementation's: above 1.00x, zstd-native is faster.

**Method:**
- Each implementation ran single-threaded on whole files in memory, with
  reused contexts and preallocated buffers.
- Each measurement ran at least 2 s for compression (8 s at level 19) and
  1.5 s for decompression. Java, C# and Node were warmed up for 3 s first.
- Every implementation decoded the same libzstd-made frames, and each
  decode was checked against the original file.

**The C builds:**
- **libzstd, gcc -O3:** libzstd 1.5.7 built with upstream's Makefile and
  timed with the same loop as the Rust benches.
- **CLI:** upstream's `zstd --single-thread -b`, which reports its fastest
  round and so slightly favours C.

| | `zstd` crate | libzstd, gcc -O3 | CLI gcc | CLI clang | CLI gcc `-march=native` | ZstdSharp 0.8.8 (C#) | klauspost/compress (Go) | aircompressor 3.14 (Java) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Compress L1 | 1.03x | 1.02x | 1.03x | 1.08x | 1.02x | 1.16x | 1.42x* | |
| Compress L3 | 1.00x | 0.99x | 1.00x | 1.02x | 1.07x | 1.18x | 1.20x* | 1.84x* |
| Compress L7 | 1.04x | 1.03x | 1.04x | 1.05x | 0.99x | | 0.62x* | |
| Compress L9 | 1.03x | 1.02x | 1.01x | 1.06x | 0.98x | 1.24x | | |
| Compress L11 | 0.98x | 0.98x | 0.98x | 1.06x | 0.99x | | 1.29x* | |
| Compress L19 | 1.30x | 1.30x | 1.31x | 1.33x | 1.15x | 1.38x | | |
| Decompress L1 | 1.05x | 1.05x | 1.05x | 1.07x | 1.09x | 1.27x | 1.14x | 1.84x |
| Decompress L3 | 1.09x | 1.08x | 1.08x | 1.11x | 1.14x | 1.22x | 1.15x | 1.75x |
| Decompress L9 | 1.08x | 1.08x | 1.08x | 1.12x | 1.15x | 1.21x | 1.17x | 1.78x |
| Decompress L19 | 1.10x | 1.09x | 1.09x | 1.13x | 1.15x | 1.20x | 1.15x | 1.77x |

\* **Different output:** these implementations don't produce libzstd's
output at that level, so their compression ratio differs.
- Go has presets, not levels. They are listed at the nearest level:
  Fastest at 1, Default at 3, Better at 7, Best at 11.
- Compared at equal compression ratio, zstd-native is 1.42x faster than
  Fastest, 1.41x faster than Default, 1.25x faster than Better and 2.36x
  faster than Best. Better compresses less than level 7, which is why it
  shows 0.62x.
- aircompressor's pure-Java compressor (`ZstdJavaCompressor`) is fixed at
  level 3. Its default `ZstdCompressor` uses a bundled native libzstd when
  one is present.

**Notes:**
- **ZstdSharp** produces libzstd's output (it is a C# port of libzstd
  1.5.7). It ran on .NET 10.
- **JavaScript:** both pure-JavaScript ports are about 10x slower at
  decompression (xml: fzstd 0.1.1 at 207 MiB/s, zstdify 1.4.0 at 246 MiB/s,
  against zstd-native's 2196 MiB/s), so they weren't measured further.
- **Bindings** to C libzstd (Python, zstd-jni, DataDog/zstd for Go) run at
  C's speed, minus the cost of calling across the language boundary.

**Where C was faster at `fecceab`:**
- **Level 11:** gcc-built libzstd was 2% faster.
- **Levels 7 and 9:** a build with `-march=native` for this exact CPU was
  1-2% faster.

Both gaps are closed by
[#53](https://github.com/zcsizmadia/zstd-native/pull/53) (see
[More levels and multithreading](#more-levels-and-multithreading)).

## Not measured yet

- **AArch64.** All hand-written assembly is x86-64; on ARM the Rust
  fallbacks run. CI tests them on Linux and macOS arm64, but speed hasn't
  been measured ([#14](https://github.com/zcsizmadia/zstd-native/issues/14)).
- **Intel.** Changes were checked main-vs-branch on an i7-12800H laptop
  (never slower, often faster), but there's no full Intel table yet
  ([#47](https://github.com/zcsizmadia/zstd-native/issues/47)). The BMI2
  choices and the huge-page threshold of #53 were tuned on Zen 3 only.
