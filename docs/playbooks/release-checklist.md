# Playbook: release checklist

Publishing to crates.io and making the repository public are **maintainer decisions**. Preparation covers everything up to
the dry run and the tag, then stop. Run `cargo publish` only as a deliberate decision for that exact release.

## 1. Prepare

1. `main` is green in CI on Linux and macOS, and `git status` is clean.
2. Versions: set `version` in the workspace `Cargo.toml` (`[workspace.package]`) and the `tickertape` dependency
   version in `crates/tickertape-cli/Cargo.toml` to the new version.
3. `CHANGELOG.md`: move "Unreleased" to `## [X.Y.Z] - YYYY-MM-DD`, and add the compare link.
4. Checks:
   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   cargo test --workspace --all-features
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
   cargo deny check
   cargo publish --dry-run -p tickertape -p tickertape-cli
   ```
   Dry-running both crates in one command lets Cargo (1.90 and later) verify the CLI against the unpublished
   library from this workspace. Check the file lists with `cargo package --list -p <crate>`: each crate must contain
   `LICENSE-MIT`, `LICENSE-APACHE` and `README.md`, and no local tooling files (`AGENTS.md`, `CLAUDE.md`).
5. Hygiene: `rg -n 'TODO\(verify\)' README.md docs/` finds nothing; `rg -n '/Users/|/home/[a-z]' --hidden -g '!target'`
   and a secrets scan (e.g. `gitleaks detect`, or `rg -n -i 'api[_-]?key|secret|token|password'`) find nothing that
   shouldn't be published.
6. Commit `docs: release vX.Y.Z`, then tag: `git tag -a vX.Y.Z -m "tickertape vX.Y.Z"` and `git push origin vX.Y.Z`.

## 2. Publish (only when the owner asks)

Crate order matters, because the CLI depends on the library:

1. `cargo publish -p tickertape`
2. Wait until the version shows on <https://crates.io/crates/tickertape> (and the index has updated, about a minute).
3. `cargo publish --dry-run -p tickertape-cli` (now against the published library), then `cargo publish -p tickertape-cli`.
   (Cargo 1.90+ can also do both in order with `cargo publish -p tickertape -p tickertape-cli`.)
4. Check <https://docs.rs/tickertape> builds. If it fails, fix it in a patch release; never yank to fix docs.
5. Create the GitHub release from the tag with the CHANGELOG section as notes (maintainer).

## 3. After publishing

- Bump to the next development version only when work on it starts.
- Note the release in the project's planning notes.
