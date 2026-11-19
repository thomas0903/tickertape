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
GitHub release instead of compiling it. This works for releases after v0.4.0.

```sh
cargo binstall tickertape-cli
```

On Linux it always installs the static musl archive, so it works whatever the system's glibc version. The crate's
metadata never lets cargo-binstall use a third-party build service. The release workflow installs the CLI this way
on Linux x86-64 and checks that the installed binary is the one in the release.

## Homebrew

For macOS (Apple Silicon and Intel) and Linux (x86-64 and ARM64), from the project's tap. This works for releases
after v0.4.0.

```sh
brew install thomas0903/tap/tickertape
```

The formula installs the release archives (the static musl ones on Linux) and checks their SHA-256 sums. The release
workflow updates it, then installs it on macOS (Apple Silicon) and runs `brew test`.

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

The static archives (from releases after v0.4.0) contain the same CLI, statically linked with the musl C library
that Rust bundles (musl 1.2.5 for current Rust; see the
[Rust platform support table](https://doc.rust-lang.org/stable/rustc/platform-support.html)). The release workflow
checks that each one has no dynamic loader and no shared-library dependencies, runs the CLI's tests with it, and
runs it in an empty root directory that contains nothing but the binary. They use musl's memory allocator instead of
glibc's. Parquet export speed with it has not been benchmarked yet, so prefer the glibc archive for large exports
where it runs.

For example, on Linux x86-64, after downloading both files:

```sh
grep 'x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS | sha256sum --check --strict
tar -xzf tickertape-v0.4.0-x86_64-unknown-linux-gnu.tar.gz
mkdir -p "$HOME/.local/bin"
cp tickertape-v0.4.0-x86_64-unknown-linux-gnu/tickertape "$HOME/.local/bin/"
"$HOME/.local/bin/tickertape" --version
```

On macOS, use `shasum -a 256 -c` instead of `sha256sum --check --strict`, and select the archive suffix for your CPU.
The macOS binaries are unsigned and are not notarized. If macOS blocks a downloaded binary, review the source and
release checksum, then allow it through System Settings → Privacy & Security, or install with Cargo.
Add `$HOME/.local/bin` to your shell's `PATH` if needed.

macOS 14.4+ uses kernel reader wake-ups; earlier macOS versions use the documented polling fallback. The
[crash contract](crash-contract.md) describes the supported storage configurations and validation coverage;
shipping a CLI for an architecture does not establish power-loss durability.

## Verify provenance and the SBOM

For releases after v0.4.0, every archive and SBOM has a [build provenance
attestation](https://docs.github.com/en/actions/security-for-github-actions/using-artifact-attestations) signed
by the release workflow, and every archive has a CycloneDX software bill of materials (`<archive name>.cdx.json` in
the same release), generated from the locked dependency graph and also attested. With the
[GitHub CLI](https://cli.github.com/):

```sh
gh attestation verify tickertape-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo thomas0903/tickertape \
  --signer-workflow thomas0903/tickertape/.github/workflows/release.yml
gh attestation verify tickertape-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo thomas0903/tickertape \
  --predicate-type https://cyclonedx.org/bom
```

The first command checks that the file was built by the release workflow of this repository; the second that the
SBOM was attested for it. The release workflow runs both for every file it publishes. The CLI binary also carries
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
