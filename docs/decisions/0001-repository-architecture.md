# 0001: Repository architecture for an agent-driven Rust library

- **Status:** accepted
- **Date:** 2026-10-31

## Context

tickertape is built mostly by coding agents (Claude Code, Codex, Gemini, coordinated through T3 Code), often in
parallel worktrees. Agents start each session without memory, so the repository has to carry the context, the
boundaries and the conventions. The repository will be public, so everything committed must be fit to publish.

## Options considered

1. **Single crate** with the CLI behind a feature flag: fewest files, but the CLI's heavy dependencies (Parquet,
   Arrow) leak into the library's dependency tree and MSRV, and tests, benches and examples all crowd one crate.
2. **Cargo workspace with a library crate, a CLI crate, and unpublished crates for tests, benches and examples:**
   library users get a small dependency tree; cross-process tests can ship their own helper binaries; lanes map to
   directories.

## Decision

- **Cargo workspace** (option 2): `crates/tickertape` (library), `crates/tickertape-cli` (binary `tickertape`), and
  `tests/` (`tickertape-tests`), `benches/` (`tickertape-bench`), `examples/` (`tickertape-examples`), all three with
  `publish = false`. `fuzz/` is a separate `cargo-fuzz` crate excluded from the workspace because it needs nightly.
- **`AGENTS.md` is canonical.** Every `CLAUDE.md` only contains `@AGENTS.md`, so all tools read the same rules. The
  root file holds the global rules; each lane has a short file with its own rules.
- **Lanes by directory** with branch prefixes: `core/`, `cli/`, `test/`, `bench/`, `examples/`, `docs/`. This keeps
  parallel agents from editing the same files.
- **Shared memory in `docs/`:** `status.md` is the dashboard and `decisions/` holds ADRs. Git history is the log.
- **Specs are documents, not just code:** `docs/format.md` and `docs/crash-contract.md` are the reference, and every
  guarantee names the test that checks it.
- **Toolchain:** `rust-toolchain.toml` pins the `stable` channel; the workspace declares an MSRV (`rust-version`)
  that CI checks separately. Edition 2024.
- **Platforms:** Linux and macOS, both in CI (`ubuntu-latest`, `macos-latest`). Local Linux runs use an OrbStack VM
  on the owner's Mac, and results from it are labelled as such.
- **Licence:** `MIT OR Apache-2.0`, the Rust ecosystem default.

## Consequences

- Agents must update `docs/status.md` at the end of each task, or the shared memory goes stale.
- The CLI only uses the library's public API, which keeps that API honest and complete.
- The fuzz crate is not covered by `cargo test --workspace`; CI builds it in a separate nightly job.
- Benchmark and test crates add build time to `cargo test --workspace`; long tests are `#[ignore]`d by default.
