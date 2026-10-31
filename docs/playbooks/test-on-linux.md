# Playbook: run the checks on Linux (OrbStack VM)

The owner develops on macOS. Linux runs locally in an [OrbStack](https://orbstack.dev) machine. OrbStack machines are
virtualised, so any benchmark or durability result from one is labelled **"OrbStack VM (Linux on macOS)"**, never
bare-metal Linux.

## One-time setup

```bash
orb create ubuntu tickertape
orb -m tickertape bash -lc 'sudo apt-get update && sudo apt-get install -y build-essential pkg-config curl git'
orb -m tickertape bash -lc 'mkdir -p $HOME/tmp && curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt'
```

## Running

The VM sees the Mac's home directory at the same path, so run commands from the repo directory. Two rules:

- **Separate target directory.** Linux build output must not mix with the macOS `target/`. Set
  `CARGO_TARGET_DIR=$HOME/.cache/tickertape-target` (that is the VM's own home, on its own disk).
- **Journals on the VM's own disk.** Tests create journals in `$TMPDIR`. In the OrbStack Ubuntu machine `/tmp` is
  `tmpfs` (RAM, where `fsync` does nothing), so set `TMPDIR=$HOME/tmp`, which is on the VM's btrfs disk. Never point
  tests at the shared Mac folder: its mmap and fsync behaviour is not what we test.

```bash
# from the repo root on the Mac
orb -m tickertape bash -lc 'cd "'"$PWD"'" && mkdir -p $HOME/tmp && \
  export CARGO_TARGET_DIR=$HOME/.cache/tickertape-target TMPDIR=$HOME/tmp && \
  cargo fmt --all --check && \
  cargo clippy --workspace --all-targets --all-features -- -D warnings && \
  cargo test --workspace --all-features'

# long tests (kill loops, 1M-record runs)
orb -m tickertape bash -lc 'cd "'"$PWD"'" && CARGO_TARGET_DIR=$HOME/.cache/tickertape-target TMPDIR=$HOME/tmp \
  cargo test -p tickertape-tests --release -- --ignored'
```

To check which filesystem the tests ran on: `orb -m tickertape bash -lc 'df -T $HOME/tmp'` (btrfs on 2026-10-31). Record it with any result.

## Docker alternative

`docker run --rm -v "$PWD":/src -w /src rust:latest cargo test --workspace` also works, but the container's `/tmp`
is an overlay filesystem. Prefer the OrbStack machine for crash and durability tests.
