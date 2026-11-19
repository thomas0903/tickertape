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
cargo test -p tickertape-tests --release -- --ignored        # long tests: 1M records, 250-kill loop
cargo +nightly miri test -p tickertape --lib format          # Miri on the format code
RUSTFLAGS="--cfg loom" cargo test -p tickertape --lib --release protocol_model
cargo +nightly miri test -p tickertape-ffi --lib pure_         # Miri on the C ABI's argument handling
TICKERTAPE_FFI_SANITIZE=1 cargo test -p tickertape-ffi --features fault-injection   # C/C++ tests under ASan, UBSan
cbindgen --config crates/tickertape-ffi/cbindgen.toml --crate tickertape-ffi \
    --output crates/tickertape-ffi/include/tickertape.h && git diff --exit-code   # header drift (cbindgen 0.29.4)
python3 .github/scripts/check_abi_baseline.py crates/tickertape-ffi/abi/tickertape-baseline.h \
    crates/tickertape-ffi/include/tickertape.h                   # frozen C declarations (from 1.0)
python3 -m unittest discover -s .github/scripts -p 'test_*.py'  # release and ABI tooling
```

`cargo test --workspace` compiles and runs C and C++ programs, so it needs a C/C++ compiler; it also runs the
Python decoder if `python3` is available. On Linux, CI also runs the C programs under Valgrind
(`TICKERTAPE_FFI_VALGRIND=1`).

On macOS, run the Linux checks in a VM or container too (`docs/playbooks/test-on-linux.md`), and keep test journals
on a local filesystem (not tmpfs or a shared folder) when you test durability behaviour.

## Pull requests

- Keep each pull request to one change, with a clear description of what and why.
- Add an entry to `CHANGELOG.md` under "Unreleased".
- Commit messages: short, imperative, with an area prefix, e.g. `core: reject seals at the first slot`.
- New dependencies: say why, and check `cargo deny check` (licences must be permissive).

## Licence

By contributing, you agree that your contributions are dual-licensed under MIT OR Apache-2.0, as described in the
README.
