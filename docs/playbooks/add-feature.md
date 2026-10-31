# Playbook: add a feature

1. **Branch.** Create `<lane>/<slug>` in its own worktree. Read `docs/status.md` and the lane's `AGENTS.md`.
2. **Spec.** Write down the behaviour before the code:
   - Public API: the signature with full rustdoc (what it does, errors, panics: none, and an example), with a
     `todo!()` body if needed.
   - On-disk or protocol changes: update `docs/format.md` or `docs/crash-contract.md`, and write an ADR
     (`record-decision.md`). A format change bumps the format version.
3. **Tests.** Write tests that fail without the feature: unit tests next to the code, `proptest` for encode/decode,
   `tests/` for cross-process or crash behaviour, `assert_cmd` for CLI commands. Check that they fail.
4. **Code.** Implement until the tests pass. Keep `unsafe` in `crates/tickertape/src/mmap.rs` only, with `// SAFETY:`
   comments.
5. **Full check.** Run every command in `AGENTS.md` → Commands on macOS, and the test suite on Linux in the OrbStack VM
   (`test-on-linux.md`).
6. **Review.** Re-read your own diff. If you are a subagent, report back (see `delegate-work.md` → Briefs). If the
   change touches `mmap.rs`, recovery, the format or the crash contract, get a cross-provider review and address the
   findings.
7. **Docs.** Update `CHANGELOG.md` (Unreleased), any affected docs, and `docs/status.md`.
8. **Commit** with a lane prefix (`core: add segment roll`), staging files by path. Merge into `main` only when CI is
   green on both platforms.
