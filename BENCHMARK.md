# Benchmarks

zstd-native 0.1.0 (main `fecceab`) against C libzstd 1.5.7 and
[structured-zstd](https://crates.io/crates/structured-zstd) 0.2.6, the
fastest other pure-Rust zstd crate. [ruzstd](https://crates.io/crates/ruzstd)
decompresses 3-7x slower than zstd-native, so it isn't in the tables.

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
| Compression, level 1 | 1.02x (0.99-1.18) | 1.06x (1.01-1.22) |
| Compression, level 3 | 1.00x (0.99-1.02) | 1.33x (1.18-1.48) |
| Compression, level 9 | 1.03x (1.01-1.06) | 1.18x (1.10-1.25) |
| Compression, level 19 | 1.30x (1.11-1.66) | 1.11x (1.06-1.23) |
| Decompression, level 1 frames | 1.05x (1.03-1.11) | 1.20x (1.03-1.43) |
| Decompression, level 3 frames | 1.09x (1.04-1.18) | 1.25x (1.15-1.42) |
| Decompression, level 9 frames | 1.09x (1.05-1.14) | 1.28x (1.14-1.43) |
| Decompression, level 19 frames | 1.09x (1.05-1.13) | 1.29x (1.16-1.44) |

The ranges in brackets are the slowest and fastest file.

## Per file

### Compression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 301.5 | 302.6 | 292.6 | 1.00x | 1.03x | 2.392 | 2.392 |
| mozilla | 432.5 | 430.3 | 401.8 | 1.00x | 1.08x | 2.565 | 2.555 |
| mr | 406.1 | 400.4 | 388.1 | 1.01x | 1.05x | 2.615 | 2.615 |
| nci | 878.0 | 852.1 | 788.3 | 1.03x | 1.11x | 11.779 | 11.788 |
| ooffice | 378.8 | 371.6 | 366.2 | 1.02x | 1.03x | 1.712 | 1.712 |
| osdb | 441.9 | 437.5 | 433.8 | 1.01x | 1.02x | 2.704 | 2.706 |
| reymont | 301.1 | 304.8 | 286.5 | 0.99x | 1.05x | 3.084 | 3.084 |
| samba | 506.0 | 513.1 | 489.5 | 0.99x | 1.03x | 3.937 | 3.917 |
| sao | 369.1 | 356.3 | 361.1 | 1.04x | 1.02x | 1.160 | 1.160 |
| webster | 341.0 | 344.7 | 337.2 | 0.99x | 1.01x | 3.035 | 3.036 |
| xml | 716.0 | 707.1 | 662.6 | 1.01x | 1.08x | 7.710 | 7.713 |
| x-ray | 826.7 | 699.7 | 675.4 | 1.18x | 1.22x | 1.251 | 1.251 |
| **geometric mean** | | | | **1.02x** | **1.06x** | | |

### Compression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 159.3 | 157.6 | 124.6 | 1.01x | 1.28x | 2.782 | 2.785 |
| mozilla | 239.3 | 241.8 | 164.2 | 0.99x | 1.46x | 2.810 | 2.809 |
| mr | 193.5 | 193.6 | 149.5 | 1.00x | 1.29x | 2.811 | 2.784 |
| nci | 669.2 | 658.6 | 452.1 | 1.02x | 1.48x | 11.840 | 11.888 |
| ooffice | 174.5 | 175.3 | 132.1 | 0.99x | 1.32x | 1.968 | 1.970 |
| osdb | 239.0 | 240.1 | 191.6 | 0.99x | 1.25x | 2.880 | 2.882 |
| reymont | 205.4 | 203.8 | 157.8 | 1.01x | 1.30x | 3.420 | 3.419 |
| samba | 323.5 | 321.8 | 234.5 | 1.00x | 1.38x | 4.360 | 4.376 |
| sao | 135.3 | 136.5 | 107.4 | 0.99x | 1.26x | 1.312 | 1.312 |
| webster | 193.1 | 191.7 | 144.5 | 1.01x | 1.34x | 3.427 | 3.430 |
| xml | 511.5 | 504.2 | 362.2 | 1.01x | 1.41x | 8.414 | 8.416 |
| x-ray | 130.4 | 129.8 | 110.3 | 1.00x | 1.18x | 1.393 | 1.357 |
| **geometric mean** | | | | **1.00x** | **1.33x** | | |

### Compression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 43.2 | 42.5 | 34.8 | 1.02x | 1.24x | 3.105 | 3.088 |
| mozilla | 68.0 | 65.9 | 57.5 | 1.03x | 1.18x | 3.073 | 3.055 |
| mr | 54.8 | 53.6 | 44.5 | 1.02x | 1.23x | 3.006 | 2.987 |
| nci | 107.3 | 105.6 | 93.9 | 1.02x | 1.14x | 14.935 | 14.970 |
| ooffice | 55.2 | 53.3 | 48.5 | 1.04x | 1.14x | 2.155 | 2.139 |
| osdb | 64.9 | 64.0 | 55.1 | 1.01x | 1.18x | 3.017 | 3.011 |
| reymont | 50.0 | 49.0 | 39.9 | 1.02x | 1.25x | 3.967 | 3.952 |
| samba | 82.4 | 78.7 | 68.6 | 1.05x | 1.20x | 4.967 | 4.932 |
| sao | 51.0 | 49.6 | 46.3 | 1.03x | 1.10x | 1.390 | 1.393 |
| webster | 52.5 | 52.2 | 44.1 | 1.01x | 1.19x | 4.041 | 4.014 |
| xml | 105.6 | 99.4 | 90.7 | 1.06x | 1.16x | 10.340 | 10.342 |
| x-ray | 52.1 | 49.5 | 46.9 | 1.05x | 1.11x | 1.581 | 1.559 |
| **geometric mean** | | | | **1.03x** | **1.18x** | | |

### Compression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd | Ratio (native = libzstd) | Ratio (structured-zstd) |
|---|---:|---:|---:|---:|---:|---:|---:|
| dickens | 2.9 | 2.6 | 2.7 | 1.11x | 1.07x | 3.576 | 3.576 |
| mozilla | 4.8 | 3.6 | 4.4 | 1.33x | 1.09x | 3.400 | 3.392 |
| mr | 4.9 | 4.2 | 4.4 | 1.17x | 1.11x | 3.209 | 3.209 |
| nci | 3.6 | 2.3 | 3.3 | 1.56x | 1.09x | 20.152 | 20.134 |
| ooffice | 5.4 | 4.5 | 4.8 | 1.20x | 1.12x | 2.371 | 2.371 |
| osdb | 4.4 | 3.3 | 3.9 | 1.33x | 1.13x | 3.253 | 3.253 |
| reymont | 3.2 | 2.6 | 2.8 | 1.23x | 1.14x | 4.915 | 4.915 |
| samba | 4.9 | 3.6 | 4.6 | 1.36x | 1.06x | 5.543 | 5.533 |
| sao | 5.2 | 4.6 | 4.8 | 1.13x | 1.08x | 1.450 | 1.450 |
| webster | 2.9 | 2.4 | 2.7 | 1.21x | 1.07x | 4.777 | 4.775 |
| xml | 5.3 | 3.2 | 4.7 | 1.66x | 1.13x | 11.801 | 11.803 |
| x-ray | 5.9 | 4.3 | 4.8 | 1.37x | 1.23x | 1.652 | 1.652 |
| **geometric mean** | | | | **1.30x** | **1.11x** | | |

### Decompression, level 1

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1315 | 1190 | 918 | 1.10x | 1.43x |
| mozilla | 1160 | 1112 | 991 | 1.04x | 1.17x |
| mr | 1360 | 1313 | 1081 | 1.04x | 1.26x |
| nci | 2298 | 2212 | 1990 | 1.04x | 1.16x |
| ooffice | 991 | 965 | 870 | 1.03x | 1.14x |
| osdb | 1453 | 1389 | 1289 | 1.05x | 1.13x |
| reymont | 1357 | 1222 | 1001 | 1.11x | 1.36x |
| samba | 1787 | 1704 | 1483 | 1.05x | 1.21x |
| sao | 997 | 970 | 909 | 1.03x | 1.10x |
| webster | 1369 | 1280 | 1072 | 1.07x | 1.28x |
| xml | 2201 | 2126 | 1914 | 1.03x | 1.15x |
| x-ray | 1107 | 1049 | 1074 | 1.05x | 1.03x |
| **geometric mean** | | | | **1.05x** | **1.20x** |

### Decompression, level 3

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1083 | 949 | 761 | 1.14x | 1.42x |
| mozilla | 1096 | 1033 | 900 | 1.06x | 1.22x |
| mr | 1199 | 1070 | 881 | 1.12x | 1.36x |
| nci | 2218 | 2122 | 1932 | 1.04x | 1.15x |
| ooffice | 916 | 861 | 751 | 1.06x | 1.22x |
| osdb | 1470 | 1385 | 1254 | 1.06x | 1.17x |
| reymont | 1237 | 1099 | 910 | 1.13x | 1.36x |
| samba | 1724 | 1606 | 1385 | 1.07x | 1.25x |
| sao | 822 | 779 | 682 | 1.05x | 1.21x |
| webster | 1172 | 997 | 852 | 1.18x | 1.38x |
| xml | 2163 | 2061 | 1879 | 1.05x | 1.15x |
| x-ray | 835 | 764 | 710 | 1.09x | 1.18x |
| **geometric mean** | | | | **1.09x** | **1.25x** |

### Decompression, level 9

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1193 | 1054 | 837 | 1.13x | 1.43x |
| mozilla | 1076 | 1014 | 870 | 1.06x | 1.24x |
| mr | 1210 | 1064 | 846 | 1.14x | 1.43x |
| nci | 2982 | 2798 | 2616 | 1.07x | 1.14x |
| ooffice | 917 | 853 | 716 | 1.07x | 1.28x |
| osdb | 1523 | 1413 | 1269 | 1.08x | 1.20x |
| reymont | 1416 | 1297 | 1086 | 1.09x | 1.30x |
| samba | 1870 | 1768 | 1537 | 1.06x | 1.22x |
| sao | 957 | 891 | 768 | 1.07x | 1.25x |
| webster | 1371 | 1238 | 1056 | 1.11x | 1.30x |
| xml | 2760 | 2616 | 2390 | 1.05x | 1.16x |
| x-ray | 730 | 648 | 509 | 1.13x | 1.43x |
| **geometric mean** | | | | **1.09x** | **1.28x** |

### Decompression, level 19

| File | zstd-native | libzstd | structured-zstd | vs libzstd | vs structured-zstd |
|---|---:|---:|---:|---:|---:|
| dickens | 1249 | 1105 | 870 | 1.13x | 1.44x |
| mozilla | 885 | 842 | 747 | 1.05x | 1.19x |
| mr | 1156 | 1040 | 829 | 1.11x | 1.39x |
| nci | 3388 | 3237 | 2860 | 1.05x | 1.19x |
| ooffice | 750 | 681 | 571 | 1.10x | 1.31x |
| osdb | 1528 | 1386 | 1193 | 1.10x | 1.28x |
| reymont | 1472 | 1385 | 1165 | 1.06x | 1.26x |
| samba | 1774 | 1657 | 1449 | 1.07x | 1.22x |
| sao | 902 | 815 | 660 | 1.11x | 1.37x |
| webster | 1419 | 1279 | 1077 | 1.11x | 1.32x |
| xml | 2885 | 2729 | 2491 | 1.06x | 1.16x |
| x-ray | 664 | 586 | 463 | 1.13x | 1.43x |
| **geometric mean** | | | | **1.09x** | **1.29x** |

## Other implementations

The same machine and corpus, on CPU 8. The figures are zstd-native's speed
divided by each implementation's: above 1.00x, zstd-native is faster.

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

**Where C is faster:**
- **Level 11:** gcc-built libzstd is 2% faster, with single files at
  0.96-1.00x.
- **Levels 7 and 9:** a build with `-march=native` for this exact CPU is
  1-2% faster.
- **Level 3:** it's even.

The lazy and row match finders (levels 5-15) are where more work could
close this gap.

## Not measured yet

- **AArch64.** All hand-written assembly is x86-64; on ARM the Rust
  fallbacks run. CI tests them on Linux and macOS arm64, but speed hasn't
  been measured ([#14](https://github.com/zcsizmadia/zstd-native/issues/14)).
- **Intel.** Changes were checked main-vs-branch on an i7-12800H laptop
  (never slower, often faster), but there's no full Intel table yet.
- **Multithreaded compression** at many workers
  ([#29](https://github.com/zcsizmadia/zstd-native/issues/29)).
