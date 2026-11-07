# Benchmarks

zstd-native 0.1.1 plus the unreleased changes (main `7eae61f`) against C
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
  level 19, 1 s for decompression), once from each of three copies of the
  binary (made with `cp`, at paths of different lengths). All runs of one
  file and level ran on the same core, in random order. Each figure is the
  best of the three copies. Each table (one level, one direction) ran
  on one core: CPUs 8, 12, 16 and 24, each on its own CCD, ran four tables
  at a time.
- **Why three copies:** speed can move several percent with where the
  binary's data and stack land in memory (see
  [#34](https://github.com/zcsizmadia/zstd-native/issues/34)). The best
  of three copies takes that noise out of the comparison.
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
| Compression, level 1 | 1.03x (0.98-1.19) | 1.07x (1.02-1.25) |
| Compression, level 3 | 1.00x (0.98-1.02) | 1.33x (1.21-1.50) |
| Compression, level 9 | 1.16x (1.10-1.22) | 1.35x (1.24-1.41) |
| Compression, level 19 | 1.40x (1.23-1.69) | 1.20x (1.13-1.32) |
| Decompression, level 1 frames | 1.06x (1.03-1.12) | 1.21x (1.04-1.45) |
| Decompression, level 3 frames | 1.10x (1.06-1.20) | 1.26x (1.15-1.42) |
| Decompression, level 9 frames | 1.10x (1.07-1.13) | 1.28x (1.15-1.42) |
| Decompression, level 19 frames | 1.10x (1.05-1.14) | 1.30x (1.12-1.45) |

The ranges in brackets are the slowest and fastest file. zstd-native is
faster than structured-zstd on every file, level and direction, and faster
than libzstd on every file at levels 9 and 19 and in decompression.

At levels 1 and 3 it is on par with libzstd: below 1.00x are reymont
(0.98x) and webster (0.99x) at level 1, and osdb (0.98x) and sao (0.99x)
at level 3. The previous table (main `ecc11d4`) had level 3 at 1.05x.
zstd-native's speeds are the same as then, and libzstd's are 1-8%
higher: an `ecc11d4` build, run interleaved with this one, measures the
same as this one, so the difference is in the measurement, not a
regression.

Level 19 compression speeds have one decimal (2.3-6.4 MiB/s), so
single-file ratios there are ±2-4%.

## More levels and multithreading

Measured against C libzstd only, at main `7eae61f`.

### Levels 1-4

One level per core, best of 3 copies, against the `zstd` crate:

| | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| Silesia geomean vs libzstd | 1.03x | 1.04x | 1.00x | 1.00x |
| Slowest-fastest file | 0.98-1.20x | 1.00-1.11x | 0.98-1.02x | 0.98-1.02x |

Single files below 1.00x:
- L1: reymont (0.98x), webster (0.99x);
- L3: sao (0.98x), osdb, ooffice (0.99x), mozilla (1.00x);
- L4: sao, osdb (0.98x), ooffice (0.99x), mozilla, webster (1.00x).

### Levels 5-15

One level per core, all 12 files, median over 3 copies of each
binary, geomean, zstd-native's speed divided by each implementation's:

| | `zstd` crate | libzstd, gcc -O3 | CLI clang | CLI gcc `-march=native` |
|---|---:|---:|---:|---:|
| L5 | 1.14x | 1.15x | 1.12x | 1.11x |
| L7 | 1.11x | 1.12x | 1.13x | 1.06x |
| L9 | 1.16x | 1.18x | 1.20x | 1.12x |
| L11 | 1.18x | 1.17x | 1.26x | 1.16x |
| L12 | 1.16x | 1.15x | 1.22x | 1.15x |
| L15 | 1.16x | 1.16x | 1.15x | 1.13x |

No single file is below 1.00x against any build. The row finder port
([#68](https://github.com/zcsizmadia/zstd-native/pull/68)) and the steadier
code placement ([#66](https://github.com/zcsizmadia/zstd-native/pull/66))
closed the gaps of [#53](https://github.com/zcsizmadia/zstd-native/pull/53)
(nci at L5 and L7 against `-march=native`, xml at L12).

The C builds, from upstream's 1.5.7 sources with gcc 11.4 and clang 14:
- **libzstd, gcc -O3:** `make libzstd.a` in `lib/`, linked into the same
  bench binary through zstd-sys's pkg-config path
  (`ZSTD_SYS_USE_PKG_CONFIG`), so it runs the same loop as the `zstd`
  crate. In #53 this build was ~4% faster than the `zstd` crate's; now
  the two are within 1%. #53 didn't record its build steps, so this may
  not be the same build.
- **CLI:** `make zstd` in `programs/` with `CC=clang`, or with
  `MOREFLAGS=-march=native` for gcc, run as `zstd -b<level>
  --single-thread -i<seconds>`. It reports its fastest round, which
  slightly favours C.

C gains as much from huge pages as zstd-native does, but only when the
user opts in (`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`).

### Multithreaded compression

Silesia concatenated (211 MB), CPUs and memory of one socket
(`numactl --cpunodebind=0 --membind=0`), nothing else running, best of
3 copies, MiB/s. Each cell is zstd-native / libzstd (ratio):

| | One-shot | Streaming (128 KB writes) |
|---|---|---|
| L1, 4 workers | 1744 / 1617 (1.08x) | 1570 / 1474 (1.07x) |
| L1, 8 workers | 3336 / 3037 (1.10x) | 2081 / 1954 (1.07x) |
| L1, 16 workers | 6235 / 5272 (1.18x) | 2343 / 1903 (1.23x) |
| L3, 4 workers | 871 / 839 (1.04x) | 766 / 701 (1.09x) |
| L3, 8 workers | 1553 / 1374 (1.13x) | 1157 / 1120 (1.03x) |
| L3, 16 workers | 2232 / 1832 (1.22x) | 1300 / 1065 (1.22x) |
| L9, 4 workers | 264 / 232 (1.14x) | 244 / 215 (1.14x) |
| L9, 8 workers | 400 / 327 (1.22x) | 333 / 304 (1.10x) |
| L9, 16 workers | 656 / 562 (1.17x) | 463 / 425 (1.09x) |
| L19, 8 workers | 18.2 / 14.2 (1.29x) | 18.3 / 13.9 (1.31x) |
| L19, 16 workers | 18.6 / 14.3 (1.30x) | 18.1 / 13.9 (1.31x) |

No cell is below 1.00x. The fixes behind these are in
[#52](https://github.com/zcsizmadia/zstd-native/pull/52), with the full
1-16 worker matrix:
- **Copy aliasing:** the calling thread's input copy no longer hits
  glibc's 4K-aliasing `memcpy` path.
- **Job table:** with 8+ workers, the job table is twice libzstd's.

## Small frames, flushes, dictionaries, streaming

Against C libzstd (the `zstd` crate) at main `7eae61f`, with structured-zstd
where it has the same API. Median over 3 copies of each binary, all runs
of one table row on one core in random order, at least 1.5 s each (1 s
for decompression). zstd-native / libzstd; above 1.00x, zstd-native is
faster. The compressed sizes were equal in every cell.

### 4 KiB frames

The first 1 MiB of each file, compressed in independent 4 KiB frames with
a reused context
(`profile_encode -- <file> <level> <native|c|structured> 1.5 4096`):

| File | L1 vs libzstd | L1 vs structured | L3 vs libzstd | L3 vs structured | L19 vs libzstd | L19 vs structured |
|---|---:|---:|---:|---:|---:|---:|
| dickens | 1.02x | 1.56x | 1.03x | 1.66x | 1.13x | 1.22x |
| mozilla | 1.00x | 1.26x | 1.01x | 1.39x | 1.41x | 0.87x |
| mr | 1.00x | 1.45x | 1.01x | 1.63x | 1.18x | 1.13x |
| nci | 1.03x | 1.55x | 1.05x | 1.82x | 1.56x | 1.09x |
| ooffice | 1.00x | 1.63x | 1.01x | 1.80x | 1.12x | 1.18x |
| osdb | 1.01x | 1.66x | 1.01x | 1.92x | 1.10x | 1.11x |
| reymont | 1.04x | 1.41x | 1.04x | 1.64x | 1.15x | 1.20x |
| samba | 1.03x | 1.50x | 1.03x | 1.72x | 1.41x | 1.11x |
| sao | 0.99x | 1.73x | 0.99x | 1.83x | 1.01x | 1.13x |
| webster | 1.02x | 1.52x | 1.04x | 1.73x | 1.25x | 1.19x |
| x-ray | 1.00x | 1.77x | 1.02x | 1.57x | 1.06x | 1.11x |
| xml | 1.03x | 1.52x | 1.04x | 1.71x | 1.58x | 1.11x |
| **geometric mean** | **1.01x** | **1.54x** | **1.02x** | **1.70x** | **1.23x** | **1.12x** |

Below 1.00x: sao at L1 and L3 (0.99x), ooffice and x-ray at L1 (1.00x),
and mozilla at L19 against structured-zstd (0.87x; its output differs).

### Streaming compression with flushes

One streaming frame (`StreamEncoder` / `ZSTD_compressStream2`) of the
whole file, with a flush after every 1, 4, 16 or 64 KiB, default window
(`profile_encode -- <file> <level> <native-flush|c-flush> 1.5 <chunk>`):

| File, flush every | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| dickens, 1 KiB | 1.04x | 1.02x | 1.00x | 1.00x |
| dickens, 4 KiB | 1.00x | 0.99x | 1.00x | 1.02x |
| dickens, 16 KiB | 0.98x | 0.98x | 0.98x | 1.00x |
| dickens, 64 KiB | 0.99x | 0.97x | 0.98x | 1.00x |
| samba, 1 KiB | 1.06x | 1.03x | 1.00x | 1.03x |
| samba, 4 KiB | 1.01x | 0.99x | 1.00x | 1.04x |
| samba, 16 KiB | 1.00x | 0.99x | 0.99x | 1.05x |
| samba, 64 KiB | 0.99x | 0.98x | 0.99x | 1.05x |
| xml, 1 KiB | 1.08x | 1.06x | 1.06x | 1.05x |
| xml, 4 KiB | 1.03x | 1.02x | 1.03x | 1.02x |
| xml, 16 KiB | 1.02x | 1.03x | 1.04x | 1.03x |
| xml, 64 KiB | 1.01x | 1.03x | 1.04x | 1.02x |
| **geometric mean** | **1.02x** | **1.01x** | **1.01x** | **1.03x** |

Below 1.00x: dickens and samba with 4-64 KiB flushes at L1-L3, by 1-3%.

### Dictionaries

Frames of 1, 4 or 16 KiB from up to 4 MiB of the file, each compressed
with a prepared 16 KiB dictionary (`EncoderDictionary` /
`ZSTD_createCDict`): raw content (the file's first 16 KiB) or trained
(ZDICT on 4 KiB pieces of the file's first 8 MiB)
(`ZSTD_BENCH_DICT=raw:16384|trained:16384 profile_encode -- <file> <level> <native-dict|c-dict> 1.5 <frame size>`):

| Dictionary, file, frame size | L1 | L2 | L3 | L4 | L19 |
|---|---:|---:|---:|---:|---:|
| raw, dickens, 1 KiB | 1.09x | 1.03x | 1.05x | 1.06x | 1.10x |
| raw, dickens, 4 KiB | 1.00x | 1.01x | 1.07x | 1.06x | 1.10x |
| raw, dickens, 16 KiB | 1.06x | 1.13x | 1.03x | 1.00x | 1.15x |
| raw, samba, 1 KiB | 1.07x | 1.03x | 1.05x | 1.06x | 1.24x |
| raw, samba, 4 KiB | 1.00x | 0.99x | 1.02x | 1.03x | 1.33x |
| raw, samba, 16 KiB | 1.09x | 1.10x | 1.00x | 1.00x | 1.41x |
| raw, xml, 1 KiB | 1.08x | 1.05x | 1.04x | 1.07x | 1.46x |
| raw, xml, 4 KiB | 0.99x | 1.00x | 0.97x | 1.00x | 1.67x |
| raw, xml, 16 KiB | 1.13x | 1.06x | 0.96x | 0.96x | 1.67x |
| trained, dickens, 1 KiB | 1.02x | 1.00x | 1.04x | 1.03x | 1.13x |
| trained, dickens, 4 KiB | 1.01x | 0.99x | 1.05x | 1.04x | 1.10x |
| trained, dickens, 16 KiB | 1.06x | 1.10x | 1.04x | 0.99x | 1.16x |
| trained, samba, 1 KiB | 1.02x | 1.01x | 1.05x | 1.04x | 1.31x |
| trained, samba, 4 KiB | 1.01x | 1.00x | 1.03x | 1.01x | 1.35x |
| trained, samba, 16 KiB | 1.06x | 1.11x | 0.97x | 0.99x | 1.43x |
| trained, xml, 1 KiB | 1.01x | 1.00x | 1.02x | 1.02x | 1.58x |
| trained, xml, 4 KiB | 1.01x | 1.00x | 1.03x | 1.00x | 1.61x |
| trained, xml, 16 KiB | 1.12x | 1.09x | 0.97x | 0.97x | 1.70x |
| **raw, geomean** | **1.06x** | **1.04x** | **1.02x** | **1.03x** | **1.33x** |
| **trained, geomean** | **1.04x** | **1.03x** | **1.02x** | **1.01x** | **1.36x** |

Below 1.00x by more than 1%: 16 KiB frames at L3 and L4 (xml 0.96-0.97x,
samba 0.97-0.99x, dickens 0.99x) and raw xml in 4 KiB frames at L3
(0.97x). The others under 1.00x are within 1%.

### Streaming decompression

The libzstd-made frame of each file, decoded through the streaming APIs
(`profile_decode -- <file> <level> <native|c>-<mode> 1`). Geomean over the
12 files, slowest and fastest file in brackets:

| Mode | L3 frames | L19 frames |
|---|---:|---:|
| One-shot, for reference | 1.09x (1.05-1.14) | 1.11x (1.07-1.14) |
| Push (`StreamDecoder` / `ZSTD_decompressStream`), whole output | 1.09x (1.05-1.14) | 1.10x (1.05-1.14) |
| Push, 1 MiB output buffer | 1.10x (1.03-1.20) | 1.08x (0.98-1.16) |
| Push, 128 KiB output buffer | 1.07x (1.02-1.12) | 1.09x (1.04-1.14) |
| `io::Read` from a 128 KiB `BufReader`, 1 MiB reads | 1.08x (1.04-1.12) | 1.09x (1.03-1.14) |
| `io::Read`, 128 KiB reads | 1.08x (1.04-1.12) | 1.08x (1.04-1.13) |
| `read_to_end`, input in memory | 1.24x (1.11-2.17) | 1.36x (1.10-2.99) |
| `read_to_end`, through a 128 KiB `BufReader` | 1.23x (1.08-2.14) | 1.38x (1.14-2.94) |

Push into the whole output now runs at one-shot speed (1280 against 1281
MiB/s geomean at L3), and `read_to_end` decodes straight into the `Vec`
([#73](https://github.com/zcsizmadia/zstd-native/pull/73)), up to 3x
libzstd's speed. The one cell below 1.00x, mr at L19 with 1 MiB buffers (0.98x),
is noise: its three copies ran at 961, 961 and 1076 MiB/s, libzstd's at
876-985.

### Cold dictionary

One 4 KiB frame per decompressor, each with its own copy of a 110 KiB
trained dictionary that is out of cache when used (libzstd: one `DDict`
per frame), `cold_dict -- <native|c> <level>`. ns per frame, median over
3 copies of each one's best of 7 rounds:

| | zstd-native | libzstd | vs libzstd |
|---|---:|---:|---:|
| L1 | 25908 | 21000 | 0.81x |
| L3 | 25348 | 21138 | 0.83x |
| L19 | 23832 | 18752 | 0.79x |

zstd-native is still 17-21% slower here. The prefetching sequence loop of
[#76](https://github.com/zcsizmadia/zstd-native/pull/76) cut this case by
23-27% ([#11](https://github.com/zcsizmadia/zstd-native/issues/11)).

## Per file

### Compression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 306.5 | 307.5 | 297.4 | 1.00x | 1.03x | 2.392 | 2.392 |
| mozilla | 443.5 | 436.0 | 405.1 | 1.02x | 1.09x | 2.565 | 2.555 |
| mr | 413.6 | 402.6 | 392.3 | 1.03x | 1.05x | 2.615 | 2.615 |
| nci | 906.4 | 863.1 | 796.6 | 1.05x | 1.14x | 11.779 | 11.788 |
| ooffice | 383.8 | 376.1 | 369.3 | 1.02x | 1.04x | 1.712 | 1.712 |
| osdb | 447.4 | 442.1 | 439.1 | 1.01x | 1.02x | 2.704 | 2.706 |
| reymont | 305.6 | 310.3 | 291.5 | 0.98x | 1.05x | 3.084 | 3.084 |
| samba | 519.4 | 520.0 | 496.6 | 1.00x | 1.05x | 3.937 | 3.917 |
| sao | 372.8 | 359.5 | 366.6 | 1.04x | 1.02x | 1.160 | 1.160 |
| webster | 348.1 | 350.5 | 338.1 | 0.99x | 1.03x | 3.035 | 3.036 |
| x-ray | 840.5 | 706.5 | 674.4 | 1.19x | 1.25x | 1.251 | 1.251 |
| xml | 738.3 | 718.9 | 672.7 | 1.03x | 1.10x | 7.710 | 7.713 |
| **geometric mean** | | | | **1.03x** | **1.07x** | | |

### Compression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 160.6 | 158.9 | 126.0 | 1.01x | 1.27x | 2.782 | 2.785 |
| mozilla | 246.2 | 244.4 | 164.1 | 1.01x | 1.50x | 2.810 | 2.809 |
| mr | 197.0 | 194.7 | 151.9 | 1.01x | 1.30x | 2.811 | 2.784 |
| nci | 682.6 | 666.8 | 458.0 | 1.02x | 1.49x | 11.840 | 11.888 |
| ooffice | 176.8 | 176.8 | 133.9 | 1.00x | 1.32x | 1.968 | 1.970 |
| osdb | 241.5 | 246.3 | 193.8 | 0.98x | 1.25x | 2.880 | 2.882 |
| reymont | 206.1 | 205.6 | 160.8 | 1.00x | 1.28x | 3.420 | 3.419 |
| samba | 325.9 | 325.9 | 235.3 | 1.00x | 1.38x | 4.360 | 4.376 |
| sao | 138.0 | 139.6 | 109.5 | 0.99x | 1.26x | 1.312 | 1.312 |
| webster | 195.1 | 194.7 | 144.8 | 1.00x | 1.35x | 3.427 | 3.430 |
| x-ray | 133.3 | 133.2 | 109.9 | 1.00x | 1.21x | 1.393 | 1.357 |
| xml | 518.8 | 510.5 | 368.4 | 1.02x | 1.41x | 8.414 | 8.416 |
| **geometric mean** | | | | **1.00x** | **1.33x** | | |

### Compression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 48.8 | 43.5 | 35.0 | 1.12x | 1.39x | 3.105 | 3.088 |
| mozilla | 76.6 | 67.4 | 57.1 | 1.14x | 1.34x | 3.073 | 3.055 |
| mr | 60.1 | 54.4 | 44.7 | 1.10x | 1.35x | 3.006 | 2.987 |
| nci | 132.3 | 109.8 | 95.6 | 1.21x | 1.38x | 14.935 | 14.970 |
| ooffice | 62.8 | 54.4 | 48.9 | 1.16x | 1.29x | 2.155 | 2.139 |
| osdb | 75.1 | 65.2 | 55.2 | 1.15x | 1.36x | 3.017 | 3.011 |
| reymont | 56.6 | 50.2 | 40.0 | 1.13x | 1.41x | 3.967 | 3.952 |
| samba | 95.9 | 79.7 | 69.4 | 1.20x | 1.38x | 4.967 | 4.932 |
| sao | 62.0 | 50.6 | 46.7 | 1.22x | 1.33x | 1.390 | 1.393 |
| webster | 59.8 | 52.4 | 43.5 | 1.14x | 1.38x | 4.041 | 4.014 |
| x-ray | 58.8 | 50.2 | 47.4 | 1.17x | 1.24x | 1.581 | 1.559 |
| xml | 124.7 | 107.1 | 90.2 | 1.16x | 1.38x | 10.340 | 10.342 |
| **geometric mean** | | | | **1.16x** | **1.35x** | | |

### Compression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 3.2 | 2.6 | 2.7 | 1.23x | 1.18x | 3.576 | 3.576 |
| mozilla | 5.2 | 3.7 | 4.5 | 1.42x | 1.17x | 3.400 | 3.392 |
| mr | 5.3 | 4.2 | 4.5 | 1.26x | 1.18x | 3.209 | 3.209 |
| nci | 3.8 | 2.3 | 3.3 | 1.65x | 1.13x | 20.152 | 20.134 |
| ooffice | 6.3 | 4.6 | 4.9 | 1.35x | 1.28x | 2.371 | 2.371 |
| osdb | 4.8 | 3.3 | 3.9 | 1.45x | 1.23x | 3.253 | 3.253 |
| reymont | 3.4 | 2.7 | 2.9 | 1.25x | 1.16x | 4.915 | 4.915 |
| samba | 5.3 | 3.6 | 4.6 | 1.44x | 1.14x | 5.543 | 5.533 |
| sao | 6.2 | 4.8 | 4.9 | 1.30x | 1.26x | 1.450 | 1.450 |
| webster | 3.2 | 2.4 | 2.7 | 1.32x | 1.17x | 4.777 | 4.775 |
| x-ray | 6.4 | 4.3 | 4.9 | 1.49x | 1.32x | 1.652 | 1.652 |
| xml | 5.6 | 3.3 | 4.7 | 1.69x | 1.17x | 11.801 | 11.803 |
| **geometric mean** | | | | **1.40x** | **1.20x** | | |

### Decompression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1344 | 1204 | 929 | 1.12x | 1.45x |
| mozilla | 1189 | 1124 | 1004 | 1.06x | 1.18x |
| mr | 1393 | 1330 | 1096 | 1.05x | 1.27x |
| nci | 2342 | 2243 | 2018 | 1.04x | 1.16x |
| ooffice | 1017 | 978 | 877 | 1.04x | 1.16x |
| osdb | 1478 | 1406 | 1298 | 1.05x | 1.14x |
| reymont | 1389 | 1235 | 1010 | 1.12x | 1.38x |
| samba | 1832 | 1723 | 1506 | 1.06x | 1.22x |
| sao | 1008 | 980 | 928 | 1.03x | 1.09x |
| webster | 1410 | 1296 | 1088 | 1.09x | 1.30x |
| x-ray | 1123 | 1061 | 1081 | 1.06x | 1.04x |
| xml | 2262 | 2147 | 1940 | 1.05x | 1.17x |
| **geometric mean** | | | | **1.06x** | **1.21x** |

### Decompression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1100 | 963 | 774 | 1.14x | 1.42x |
| mozilla | 1127 | 1053 | 921 | 1.07x | 1.22x |
| mr | 1219 | 1085 | 889 | 1.12x | 1.37x |
| nci | 2291 | 2161 | 1998 | 1.06x | 1.15x |
| ooffice | 935 | 875 | 762 | 1.07x | 1.23x |
| osdb | 1511 | 1407 | 1271 | 1.07x | 1.19x |
| reymont | 1261 | 1112 | 920 | 1.13x | 1.37x |
| samba | 1772 | 1632 | 1413 | 1.09x | 1.25x |
| sao | 844 | 789 | 686 | 1.07x | 1.23x |
| webster | 1219 | 1017 | 924 | 1.20x | 1.32x |
| x-ray | 849 | 771 | 715 | 1.10x | 1.19x |
| xml | 2227 | 2082 | 1905 | 1.07x | 1.17x |
| **geometric mean** | | | | **1.10x** | **1.26x** |

### Decompression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1207 | 1071 | 854 | 1.13x | 1.41x |
| mozilla | 1126 | 1035 | 903 | 1.09x | 1.25x |
| mr | 1232 | 1090 | 873 | 1.13x | 1.41x |
| nci | 3110 | 2863 | 2700 | 1.09x | 1.15x |
| ooffice | 937 | 864 | 734 | 1.08x | 1.28x |
| osdb | 1561 | 1438 | 1293 | 1.09x | 1.21x |
| reymont | 1456 | 1316 | 1114 | 1.11x | 1.31x |
| samba | 1948 | 1797 | 1573 | 1.08x | 1.24x |
| sao | 976 | 906 | 778 | 1.08x | 1.25x |
| webster | 1406 | 1271 | 1080 | 1.11x | 1.30x |
| x-ray | 744 | 660 | 523 | 1.13x | 1.42x |
| xml | 2840 | 2648 | 2425 | 1.07x | 1.17x |
| **geometric mean** | | | | **1.10x** | **1.28x** |

### Decompression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1261 | 1124 | 884 | 1.12x | 1.43x |
| mozilla | 968 | 847 | 771 | 1.14x | 1.26x |
| mr | 1171 | 1049 | 836 | 1.12x | 1.40x |
| nci | 3473 | 3299 | 3105 | 1.05x | 1.12x |
| ooffice | 761 | 692 | 582 | 1.10x | 1.31x |
| osdb | 1560 | 1414 | 1221 | 1.10x | 1.28x |
| reymont | 1537 | 1399 | 1189 | 1.10x | 1.29x |
| samba | 1838 | 1700 | 1478 | 1.08x | 1.24x |
| sao | 907 | 825 | 671 | 1.10x | 1.35x |
| webster | 1447 | 1306 | 1096 | 1.11x | 1.32x |
| x-ray | 680 | 594 | 470 | 1.14x | 1.45x |
| xml | 2971 | 2778 | 2519 | 1.07x | 1.18x |
| **geometric mean** | | | | **1.10x** | **1.30x** |


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
