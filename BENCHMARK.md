# Benchmarks

zstd-native 0.1.1 plus the unreleased changes (main `96cdb56`) against C
libzstd 1.5.7 and
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
  within 0.01x (level 9: 1.20x against libzstd), and no cell changes side
  of 1.00x. Cells whose copies differed by more than 5% got 2-4 more
  rounds.
- **One table per socket:** two tables ran at a time, with nothing else
  running: the compression tables on CPU 8 (socket 0), the decompression
  tables, level 2 and the small-frame tables on CPU 40 (socket 1), each
  with its memory on its own node (`numactl --membind`). Before the run, sao, osdb, nci
  and samba at levels 2 and 3 (both libraries, 3 copies, 3 rounds) ran
  alone on CPU 8, alone on CPU 40, on both at once, and alone on CPU 8
  again: the cells run in parallel were within 1.4% of the same cells
  run alone (geomean 0.998-1.001), the same spread as the repeat alone.
- **Why three copies:** speed can move several percent with where the
  binary's data and stack land in memory (see
  [#34](https://github.com/zcsizmadia/zstd-native/issues/34)). libzstd
  is bimodal at level 2 (and on some files at levels 5-12): often one
  run in three or four is 12-21% slower, with the same instruction count
  and more cycles (`perf stat`, nci at level 2: 7.86 against 6.35-6.46 billion
  cycles, 14.0 billion instructions each). The best of six runs takes
  that noise out of the comparison; level 2 ran six rounds.
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
| Compression, level 1 | 1.07x (1.04-1.20) | 1.11x (1.04-1.25) |
| Compression, level 3 | 1.07x (1.03-1.11) | 1.37x (1.25-1.54) |
| Compression, level 9 | 1.19x (1.15-1.28) | 1.37x (1.27-1.42) |
| Compression, level 19 | 1.40x (1.23-1.68) | 1.18x (1.13-1.31) |
| Decompression, level 1 frames | 1.07x (1.03-1.15) | 1.20x (1.03-1.44) |
| Decompression, level 3 frames | 1.11x (1.07-1.19) | 1.25x (1.15-1.42) |
| Decompression, level 9 frames | 1.11x (1.07-1.17) | 1.28x (1.13-1.41) |
| Decompression, level 19 frames | 1.12x (1.06-1.18) | 1.29x (1.13-1.44) |

The ranges in brackets are the slowest and fastest file. zstd-native is
faster than libzstd and structured-zstd on every file, level and
direction.

The previous table (#78, main `7eae61f`) had level 1 at 1.03x and level
3 at 1.00x. Since then the sequence encoder
([#83](https://github.com/zcsizmadia/zstd-native/pull/83)) made
zstd-native 2-3% faster at levels 1-4. The rest is libzstd: #78 ran four
tables at a time on one socket, and its libzstd figures at levels 1-4
were the outlier, up to 9% higher (sao and x-ray at level 3) than in this
run and the runs before and after it (`ecc11d4`, #83), with the same
libzstd build. Decompression moved by measurement only: a `7eae61f`
build, run interleaved with this one (six files, levels 3 and 19),
decodes at the same speed as this one (0.4% faster by the median,
0.1% by the best run, geomean).

Level 19 compression speeds have one decimal (2.3-6.4 MiB/s), so
single-file ratios there are ±2-4%.

## More levels and multithreading

Measured against C libzstd only, at main `96cdb56`.

### Levels 1-4

One level per core, against the `zstd` crate: best of the runs, and in
brackets the median (level 2 ran 6 rounds, the others 2):

| File | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| dickens | 1.04x (1.05x) | 1.05x (1.05x) | 1.04x (1.04x) | 1.02x (1.04x) |
| mozilla | 1.06x (1.07x) | 1.08x (1.09x) | 1.09x (1.09x) | 1.09x (1.08x) |
| mr | 1.07x (1.08x) | 1.08x (1.08x) | 1.07x (1.07x) | 1.06x (1.06x) |
| nci | 1.12x (1.12x) | 1.13x (1.12x) | 1.07x (1.06x) | 1.06x (1.06x) |
| ooffice | 1.06x (1.06x) | 1.07x (1.07x) | 1.09x (1.09x) | 1.09x (1.09x) |
| osdb | 1.05x (1.06x) | 1.07x (1.07x) | 1.06x (1.06x) | 1.06x (1.06x) |
| reymont | 1.04x (1.04x) | 1.05x (1.05x) | 1.04x (1.04x) | 1.04x (1.04x) |
| samba | 1.05x (1.06x) | 1.08x (1.08x) | 1.06x (1.06x) | 1.05x (1.05x) |
| sao | 1.05x (1.05x) | 1.06x (1.06x) | 1.10x (1.10x) | 1.08x (1.09x) |
| webster | 1.04x (1.04x) | 1.05x (1.05x) | 1.03x (1.04x) | 1.04x (1.03x) |
| x-ray | 1.20x (1.19x) | 1.13x (1.13x) | 1.11x (1.11x) | 1.09x (1.09x) |
| xml | 1.09x (1.09x) | 1.10x (1.11x) | 1.05x (1.05x) | 1.06x (1.06x) |
| **geometric mean** | **1.07x (1.07x)** | **1.08x (1.08x)** | **1.07x (1.07x)** | **1.06x (1.06x)** |

No file is below 1.00x; the slowest is dickens at L4 (1.02x). The
previous table had files at 0.98-1.00x at levels 1, 3 and 4; see the
[summary](#summary-geometric-mean-over-the-12-files) for what changed.

### Levels 5-15

One level per core, all 12 files, median of the runs (3 copies of each
binary, 2 rounds, 4 for cells whose copies differed by over 5%), geomean,
zstd-native's speed divided by each implementation's:

| | `zstd` crate | libzstd, gcc -O3 | CLI clang | CLI gcc `-march=native` |
|---|---:|---:|---:|---:|
| L5 | 1.21x | 1.14x | 1.13x | 1.10x |
| L7 | 1.16x | 1.13x | 1.15x | 1.08x |
| L9 | 1.20x | 1.18x | 1.21x | 1.13x |
| L11 | 1.17x | 1.18x | 1.26x | 1.17x |
| L12 | 1.18x | 1.18x | 1.22x | 1.15x |
| L15 | 1.17x | 1.17x | 1.16x | 1.13x |

No single file is below 1.03x against any build (the slowest: mr at L7
and dickens at L5 against `-march=native`, 1.04x). The row finder port
([#68](https://github.com/zcsizmadia/zstd-native/pull/68)) and the steadier
code placement ([#66](https://github.com/zcsizmadia/zstd-native/pull/66))
closed the gaps of [#53](https://github.com/zcsizmadia/zstd-native/pull/53)
(nci at L5 and L7 against `-march=native`, xml at L12).

The `zstd` crate's build is bimodal on some files at levels 5-12, as at
level 2 (nci at L5: 148-159 or 203-212 MiB/s from run to run), which
lowers its medians. By the best of the runs, zstd-native is 1.16x faster
than it at L5 and 1.15-1.19x at L7-L15. For the other builds, the
medians and the bests agree within 0.01x.

The C builds, from upstream's 1.5.7 sources with gcc 11.4 and clang 14:
- **libzstd, gcc -O3:** `make libzstd.a` in `lib/`, linked into the same
  bench binary through zstd-sys's pkg-config path
  (`ZSTD_SYS_USE_PKG_CONFIG`), so it runs the same loop as the `zstd`
  crate. In #53 this build was ~4% faster than the `zstd` crate's; now
  the two are within 3%, either way (by the best of the runs: gcc -O3
  faster at L5-L9, the `zstd` crate at L11-L12). #53 didn't record its
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
| L1, 4 workers | 1819 / 1610 (1.13x) | 1598 / 1458 (1.10x; median 1.06x) |
| L1, 8 workers | 3516 / 2998 (1.17x) | 2409 / 1998 (1.21x; median 1.09x) |
| L1, 16 workers | 6587 / 5245 (1.26x) | 2387 / 1972 (1.21x) |
| L3, 4 workers | 903 / 791 (1.14x) | 752 / 692 (1.09x; median 1.13x) |
| L3, 8 workers | 1603 / 1342 (1.20x) | 1201 / 1091 (1.10x; median 1.04x) |
| L3, 16 workers | 2309 / 1818 (1.27x) | 1533 / 1053 (1.46x; median 1.10x) |
| L9, 4 workers | 268 / 226 (1.19x) | 241 / 212 (1.14x) |
| L9, 8 workers | 404 / 323 (1.25x) | 342 / 290 (1.18x) |
| L9, 16 workers | 657 / 554 (1.19x) | 468 / 432 (1.08x; median 1.11x) |
| L19, 8 workers | 18.4 / 14.0 (1.31x) | 18.4 / 13.9 (1.33x) |
| L19, 16 workers | 18.7 / 14.1 (1.33x) | 18.6 / 14.0 (1.33x) |

zstd-native's streaming at L1 and L3 with 8-16 workers varies by 9-37%
from run to run (L3, 16 workers: 1120-1533 MiB/s, median 1141), libzstd's
by 2-4%; the extra rounds didn't change that, so those cells give both
figures. The compressed sizes were equal in every cell.

No cell is below 1.00x. The fixes behind these are in
[#52](https://github.com/zcsizmadia/zstd-native/pull/52), with the full
1-16 worker matrix:
- **Copy aliasing:** the calling thread's input copy no longer hits
  glibc's 4K-aliasing `memcpy` path.
- **Job table:** with 8+ workers, the job table is twice libzstd's.

## Small frames, flushes, dictionaries, streaming

Against C libzstd (the `zstd` crate) at main `96cdb56`, with structured-zstd
where it has the same API. Median of the runs: 3 copies of the binary, 2
rounds (more for cells whose copies differed by over 5%), all runs of one
table on CPU 40 in random order, at least 1.5 s each (1 s for
decompression). zstd-native / libzstd; above 1.00x, zstd-native is
faster. The compressed sizes were equal in every cell, and the best of
the runs is on the same side of 1.00x as the median in every cell.

### 4 KiB frames

The first 1 MiB of each file, compressed in independent 4 KiB frames with
a reused context
(`profile_encode -- <file> <level> <native|c|structured> 1.5 4096`):

| File | L1 vs libzstd | L1 vs structured | L3 vs libzstd | L3 vs structured | L19 vs libzstd | L19 vs structured |
|---|---:|---:|---:|---:|---:|---:|
| dickens | 1.09x | 1.61x | 1.10x | 1.75x | 1.14x | 1.21x |
| mozilla | 1.03x | 1.27x | 1.08x | 1.42x | 1.41x | 0.86x |
| mr | 1.07x | 1.53x | 1.11x | 1.76x | 1.18x | 1.12x |
| nci | 1.07x | 1.58x | 1.09x | 1.88x | 1.56x | 1.09x |
| ooffice | 1.04x | 1.65x | 1.07x | 1.86x | 1.12x | 1.19x |
| osdb | 1.04x | 1.68x | 1.07x | 1.94x | 1.10x | 1.11x |
| reymont | 1.10x | 1.48x | 1.11x | 1.72x | 1.17x | 1.20x |
| samba | 1.06x | 1.55x | 1.07x | 1.79x | 1.41x | 1.11x |
| sao | 1.02x | 1.74x | 1.06x | 1.88x | 1.02x | 1.13x |
| webster | 1.06x | 1.58x | 1.08x | 1.79x | 1.27x | 1.20x |
| x-ray | 1.03x | 1.76x | 1.08x | 1.57x | 1.08x | 1.13x |
| xml | 1.06x | 1.56x | 1.07x | 1.76x | 1.58x | 1.11x |
| **geometric mean** | **1.05x** | **1.58x** | **1.08x** | **1.75x** | **1.24x** | **1.12x** |

The one cell below 1.00x is mozilla at L19 against structured-zstd
(0.86x; its output differs).

### Streaming compression with flushes

One streaming frame (`StreamEncoder` / `ZSTD_compressStream2`) of the
whole file, with a flush after every 1, 4, 16 or 64 KiB, default window
(`profile_encode -- <file> <level> <native-flush|c-flush> 1.5 <chunk>`):

| File, flush every | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| dickens, 1 KiB | 1.05x | 1.08x | 1.01x | 1.02x |
| dickens, 4 KiB | 1.01x | 1.08x | 1.04x | 1.04x |
| dickens, 16 KiB | 1.01x | 1.05x | 1.02x | 1.04x |
| dickens, 64 KiB | 1.02x | 1.05x | 1.02x | 1.03x |
| samba, 1 KiB | 1.07x | 1.05x | 1.02x | 1.05x |
| samba, 4 KiB | 1.03x | 1.05x | 1.04x | 1.07x |
| samba, 16 KiB | 1.04x | 1.05x | 1.04x | 1.10x |
| samba, 64 KiB | 1.04x | 1.05x | 1.04x | 1.09x |
| xml, 1 KiB | 1.10x | 1.07x | 1.06x | 1.06x |
| xml, 4 KiB | 1.04x | 1.09x | 1.07x | 1.05x |
| xml, 16 KiB | 1.07x | 1.09x | 1.09x | 1.08x |
| xml, 64 KiB | 1.07x | 1.09x | 1.08x | 1.07x |
| **geometric mean** | **1.05x** | **1.07x** | **1.04x** | **1.06x** |

No cell is below 1.00x.

### Dictionaries

Frames of 1, 4 or 16 KiB from up to 4 MiB of the file, each compressed
with a prepared 16 KiB dictionary (`EncoderDictionary` /
`ZSTD_createCDict`): raw content (the file's first 16 KiB) or trained
(ZDICT on 4 KiB pieces of the file's first 8 MiB)
(`ZSTD_BENCH_DICT=raw:16384|trained:16384 profile_encode -- <file> <level> <native-dict|c-dict> 1.5 <frame size>`):

| Dictionary, file, frame size | L1 | L2 | L3 | L4 | L19 |
|---|---:|---:|---:|---:|---:|
| raw, dickens, 1 KiB | 1.08x | 1.06x | 1.06x | 1.07x | 1.12x |
| raw, dickens, 4 KiB | 1.04x | 1.07x | 1.07x | 1.10x | 1.11x |
| raw, dickens, 16 KiB | 1.10x | 1.15x | 1.07x | 1.04x | 1.19x |
| raw, samba, 1 KiB | 1.06x | 1.04x | 1.06x | 1.06x | 1.26x |
| raw, samba, 4 KiB | 1.02x | 1.03x | 1.04x | 1.05x | 1.34x |
| raw, samba, 16 KiB | 1.12x | 1.12x | 1.03x | 1.03x | 1.45x |
| raw, xml, 1 KiB | 1.09x | 1.05x | 1.06x | 1.08x | 1.47x |
| raw, xml, 4 KiB | 1.02x | 1.01x | 1.00x | 1.02x | 1.67x |
| raw, xml, 16 KiB | 1.14x | 1.04x | 1.02x | 1.00x | 1.71x |
| trained, dickens, 1 KiB | 1.04x | 1.04x | 1.03x | 1.05x | 1.13x |
| trained, dickens, 4 KiB | 1.05x | 1.07x | 1.09x | 1.08x | 1.12x |
| trained, dickens, 16 KiB | 1.10x | 1.13x | 1.06x | 1.05x | 1.20x |
| trained, samba, 1 KiB | 1.04x | 1.03x | 1.04x | 1.04x | 1.32x |
| trained, samba, 4 KiB | 1.04x | 1.05x | 1.06x | 1.06x | 1.36x |
| trained, samba, 16 KiB | 1.09x | 1.14x | 1.04x | 1.02x | 1.47x |
| trained, xml, 1 KiB | 1.04x | 1.02x | 1.03x | 1.05x | 1.60x |
| trained, xml, 4 KiB | 1.03x | 1.02x | 1.03x | 1.04x | 1.62x |
| trained, xml, 16 KiB | 1.14x | 1.08x | 1.02x | 1.01x | 1.73x |
| **raw, geomean** | **1.07x** | **1.06x** | **1.05x** | **1.05x** | **1.35x** |
| **trained, geomean** | **1.06x** | **1.06x** | **1.04x** | **1.04x** | **1.38x** |

No cell is below 1.00x. The lowest are raw xml at L4 in 16 KiB frames
(1.004x) and at L3 in 4 KiB frames (1.005x); the code histograms of
[#83](https://github.com/zcsizmadia/zstd-native/pull/83) lifted the 16
KiB frames at L3 and L4 from 0.96-0.99x.

### Streaming decompression

The libzstd-made frame of each file, decoded through the streaming APIs
(`profile_decode -- <file> <level> <native|c>-<mode> 1`). Geomean over the
12 files, slowest and fastest file in brackets:

| Mode | L3 frames | L19 frames |
|---|---:|---:|
| One-shot, for reference | 1.11x (1.07-1.17) | 1.12x (1.06-1.18) |
| Push (`StreamDecoder` / `ZSTD_decompressStream`), whole output | 1.11x (1.06-1.18) | 1.12x (1.02-1.18) |
| Push, 1 MiB output buffer | 1.11x (1.06-1.20) | 1.11x (1.04-1.17) |
| Push, 128 KiB output buffer | 1.11x (1.05-1.18) | 1.11x (1.04-1.17) |
| `io::Read` from a 128 KiB `BufReader`, 1 MiB reads | 1.11x (1.06-1.18) | 1.11x (1.04-1.17) |
| `io::Read`, 128 KiB reads | 1.10x (1.06-1.18) | 1.11x (1.04-1.17) |
| `read_to_end`, input in memory | 1.25x (1.09-2.18) | 1.39x (1.07-3.01) |
| `read_to_end`, through a 128 KiB `BufReader` | 1.24x (1.09-2.17) | 1.38x (1.07-2.99) |

Push into the whole output runs at one-shot speed (1275 MiB/s geomean at
L3 both ways), and `read_to_end` decodes straight into the `Vec`
([#73](https://github.com/zcsizmadia/zstd-native/pull/73)), up to 3x
libzstd's speed. No cell is below 1.00x. The lowest, nci at L19 with the
whole output (1.02x), is bimodal: zstd-native ran at 3250-3340 or 3460-3490
MiB/s from run to run (1.06x by the best run), as it does one-shot.

### Dictionary decompression

4 KiB frames compressed with a 110 KiB trained dictionary, decoded by
each library with the dictionary installed in different ways
(`cold_dict -- <native|c> <level> <mode>`). Each run takes its best of 7
rounds of 2048 frames; ns per frame, median of the runs, zstd-native /
libzstd (speed ratio):

| Mode | L1 | L3 | L19 |
|---|---:|---:|---:|
| `warm`: one dictionary, installed once | 4094 / 4318 (1.05x) | 3628 / 3909 (1.08x) | 3102 / 3115 (1.00x) |
| `reinstall`: the same dictionary installed before each frame | 4098 / 4326 (1.06x) | 3624 / 3898 (1.08x) | 3106 / 3105 (1.00x) |
| `switch`: one decompressor, the next of 2048 prepared (cold) dictionaries each frame | 17165 / 20410 (1.19x) | 16424 / 20732 (1.26x) | 15377 / 19656 (1.28x) |
| `fresh`: a cold decompressor with its own cold dictionary each frame, both sides | 22074 / 27492 (1.25x) | 21816 / 26316 (1.21x) | 20801 / 24828 (1.19x) |
| `ctx`: as `fresh`, but libzstd reuses one warm `DCtx` | 22178 / 20882 (0.94x) | 21325 / 20998 (0.98x) | 21040 / 19708 (0.94x) |
| `create`: parse a cold copy of the dictionary bytes, install, decode | 24960 / 23018 (0.92x) | 24034 / 22482 (0.94x) | 23606 / 21716 (0.92x) |
| `parse`: only the parse of `create` | 19028 / 18514 (0.97x) | 19156 / 18594 (0.97x) | 18914 / 18456 (0.98x) |

[#82](https://github.com/zcsizmadia/zstd-native/pull/82) made frames use
the dictionary's tables in place and keep a dictionary installed again
warm: `ctx`, the one mode the previous table measured, went from
0.79-0.83x to 0.94-0.98x. Below 1.00x are `ctx`, which compares a cold
decompressor with libzstd's warm context, and `create` and `parse`,
where zstd-native's dictionary parse is 2-3% slower than
`ZSTD_createDDict`. `warm` and `reinstall` at L19 are on par (within
0.5%).

The whole Silesia corpus catenated, first 4 MiB in 4 KiB frames, each
compressed with a 110 KiB dictionary trained on them, all decoded by one
decompressor holding the dictionary (`ZSTD_BENCH_DICT=4096 profile_decode
-- cat <level> <native|c> 2`, `ZSTD_BENCH_CAT_MB=4`): 707 against 608 MiB/s
at L1 (1.16x), 740 against 620 at L3 (1.19x), 707 against 613 at L19
(1.15x).

## Per file

### Compression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 316.5 | 303.0 | 292.1 | 1.04x | 1.08x | 2.392 | 2.392 |
| mozilla | 458.9 | 431.8 | 402.7 | 1.06x | 1.14x | 2.565 | 2.555 |
| mr | 426.4 | 396.7 | 388.5 | 1.07x | 1.10x | 2.615 | 2.615 |
| nci | 950.5 | 848.4 | 788.1 | 1.12x | 1.21x | 11.779 | 11.788 |
| ooffice | 394.2 | 370.5 | 367.1 | 1.06x | 1.07x | 1.712 | 1.712 |
| osdb | 460.2 | 436.4 | 435.1 | 1.05x | 1.06x | 2.704 | 2.706 |
| reymont | 316.2 | 304.1 | 286.0 | 1.04x | 1.11x | 3.084 | 3.084 |
| samba | 536.9 | 510.2 | 489.9 | 1.05x | 1.10x | 3.937 | 3.917 |
| sao | 379.4 | 360.4 | 364.2 | 1.05x | 1.04x | 1.160 | 1.160 |
| webster | 358.9 | 345.1 | 332.0 | 1.04x | 1.08x | 3.035 | 3.036 |
| x-ray | 848.8 | 710.1 | 679.9 | 1.20x | 1.25x | 1.251 | 1.251 |
| xml | 765.2 | 704.2 | 663.7 | 1.09x | 1.15x | 7.710 | 7.713 |
| **geometric mean** | | | | **1.07x** | **1.11x** | | |

### Compression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 164.6 | 157.9 | 125.1 | 1.04x | 1.32x | 2.782 | 2.785 |
| mozilla | 252.8 | 231.9 | 165.2 | 1.09x | 1.53x | 2.810 | 2.809 |
| mr | 201.7 | 188.3 | 150.6 | 1.07x | 1.34x | 2.811 | 2.784 |
| nci | 702.8 | 656.9 | 457.3 | 1.07x | 1.54x | 11.840 | 11.888 |
| ooffice | 178.7 | 164.6 | 132.5 | 1.09x | 1.35x | 1.968 | 1.970 |
| osdb | 244.5 | 231.4 | 191.4 | 1.06x | 1.28x | 2.880 | 2.882 |
| reymont | 212.6 | 203.9 | 160.0 | 1.04x | 1.33x | 3.420 | 3.419 |
| samba | 335.2 | 317.1 | 235.7 | 1.06x | 1.42x | 4.360 | 4.376 |
| sao | 140.9 | 128.2 | 107.3 | 1.10x | 1.31x | 1.312 | 1.312 |
| webster | 200.4 | 194.5 | 144.8 | 1.03x | 1.38x | 3.427 | 3.430 |
| x-ray | 135.0 | 121.8 | 108.4 | 1.11x | 1.25x | 1.393 | 1.357 |
| xml | 531.8 | 504.6 | 366.3 | 1.05x | 1.45x | 8.414 | 8.416 |
| **geometric mean** | | | | **1.07x** | **1.37x** | | |

### Compression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 49.1 | 42.8 | 34.9 | 1.15x | 1.41x | 3.105 | 3.088 |
| mozilla | 77.0 | 65.7 | 56.8 | 1.17x | 1.36x | 3.073 | 3.055 |
| mr | 60.7 | 53.0 | 44.5 | 1.15x | 1.36x | 3.006 | 2.987 |
| nci | 133.8 | 114.5 | 96.2 | 1.17x | 1.39x | 14.935 | 14.970 |
| ooffice | 63.6 | 52.7 | 48.4 | 1.21x | 1.31x | 2.155 | 2.139 |
| osdb | 75.8 | 64.0 | 55.1 | 1.18x | 1.38x | 3.017 | 3.011 |
| reymont | 56.8 | 49.5 | 40.0 | 1.15x | 1.42x | 3.967 | 3.952 |
| samba | 96.7 | 80.9 | 69.3 | 1.19x | 1.40x | 4.967 | 4.932 |
| sao | 62.3 | 48.5 | 46.2 | 1.28x | 1.35x | 1.390 | 1.393 |
| webster | 60.5 | 52.3 | 44.0 | 1.16x | 1.38x | 4.041 | 4.014 |
| x-ray | 60.0 | 47.7 | 47.4 | 1.26x | 1.27x | 1.581 | 1.559 |
| xml | 125.3 | 105.0 | 90.4 | 1.19x | 1.39x | 10.340 | 10.342 |
| **geometric mean** | | | | **1.19x** | **1.37x** | | |

### Compression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 3.2 | 2.6 | 2.8 | 1.23x | 1.16x | 3.576 | 3.576 |
| mozilla | 5.1 | 3.6 | 4.4 | 1.41x | 1.16x | 3.400 | 3.392 |
| mr | 5.3 | 4.2 | 4.5 | 1.27x | 1.17x | 3.209 | 3.209 |
| nci | 3.8 | 2.3 | 3.4 | 1.65x | 1.13x | 20.152 | 20.134 |
| ooffice | 6.2 | 4.6 | 5.0 | 1.36x | 1.25x | 2.371 | 2.371 |
| osdb | 4.8 | 3.3 | 4.0 | 1.45x | 1.20x | 3.253 | 3.253 |
| reymont | 3.4 | 2.7 | 2.9 | 1.26x | 1.15x | 4.915 | 4.915 |
| samba | 5.3 | 3.6 | 4.7 | 1.45x | 1.13x | 5.543 | 5.533 |
| sao | 6.2 | 4.7 | 4.9 | 1.32x | 1.25x | 1.450 | 1.450 |
| webster | 3.2 | 2.4 | 2.8 | 1.32x | 1.15x | 4.777 | 4.775 |
| x-ray | 6.4 | 4.3 | 4.8 | 1.48x | 1.31x | 1.652 | 1.652 |
| xml | 5.5 | 3.3 | 4.8 | 1.68x | 1.17x | 11.801 | 11.803 |
| **geometric mean** | | | | **1.40x** | **1.18x** | | |

### Decompression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1343 | 1195 | 930 | 1.12x | 1.44x |
| mozilla | 1188 | 1121 | 1006 | 1.06x | 1.18x |
| mr | 1402 | 1323 | 1101 | 1.06x | 1.27x |
| nci | 2342 | 2204 | 2020 | 1.06x | 1.16x |
| ooffice | 1016 | 983 | 881 | 1.03x | 1.15x |
| osdb | 1483 | 1399 | 1300 | 1.06x | 1.14x |
| reymont | 1385 | 1209 | 1016 | 1.15x | 1.36x |
| samba | 1836 | 1708 | 1509 | 1.07x | 1.22x |
| sao | 1009 | 982 | 927 | 1.03x | 1.09x |
| webster | 1410 | 1286 | 1090 | 1.10x | 1.29x |
| x-ray | 1124 | 1062 | 1086 | 1.06x | 1.03x |
| xml | 2263 | 2124 | 1938 | 1.07x | 1.17x |
| **geometric mean** | | | | **1.07x** | **1.20x** |

### Decompression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1097 | 932 | 775 | 1.18x | 1.42x |
| mozilla | 1122 | 1043 | 915 | 1.08x | 1.23x |
| mr | 1219 | 1051 | 893 | 1.16x | 1.37x |
| nci | 2292 | 2125 | 1993 | 1.08x | 1.15x |
| ooffice | 934 | 870 | 762 | 1.07x | 1.23x |
| osdb | 1508 | 1385 | 1274 | 1.09x | 1.18x |
| reymont | 1258 | 1089 | 925 | 1.16x | 1.36x |
| samba | 1770 | 1599 | 1412 | 1.11x | 1.25x |
| sao | 844 | 792 | 691 | 1.07x | 1.22x |
| webster | 1199 | 1005 | 918 | 1.19x | 1.31x |
| x-ray | 851 | 769 | 714 | 1.11x | 1.19x |
| xml | 2226 | 2052 | 1905 | 1.08x | 1.17x |
| **geometric mean** | | | | **1.11x** | **1.25x** |

### Decompression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1204 | 1035 | 852 | 1.16x | 1.41x |
| mozilla | 1099 | 1025 | 889 | 1.07x | 1.24x |
| mr | 1232 | 1057 | 875 | 1.17x | 1.41x |
| nci | 3103 | 2811 | 2739 | 1.10x | 1.13x |
| ooffice | 935 | 856 | 732 | 1.09x | 1.28x |
| osdb | 1559 | 1413 | 1289 | 1.10x | 1.21x |
| reymont | 1458 | 1286 | 1112 | 1.13x | 1.31x |
| samba | 1929 | 1768 | 1567 | 1.09x | 1.23x |
| sao | 971 | 893 | 777 | 1.09x | 1.25x |
| webster | 1407 | 1242 | 1076 | 1.13x | 1.31x |
| x-ray | 739 | 640 | 523 | 1.15x | 1.41x |
| xml | 2829 | 2608 | 2425 | 1.08x | 1.17x |
| **geometric mean** | | | | **1.11x** | **1.28x** |

### Decompression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1258 | 1087 | 881 | 1.16x | 1.43x |
| mozilla | 915 | 852 | 768 | 1.07x | 1.19x |
| mr | 1174 | 1027 | 839 | 1.14x | 1.40x |
| nci | 3484 | 3284 | 3086 | 1.06x | 1.13x |
| ooffice | 761 | 682 | 581 | 1.12x | 1.31x |
| osdb | 1563 | 1387 | 1223 | 1.13x | 1.28x |
| reymont | 1538 | 1367 | 1188 | 1.13x | 1.29x |
| samba | 1832 | 1667 | 1481 | 1.10x | 1.24x |
| sao | 905 | 807 | 674 | 1.12x | 1.34x |
| webster | 1451 | 1279 | 1097 | 1.13x | 1.32x |
| x-ray | 678 | 577 | 471 | 1.18x | 1.44x |
| xml | 2973 | 2744 | 2516 | 1.08x | 1.18x |
| **geometric mean** | | | | **1.12x** | **1.29x** |


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
