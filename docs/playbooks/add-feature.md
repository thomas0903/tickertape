# Playbook: add a feature

1. **Branch.** Create a branch named `<area>/<slug>` (areas: `core`, `cli`, `test`, `bench`, `examples`, `docs`).
2. **Spec.** Write down the behaviour before the code:
   - Public API: the signature with full rustdoc (what it does, errors, panics: none, and an example), with a
     `todo!()` body if needed.
   - On-disk or protocol changes: update `docs/format.md` or `docs/crash-contract.md`, and write an ADR
     (`record-decision.md`). A format change bumps the format version.
3. **Tests.** Write tests that fail without the feature: unit tests next to the code, `proptest` for encode/decode,
   `tests/` for cross-process or crash behaviour, `assert_cmd` for CLI commands. Check that they fail.
4. **Code.** Implement until the tests pass. Keep `unsafe` in `crates/tickertape/src/mmap.rs` only, with `// SAFETY:`
   comments.
5. **Full check.** Run every command in `CONTRIBUTING.md` → "The checks CI runs", and the test suite on Linux too
   (`test-on-linux.md`).
6. **Review.** Re-read your own diff. If the change touches `mmap.rs`, recovery, the format or the crash contract, get
   an independent review and address the findings.
7. **Docs.** Update `CHANGELOG.md` (Unreleased) and any affected docs.
8. **Commit** with an area prefix (`core: add segment roll`), staging files by path. Merge into `main` only when CI is
   green on both platforms.
