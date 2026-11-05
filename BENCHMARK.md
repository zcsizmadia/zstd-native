# Benchmarks

zstd-native 0.1.0 (main `ecc11d4`) against C libzstd 1.5.7 and
[structured-zstd](https://crates.io/crates/structured-zstd) 0.2.6, the
fastest other pure-Rust zstd crate. [ruzstd](https://crates.io/crates/ruzstd)
decompresses 3-7x slower than zstd-native, so it isn't in the tables.
Other levels and multithreading, measured against C only, are in
[More levels and multithreading](#more-levels-and-multithreading).

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
  level 19, 1 s for decompression), from three binary paths of different
  lengths. Each figure is the best of the three.
- **Why three paths:** speed can move several percent with where the
  binary's data and stack land in memory (see
  [#34](https://github.com/zcsizmadia/zstd-native/issues/34)). The best
  of three paths takes that noise out of the comparison.
- **Reproduce:**
  `cargo run --release -p zstd-bench --example profile_encode -- <file> <level> <native|c|structured> <seconds>`
  (and `profile_decode`), with `ZSTD_BENCH_CORPUS=<silesia dir>`.

Speeds are MiB/s of uncompressed data. Above 1.00x, zstd-native is faster.
**zstd-native's output is byte-identical to libzstd's**, so their ratios
are the same. structured-zstd's output differs from libzstd's (at level 19
on every file checked), and its ratios differ by up to 2.6% (x-ray at
level 3), in either direction.

## Summary (geometric mean over the 12 files)

| | vs libzstd 1.5.7 | vs structured-zstd 0.2.6 |
|---|---:|---:|
| Compression, level 1 | 1.04x (1.01-1.20) | 1.07x (1.02-1.23) |
| Compression, level 3 | 1.05x (1.02-1.08) | 1.33x (1.21-1.50) |
| Compression, level 9 | 1.13x (1.09-1.20) | 1.28x (1.19-1.33) |
| Compression, level 19 | 1.38x (1.19-1.69) | 1.17x (1.09-1.33) |
| Decompression, level 1 frames | 1.07x (1.02-1.14) | 1.20x (1.04-1.44) |
| Decompression, level 3 frames | 1.10x (1.05-1.18) | 1.24x (1.14-1.42) |
| Decompression, level 9 frames | 1.11x (1.07-1.17) | 1.27x (1.11-1.44) |
| Decompression, level 19 frames | 1.11x (1.06-1.19) | 1.29x (1.12-1.45) |

The ranges in brackets are the slowest and fastest file. zstd-native is
faster than both on every file, level and direction. Level 19 compression
speeds have one decimal (2.6-6.4 MiB/s), so single-file ratios there are
±2-4%.

## More levels and multithreading

Measured against C libzstd only.

### Levels 1-4

Best of 2 paths × 2 reps, interleaved, CPU 16, main `5d6d676`, against
the `zstd` crate ([#17](https://github.com/zcsizmadia/zstd-native/issues/17)):

| | L1 | L2 | L3 | L4 |
|---|---:|---:|---:|---:|
| Silesia geomean vs libzstd | 1.04x | 1.05x | 1.05x | 1.04x |

Below 1.00x: only the zstd C sources compressed in 16 KB frames, at L2
(0.98x) and L4 (0.97x).

### Levels 5-15 ([#53](https://github.com/zcsizmadia/zstd-native/pull/53))

Three changes:
- BMI2 builds of the lazy, row and btlazy2 parsers;
- the row finder keeps its row head in a register;
- match finder tables of 8 MiB and more go on transparent huge pages
  (Linux x86-64).

CPU 8, all 12 files, median over 3 copies of each binary, geomean,
zstd-native's speed divided by each implementation's:

| | `zstd` crate | libzstd, gcc -O3 | CLI clang | CLI gcc `-march=native` |
|---|---:|---:|---:|---:|
| L5 | 1.11x | 1.06x | 1.13x | 1.01x |
| L7 | 1.10x | 1.06x | 1.13x | 1.01x |
| L9 | 1.15x | 1.11x | 1.19x | 1.06x |
| L11 | 1.09x | 1.06x | 1.15x | 1.07x |
| L12 | 1.09x | 1.03x | 1.10x | 1.03x |
| L15 | 1.16x | 1.14x | 1.15x | 1.13x |

Single files below 1.00x:
- against `-march=native`: nci at L5 (0.94x) and L7 (0.97x), dickens and
  reymont at L5 (0.98-0.99x);
- xml at L12 (0.98x).

C gains as much from huge pages as zstd-native does, but only when the
user opts in (`GLIBC_TUNABLES=glibc.malloc.hugetlb=1`).

### Multithreaded compression ([#52](https://github.com/zcsizmadia/zstd-native/pull/52))

The fixes:
- **Copy aliasing:** the calling thread's input copy no longer hits
  glibc's 4K-aliasing `memcpy` path.
- **Job table:** with 8+ workers, the job table is twice libzstd's.

Silesia concatenated (211 MB), CPUs and memory of one socket, best of 2,
MiB/s. Each cell is zstd-native / libzstd (ratio):

| | One-shot | Streaming (128 KB writes) |
|---|---|---|
| L1, 4 workers | 1678 / 1503 (1.12x) | 1342 / 1318 (1.02x) |
| L1, 8 workers | 3181 / 2810 (1.13x) | 1970 / 1763 (1.12x) |
| L1, 16 workers | 5614 / 4519 (1.24x) | 2142 / 1847 (1.16x) |
| L3, 8 workers | 1491 / 1318 (1.13x) | 1226 / 1014 (1.21x) |
| L3, 16 workers | 2240 / 1780 (1.26x) | 1166 / 997 (1.17x) |
| L9, 8 workers | 302 / 288 (1.05x) | 280 / 263 (1.06x) |
| L9, 16 workers | 505 / 492 (1.03x) | 403 / 389 (1.03x) |
| L19, 8 workers | 14.5 / 11.8 (1.23x) | 14.4 / 11.6 (1.24x) |
| L19, 16 workers | 15.5 / 12.6 (1.23x) | 15.5 / 12.7 (1.22x) |

Below 1.00x: streaming with 4 workers at L3 (0.98x) and L9 (0.99x),
within run-to-run noise. The full 1-16 worker matrix is in #52.

## Per file

### Compression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 308.5 | 301.4 | 298.2 | 1.02x | 1.03x | 2.392 | 2.392 |
| mozilla | 442.5 | 430.5 | 410.2 | 1.03x | 1.08x | 2.565 | 2.555 |
| mr | 412.0 | 395.9 | 392.9 | 1.04x | 1.05x | 2.615 | 2.615 |
| nci | 890.9 | 852.8 | 798.5 | 1.04x | 1.12x | 11.779 | 11.788 |
| ooffice | 385.8 | 370.3 | 369.4 | 1.04x | 1.04x | 1.712 | 1.712 |
| osdb | 448.3 | 436.1 | 439.8 | 1.03x | 1.02x | 2.704 | 2.706 |
| reymont | 305.7 | 302.3 | 291.3 | 1.01x | 1.05x | 3.084 | 3.084 |
| samba | 516.6 | 511.5 | 496.0 | 1.01x | 1.04x | 3.937 | 3.917 |
| sao | 375.9 | 359.1 | 366.1 | 1.05x | 1.03x | 1.160 | 1.160 |
| webster | 348.5 | 345.1 | 339.5 | 1.01x | 1.03x | 3.035 | 3.036 |
| xml | 724.1 | 709.9 | 670.5 | 1.02x | 1.08x | 7.710 | 7.713 |
| x-ray | 841.9 | 699.5 | 683.7 | 1.20x | 1.23x | 1.251 | 1.251 |
| **geometric mean** | | | | **1.04x** | **1.07x** | | |

### Compression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 161.5 | 157.3 | 126.4 | 1.03x | 1.28x | 2.782 | 2.785 |
| mozilla | 244.2 | 230.2 | 166.8 | 1.06x | 1.46x | 2.810 | 2.809 |
| mr | 196.8 | 187.1 | 153.0 | 1.05x | 1.29x | 2.811 | 2.784 |
| nci | 687.4 | 657.7 | 458.8 | 1.05x | 1.50x | 11.840 | 11.888 |
| ooffice | 176.4 | 164.7 | 134.1 | 1.07x | 1.32x | 1.968 | 1.970 |
| osdb | 241.6 | 229.7 | 192.7 | 1.05x | 1.25x | 2.880 | 2.882 |
| reymont | 207.6 | 203.0 | 161.2 | 1.02x | 1.29x | 3.420 | 3.419 |
| samba | 326.8 | 315.4 | 236.2 | 1.04x | 1.38x | 4.360 | 4.376 |
| sao | 136.6 | 128.8 | 109.3 | 1.06x | 1.25x | 1.312 | 1.312 |
| webster | 197.6 | 190.3 | 145.7 | 1.04x | 1.36x | 3.427 | 3.430 |
| xml | 519.5 | 500.0 | 368.3 | 1.04x | 1.41x | 8.414 | 8.416 |
| x-ray | 133.5 | 123.3 | 110.7 | 1.08x | 1.21x | 1.393 | 1.357 |
| **geometric mean** | | | | **1.05x** | **1.33x** | | |

### Compression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 46.6 | 42.4 | 35.5 | 1.10x | 1.31x | 3.105 | 3.088 |
| mozilla | 73.7 | 65.5 | 58.0 | 1.13x | 1.27x | 3.073 | 3.055 |
| mr | 59.4 | 53.3 | 45.3 | 1.11x | 1.31x | 3.006 | 2.987 |
| nci | 121.8 | 111.3 | 96.2 | 1.09x | 1.27x | 14.935 | 14.970 |
| ooffice | 59.9 | 52.8 | 48.8 | 1.13x | 1.23x | 2.155 | 2.139 |
| osdb | 72.5 | 63.8 | 56.1 | 1.14x | 1.29x | 3.017 | 3.011 |
| reymont | 53.9 | 49.4 | 40.5 | 1.09x | 1.33x | 3.967 | 3.952 |
| samba | 91.5 | 81.4 | 70.0 | 1.12x | 1.31x | 4.967 | 4.932 |
| sao | 58.0 | 48.9 | 47.1 | 1.19x | 1.23x | 1.390 | 1.393 |
| webster | 57.1 | 52.4 | 44.4 | 1.09x | 1.29x | 4.041 | 4.014 |
| xml | 120.4 | 104.4 | 91.0 | 1.15x | 1.32x | 10.340 | 10.342 |
| x-ray | 57.4 | 47.8 | 48.4 | 1.20x | 1.19x | 1.581 | 1.559 |
| **geometric mean** | | | | **1.13x** | **1.28x** | | |

### Compression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 3.3 | 2.6 | 2.7 | 1.27x | 1.22x | 3.576 | 3.576 |
| mozilla | 5.1 | 3.6 | 4.4 | 1.42x | 1.16x | 3.400 | 3.392 |
| mr | 5.4 | 4.2 | 4.5 | 1.29x | 1.20x | 3.209 | 3.209 |
| nci | 3.7 | 2.3 | 3.4 | 1.61x | 1.09x | 20.152 | 20.134 |
| ooffice | 5.8 | 4.6 | 5.0 | 1.26x | 1.16x | 2.371 | 2.371 |
| osdb | 4.6 | 3.3 | 4.0 | 1.39x | 1.15x | 3.253 | 3.253 |
| reymont | 3.4 | 2.7 | 2.9 | 1.26x | 1.17x | 4.915 | 4.915 |
| samba | 5.2 | 3.6 | 4.6 | 1.44x | 1.13x | 5.543 | 5.533 |
| sao | 5.7 | 4.8 | 5.0 | 1.19x | 1.14x | 1.450 | 1.450 |
| webster | 3.2 | 2.4 | 2.7 | 1.33x | 1.19x | 4.777 | 4.775 |
| xml | 5.4 | 3.2 | 4.8 | 1.69x | 1.13x | 11.801 | 11.803 |
| x-ray | 6.4 | 4.3 | 4.8 | 1.49x | 1.33x | 1.652 | 1.652 |
| **geometric mean** | | | | **1.38x** | **1.17x** | | |

### Decompression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1338 | 1188 | 932 | 1.13x | 1.44x |
| mozilla | 1178 | 1121 | 1007 | 1.05x | 1.17x |
| mr | 1379 | 1321 | 1101 | 1.04x | 1.25x |
| nci | 2327 | 2195 | 2020 | 1.06x | 1.15x |
| ooffice | 1005 | 981 | 878 | 1.02x | 1.14x |
| osdb | 1471 | 1396 | 1302 | 1.05x | 1.13x |
| reymont | 1377 | 1206 | 1013 | 1.14x | 1.36x |
| samba | 1812 | 1705 | 1511 | 1.06x | 1.20x |
| sao | 1006 | 979 | 919 | 1.03x | 1.09x |
| webster | 1392 | 1284 | 1088 | 1.08x | 1.28x |
| xml | 2231 | 2119 | 1939 | 1.05x | 1.15x |
| x-ray | 1125 | 1063 | 1084 | 1.06x | 1.04x |
| **geometric mean** | | | | **1.07x** | **1.20x** |

### Decompression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1102 | 933 | 775 | 1.18x | 1.42x |
| mozilla | 1116 | 1044 | 917 | 1.07x | 1.22x |
| mr | 1212 | 1058 | 900 | 1.15x | 1.35x |
| nci | 2274 | 2122 | 1996 | 1.07x | 1.14x |
| ooffice | 932 | 872 | 760 | 1.07x | 1.23x |
| osdb | 1498 | 1386 | 1273 | 1.08x | 1.18x |
| reymont | 1258 | 1087 | 927 | 1.16x | 1.36x |
| samba | 1755 | 1591 | 1410 | 1.10x | 1.24x |
| sao | 834 | 791 | 692 | 1.05x | 1.21x |
| webster | 1131 | 1070 | 922 | 1.06x | 1.23x |
| xml | 2194 | 2051 | 1904 | 1.07x | 1.15x |
| x-ray | 849 | 767 | 719 | 1.11x | 1.18x |
| **geometric mean** | | | | **1.10x** | **1.24x** |

### Decompression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1214 | 1039 | 854 | 1.17x | 1.42x |
| mozilla | 1101 | 1025 | 932 | 1.07x | 1.18x |
| mr | 1225 | 1051 | 867 | 1.17x | 1.41x |
| nci | 3041 | 2838 | 2731 | 1.07x | 1.11x |
| ooffice | 931 | 855 | 731 | 1.09x | 1.27x |
| osdb | 1560 | 1413 | 1285 | 1.10x | 1.21x |
| reymont | 1448 | 1287 | 1112 | 1.13x | 1.30x |
| samba | 1919 | 1764 | 1565 | 1.09x | 1.23x |
| sao | 978 | 892 | 779 | 1.10x | 1.26x |
| webster | 1400 | 1242 | 1073 | 1.13x | 1.30x |
| xml | 2806 | 2604 | 2420 | 1.08x | 1.16x |
| x-ray | 748 | 638 | 521 | 1.17x | 1.44x |
| **geometric mean** | | | | **1.11x** | **1.27x** |

### Decompression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1272 | 1089 | 884 | 1.17x | 1.44x |
| mozilla | 913 | 855 | 771 | 1.07x | 1.18x |
| mr | 1178 | 1034 | 842 | 1.14x | 1.40x |
| nci | 3464 | 3260 | 3080 | 1.06x | 1.12x |
| ooffice | 760 | 684 | 582 | 1.11x | 1.31x |
| osdb | 1547 | 1386 | 1224 | 1.12x | 1.26x |
| reymont | 1502 | 1368 | 1189 | 1.10x | 1.26x |
| samba | 1807 | 1669 | 1475 | 1.08x | 1.23x |
| sao | 915 | 806 | 672 | 1.14x | 1.36x |
| webster | 1448 | 1281 | 1106 | 1.13x | 1.31x |
| xml | 2935 | 2736 | 2517 | 1.07x | 1.17x |
| x-ray | 684 | 577 | 472 | 1.19x | 1.45x |
| **geometric mean** | | | | **1.11x** | **1.29x** |

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
