# Install tickertape

## Rust library

```sh
cargo add tickertape
```

The library's minimum Rust version is 1.85. See the [quickstart](../README.md#quickstart) and
[API documentation](https://docs.rs/tickertape/).

## CLI with Cargo

```sh
cargo install tickertape-cli --locked
tickertape --version
```

## Prebuilt CLI with cargo-binstall

[cargo-binstall](https://github.com/cargo-bins/cargo-binstall) downloads the archive for your platform from the
GitHub release instead of compiling it. This works from v0.5.1.

```sh
cargo binstall tickertape-cli
```

On Linux it always installs the static musl archive, so it works whatever the system's glibc version. The crate's
metadata never lets cargo-binstall use a third-party build service. The release workflow installs the CLI this way
on Linux x86-64 and checks that the installed binary is the one in the release.

## Homebrew

For macOS (Apple Silicon and Intel) and Linux (x86-64 and ARM64), from the project's tap. This works from v0.5.1.

```sh
brew install thomas0903/tap/tickertape
```

The formula installs the release archives (the static musl ones on Linux) and checks their SHA-256 sums. The release
workflow updates it, then installs it on macOS (Apple Silicon) and runs `brew test`.

## CLI with pipx or uv (PyPI)

Published from v0.5.1.

The same CLI, packaged as a Python wheel that contains only the `tickertape` executable (no Python code), for Linux
x86-64 and ARM64 (manylinux: glibc 2.17 or newer; musllinux: musl 1.2 or newer, such as Alpine) and macOS 11 or newer
(Intel and Apple Silicon):

```sh
pipx install tickertape-cli
tickertape --version
uvx --from tickertape-cli tickertape --version    # run without installing
```

The wheels are built with maturin from the same source as the archives (they are not the archives' binaries).
`python-wheels.yml` installs each wheel on its own platform (the musllinux ones in Alpine) and runs `--version`,
`verify` and `tail` on a journal written by v0.4.0; after a release it installs the published CLI with pipx. On
other platforms, pip finds no wheel: use Cargo instead.

## mise

```sh
mise use github:thomas0903/tickertape
```

On Linux, mise picks the glibc archive. For the static musl archive, use
`mise use 'github:thomas0903/tickertape[matching=musl]'`. Checked by hand with mise 2026.11.1, not in the release
workflow.

## Prebuilt CLI

Download an archive and `SHA256SUMS` from the same [GitHub release](https://github.com/thomas0903/tickertape/releases/latest).
Each archive contains the `tickertape` executable, both licences and installation notes.

| Platform | Archive suffix | Requirement |
|---|---|---|
| Linux, Intel/AMD 64-bit | `x86_64-unknown-linux-gnu.tar.gz` | glibc 2.35+ (Ubuntu 22.04 or newer, for example) |
| Linux, ARM64 | `aarch64-unknown-linux-gnu.tar.gz` | glibc 2.35+ |
| Linux, Intel/AMD 64-bit, static | `x86_64-unknown-linux-musl.tar.gz` | none: no C library needed |
| Linux, ARM64, static | `aarch64-unknown-linux-musl.tar.gz` | none: no C library needed |
| macOS, Intel | `x86_64-apple-darwin.tar.gz` | macOS 11+ |
| macOS, Apple Silicon | `aarch64-apple-darwin.tar.gz` | macOS 11+ |

The static archives (from v0.5.1) contain the same CLI, statically linked with the musl C library
that Rust bundles (musl 1.2.5 for current Rust; see the
[Rust platform support table](https://doc.rust-lang.org/stable/rustc/platform-support.html)). The release workflow
checks that each one has no dynamic loader and no shared-library dependencies, runs the CLI's tests with it, and
runs it in an empty root directory that contains nothing but the binary. They use musl's memory allocator instead of
glibc's. Parquet export speed with it has not been benchmarked yet, so prefer the glibc archive for large exports
where it runs.

For example, on Linux x86-64, after downloading both files:

```sh
grep 'x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS | sha256sum --check --strict
tar -xzf tickertape-v0.5.1-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
cp tickertape-v0.5.1-x86_64-unknown-linux-gnu/tickertape "$HOME/.local/bin/"
"$HOME/.local/bin/tickertape" --version
```

On macOS, use `shasum -a 256 -c` instead of `sha256sum --check --strict`, and select the archive suffix for your CPU.
The macOS binaries are unsigned and are not notarized. If macOS blocks a downloaded binary, review the source and
release checksum, then allow it through System Settings → Privacy & Security, or install with Cargo.
Add `$HOME/.local/bin` to your shell's `PATH` if needed.

macOS 14.4+ uses kernel reader wake-ups; earlier macOS versions use the documented polling fallback. The
[crash contract](crash-contract.md) describes the supported storage configurations and validation coverage;
shipping a CLI for an architecture does not establish power-loss durability.

## C library

Published from v0.5.1; to build the library from source instead, see
[`c-api.md`](c-api.md#building-and-linking).

The C ABI ([`c-api.md`](c-api.md)) and the C++ binding ([`cpp-api.md`](cpp-api.md)) as one archive per platform in
the [GitHub release](https://github.com/thomas0903/tickertape/releases/latest), listed in the same `SHA256SUMS`:

| Platform | Archive | Requirement |
|---|---|---|
| Linux, Intel/AMD 64-bit | `tickertape-c-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | glibc 2.35+ |
| Linux, ARM64 | `tickertape-c-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | glibc 2.35+ |
| macOS, Intel | `tickertape-c-vX.Y.Z-x86_64-apple-darwin.tar.gz` | macOS 11+ |
| macOS, Apple Silicon | `tickertape-c-vX.Y.Z-aarch64-apple-darwin.tar.gz` | macOS 11+ |

There is no musl archive: Rust's musl targets build no usable shared library. Each archive has one top-level
directory with:

- `include/tickertape.h` (C99, C++11) and `include/tickertape.hpp` (the C++17 binding);
- `lib/libtickertape.so.X.Y.Z` with the links `libtickertape.so.0.Y` (the soname before 1.0; `libtickertape.so.1`
  from 1.0) and `libtickertape.so`; on macOS `lib/libtickertape.X.Y.Z.dylib` with the install name
  `@rpath/libtickertape.0.Y.dylib` and the same links;
- `lib/libtickertape.a`;
- `lib/pkgconfig/tickertape.pc` (shared) and `tickertape-static.pc` (static, with the system libraries it needs),
  which find the directories relative to themselves, so the archive works wherever it is unpacked;
- both licences and a README.

```sh
tar -xzf tickertape-c-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
export PKG_CONFIG_PATH="$PWD/tickertape-c-vX.Y.Z-x86_64-unknown-linux-gnu/lib/pkgconfig"
cc app.c $(pkg-config --cflags --libs tickertape) -Wl,-rpath,"$(pkg-config --variable=libdir tickertape)" -o app
cc app.c $(pkg-config --cflags --libs tickertape-static) -o app-static
c++ -std=c++17 app.cpp $(pkg-config --cflags --libs tickertape-static) -o app-cpp
```

The shared library is not on the system's search path, hence the `-rpath` (or copy `lib/` to a directory the loader
searches). Before 1.0 the C ABI is unstable, and a program requires exactly the library version of its header.
`crates/tickertape-ffi/scripts/install-test.sh` checks each archive the way it is used: it unpacks it, checks the
soname or install name, builds the C examples and the C++ example through pkg-config against the shared and the static
library, checks which library each program loads, and records, follows live and replays journals with them. CI runs
it on Linux x86-64 and macOS (Apple Silicon) with every change, the release workflow on every archive, and again on
the published Linux x86-64 archive.

## Java (Maven Central)

Published from v0.5.1; to build it from source instead, see [`java-api.md`](java-api.md).

The binding JAR and the natives JAR of your platform (`linux-x86_64`, `linux-aarch64`, `macos-x86_64` or
`macos-aarch64`; the Linux libraries need glibc 2.35+), on JDK 25 or newer:

```xml
<dependency>
  <groupId>io.github.thomas0903</groupId>
  <artifactId>tickertape</artifactId>
  <version>X.Y.Z</version>
</dependency>
<dependency>
  <groupId>io.github.thomas0903</groupId>
  <artifactId>tickertape</artifactId>
  <version>X.Y.Z</version>
  <classifier>natives-linux-x86_64</classifier>
</dependency>
```

Run with `--enable-native-access=io.github.thomas0903.tickertape` (or `ALL-UNNAMED` on the class path); see
[`java-api.md`](java-api.md) for how the binding finds the library. Each natives JAR holds the shared library of the
matching C archive above. The files on Central are signed with the project's GPG key. The release workflow checks
that every file on Central is identical to the attested build and runs the binding's install test with the JARs
downloaded from Central.

## Verify provenance and the SBOM

From v0.5.1, every archive, wheel and JAR a release produces has a [build provenance
attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations) and an
attested CycloneDX software bill of materials generated from the locked dependency graph (the Java binding has no
runtime dependencies, so its SBOM lists none):

| Files | Signed by | SBOM |
|---|---|---|
| CLI and C library archives (GitHub release) | `release.yml` | `<archive name>.cdx.json` in the same release, itself attested |
| Python wheels and sdist, CLI wheels (PyPI) | `python-wheels.yml` | in the attestation |
| JARs (Maven Central) | `release.yml` | in the attestation (the natives JARs share their C archive's SBOM); the pom has provenance only |

With the [GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify tickertape-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo thomas0903/tickertape \
  --signer-workflow thomas0903/tickertape/.github/workflows/release.yml
gh attestation verify tickertape-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo thomas0903/tickertape \
  --predicate-type https://cyclonedx.org/bom
```

The first command checks that the file was built by the release workflow of this repository; the second that the
SBOM was attested for it (`gh attestation verify --format json` prints it). For a wheel, use
`--signer-workflow thomas0903/tickertape/.github/workflows/python-wheels.yml`. The release workflow runs both
commands for every archive and SBOM it attaches to the release. The CLI binary also carries
its locked dependency list, embedded by [`cargo auditable`](https://github.com/rust-secure-code/cargo-auditable)
(its README lists the tools that read it).

## Try the CLI

```sh
tickertape --help
tickertape inspect ./journal
tickertape verify ./journal
tickertape tail ./journal --no-follow --count 10
```

The [example application](../examples/README.md) creates a journal with synthetic market data. The
[CLI reference](cli.md) documents every command, output schema and exit code.
