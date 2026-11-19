# Contributing to tickertape

Thanks for your interest. Bug reports, questions about the crash contract, and small focused pull requests are all
welcome.

Use the [bug report or feature request forms](https://github.com/thomas0903/tickertape/issues/new/choose).
For bugs, a small reproduction plus the tickertape version, OS, CPU architecture and filesystem are especially useful.
Report possible vulnerabilities privately as described in [`SECURITY.md`](SECURITY.md).

## Before you start

- **Read the specs.** `docs/format.md` (byte layout) and `docs/crash-contract.md` (guarantees) are normative. A
  change to either needs a short design record in `docs/decisions/` (copy `_template.md`), in the same pull request.
- **Spec, then tests, then code.** Write down the behaviour (rustdoc, spec section or ADR), add tests that fail
  without the change, then implement. `docs/playbooks/add-feature.md` has the checklist.
- **`unsafe` lives in two modules, plus the C ABI crate.** In the core crate only `crates/tickertape/src/mmap.rs`
  (mappings and system calls) and `crates/tickertape/src/crc.rs` (CPU intrinsics, ADR 0019) may contain `unsafe`
  code. The C ABI crate `crates/tickertape-ffi` cannot avoid it (raw pointers from C, exported symbols), so it is a
  deliberate, scoped exception (ADR 0028): keep each block small, and document each `unsafe extern "C" fn`'s contract
  in a `# Safety` section (it is copied into the header). Every `unsafe` block needs a `// SAFETY:` comment. Changes
  there get extra review. FFI code never goes into the core crate.
  Outside the core crates, the iceoryx2 integration has `unsafe` code in exactly one module,
  `integrations/iceoryx2/src/dynamic.rs`, for iceoryx2 API that is only offered as `unsafe` (services whose types are
  known only at run time); the crate denies `unsafe_code` everywhere else. Its binary,
  `integrations/iceoryx2/src/main.rs`, has one `unsafe` call, which passes the user's trust in a recording on to the
  `unsafe fn` `Replayer::run_dynamic`. The same rules apply to both. Its allocation measurement test,
  `integrations/iceoryx2/tests/allocation.rs`, implements a counting global allocator (an `unsafe impl GlobalAlloc`
  that forwards to `System`), test code only, under the same rules.
- **No unverified claims.** Numbers in docs come from recorded runs (`docs/playbooks/run-benchmark.md`); guarantees
  name the test that checks them.

## The checks CI runs

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo +1.85 check --workspace --all-features --all-targets   # MSRV
cargo deny check
cargo test -p tickertape-tests --release -- --ignored        # long tests: 1M records, 250-kill loop, 20 s soak
cargo +nightly miri test -p tickertape --lib -- format crc map_ro   # Miri: format, CRC, mapped loads
RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release protocol_model
cargo check -p tickertape --all-features --target i686-unknown-linux-gnu   # 32-bit: no read-only mappings
cargo +nightly miri test -p tickertape-ffi --lib pure_         # Miri on the C ABI's argument handling
TICKERTAPE_FFI_SANITIZE=1 cargo test -p tickertape-ffi --features fault-injection   # C/C++ tests under ASan, UBSan
cbindgen --config crates/tickertape-ffi/cbindgen.toml --crate tickertape-ffi \
    --output crates/tickertape-ffi/include/tickertape.h && git diff --exit-code   # header drift (cbindgen 0.29.4)
python3 .github/scripts/check_abi_baseline.py crates/tickertape-ffi/abi/tickertape-baseline.h \
    crates/tickertape-ffi/include/tickertape.h                   # frozen C declarations (from 1.0)
python3 -m unittest discover -s .github/scripts -p 'test_*.py'  # release and ABI tooling
tests/compat/generate.sh --check-old-readers                 # released readers on new journals (ADR 0033)
go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12    # workflow lint
```

`cargo test --workspace` compiles and runs C and C++ programs, so it needs a C/C++ compiler; it also runs the
Python decoder if `python3` is available. On Linux, CI also runs the C programs under Valgrind
(`TICKERTAPE_FFI_VALGRIND=1`).

Changes to `crc.rs` should also pass the whole suite on tickertape's fallback path, which its hardware path
otherwise hides on every machine that has the CRC instruction:
`RUSTFLAGS="--cfg tickertape_force_soft_crc" cargo test --workspace --all-features`. This compiles out tickertape's
own hardware path only; the `crc32c` crate it falls back to still picks its hardware implementation on capable CPUs.

Changes to `crates/tickertape/src/reader.rs` or `crates/tickertape/src/format.rs` need the `replay` benchmark run
before and after on the same machine (`cargo bench -p tickertape-bench --bench replay`, on the base commit and on
the change), with both sets of figures in the pull request description. The readers' per-record path is short enough
that a change elsewhere in these files can change how the compiler builds it: an unrelated change once made small
records markedly slower to replay without touching the per-record code. The size and signature checks next to
`SlotOutcome` in `reader.rs` catch only the record header moving back into the values that path returns; no test
catches a slowdown. These figures are informal and belong in the pull request, not in the docs
(`docs/playbooks/run-benchmark.md` describes recorded runs).

CI runs these on Linux x86-64 and macOS, and the tests, long tests and fuzz smoke on aarch64 Linux too.
`cargo test --workspace` includes the format-v1 compatibility fixtures (`tests/compat/README.md`), which check the
CLI and the Python decoder too and need `python3` in CI. A weekly workflow (`.github/workflows/soak.yml`) runs the
soak for 90 minutes per platform; run it locally for longer before changing recovery, the writer or the readers:

```bash
TICKERTAPE_SOAK_SECS=1800 cargo test -p tickertape-tests --release --test soak -- --ignored --nocapture
```

It prints its seed and what it exercised; on failure it keeps the journal and logs in `target/tmp/soak-failures/`
and prints the command that replays the seed (`TICKERTAPE_TEST_SEED`).

On macOS, run the Linux checks in a VM or container too (`docs/playbooks/test-on-linux.md`), and keep test journals
on a local filesystem (not tmpfs or a shared folder) when you test durability behaviour.

The iceoryx2 integration (`integrations/iceoryx2`) is outside the workspace, with its own lockfile and MSRV (1.89).
Its checks, run from that directory:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo clippy --all-targets --locked -- -D warnings
cargo clippy --all-targets --no-default-features --locked -- -D warnings
cargo install --locked iceoryx2-cli@0.10.0   # once; the upstream-tool test needs `iox2`
TICKERTAPE_REQUIRE_IOX2=1 cargo test --all-features --locked
cargo test --locked
cargo test --no-default-features --locked
cargo +1.89 check --all-features --all-targets --locked
cargo-deny --manifest-path Cargo.toml --config ../../deny.toml --all-features check
```

Run its cross-process tests in release mode too (`--release`), and on Linux as well as macOS.

## Pull requests

- Keep each pull request to one change, with a clear description of what and why.
- Add an entry to `CHANGELOG.md` under "Unreleased".
- Commit messages: short, imperative, with an area prefix, e.g. `core: reject seals at the first slot`.
- New dependencies: say why, and check `cargo deny check` (licences must be permissive).

## Licence

By contributing, you agree that your contributions are dual-licensed under MIT OR Apache-2.0, as described in the
README.
