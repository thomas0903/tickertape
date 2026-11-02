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

## Prebuilt CLI

Download an archive and `SHA256SUMS` from the same [GitHub release](https://github.com/thomas0903/tickertape/releases/latest).
Each archive contains the `tickertape` executable, both licences and installation notes.

| Platform | Archive suffix | Requirement |
|---|---|---|
| Linux, Intel/AMD 64-bit | `x86_64-unknown-linux-gnu.tar.gz` | glibc 2.35+ (Ubuntu 22.04 or newer, for example) |
| Linux, ARM64 | `aarch64-unknown-linux-gnu.tar.gz` | glibc 2.35+ |
| macOS, Intel | `x86_64-apple-darwin.tar.gz` | macOS 11+ |
| macOS, Apple Silicon | `aarch64-apple-darwin.tar.gz` | macOS 11+ |

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

These are dynamically linked glibc binaries on Linux, not musl/Alpine builds. macOS 14.4+ uses kernel reader wake-ups;
earlier macOS versions use the documented polling fallback. The
[crash contract](crash-contract.md) describes the supported storage configurations and validation coverage;
shipping a CLI for an architecture does not establish power-loss durability.

## Try the CLI

```sh
tickertape --help
tickertape inspect ./journal
tickertape verify ./journal
tickertape tail ./journal --no-follow --count 10
```

The [example application](../examples/README.md) creates a journal with synthetic market data. The
[CLI reference](cli.md) documents every command, output schema and exit code.
