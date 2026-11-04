# Security policy

zstd-native decodes untrusted input, so memory safety and correct rejection
of malformed data are treated as security issues.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/zcsizmadia/zstd-native/security/advisories/new),
not in public issues. Include the input that triggers the problem, or a
script that produces it, the version or commit, and the platform.

You should get a reply within a week. Fixes are released as a new patch
version and published as a GitHub security advisory and a RustSec advisory.

## In scope

- **Malformed input:** memory unsafety, panics, infinite loops or excessive
  memory use on any input. Decoders must return an error instead.
- **Leniency:** accepting data that libzstd 1.5.7 rejects, or giving output
  that differs from libzstd's for valid input.
- **Window limit:** bypassing the streaming decoder's window limit.
- **Compression:** a crash, undefined behaviour or excessive memory use on
  any input or parameters.

## How the code is checked

- **`unsafe` code:** limited to benchmarked hot loops and the x86-64
  assembly, each with a SAFETY comment and debug assertions. The `portable`
  feature turns all of it off, so the safe fallbacks are tested on every
  platform.
- **Fuzzing:** every push is fuzzed, differentially against C libzstd, with
  longer nightly runs.
- **Tests:** the test suite compares decoding of mutated frames and encoder
  output byte for byte with libzstd.
- **Miri:** checks the portable code paths on x86-64 and big-endian s390x.
- **Dependencies:** checked with `cargo-deny` (RustSec advisories,
  licenses). The library itself has no dependencies.
