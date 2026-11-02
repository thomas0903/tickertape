# Playbook: release checklist

Publishing a new version is a maintainer decision. Preparing a change does not authorize a release.
After the owner authorizes that version, push its tag to start `.github/workflows/release.yml`.

## One-time configuration

The GitHub `release` environment allows the `main` branch and tags matching `v*`. It has no required reviewer;
the explicit version tag (or manual workflow dispatch with `publish=true`) authorizes publishing.

Each crate, `tickertape` and `tickertape-cli`, has its own crates.io Trusted Publisher entry:

| Setting | Value |
|---|---|
| GitHub repository owner | `thomas0903` |
| Repository | `tickertape` |
| Workflow filename | `release.yml` |
| Environment | `release` |

The workflow exchanges GitHub OIDC identity for a short-lived crates.io token. No long-lived publishing token is
stored as a repository secret. See the [crates.io setup documentation](https://crates.io/docs/trusted-publishing).
New crate names still need an initial, explicitly authorized API-token publish before Trusted Publishing can be configured.

`main` requires pull requests and all CI checks, including release tooling. No approving review is required for this
single-maintainer repository. Administrators are subject to the checks; force pushes and branch deletion are disabled.

## Prepare

1. Set `[workspace.package].version` and `[workspace.dependencies].tickertape.version` in `Cargo.toml`.
2. Add a dated `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md`, and update its compare link.
3. Check the packages and run the normal checks:

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo test --workspace --all-features --locked
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked
   cargo deny check
   cargo publish --dry-run --locked -p tickertape -p tickertape-cli
   cargo package --list -p tickertape
   cargo package --list -p tickertape-cli
   python3 -m unittest discover -s .github/scripts -p 'test_*.py' -v
   go run github.com/rhysd/actionlint/cmd/actionlint@v1.7.12
   gitleaks git --redact --no-banner
   ```

   Each crate must include both licences and its README, and exclude local agent tooling. Dry-running both packages
   together requires Cargo 1.90+. The release scripts require Python 3.11+; CI selects Python 3.12.
4. Merge the release preparation pull request and wait for all CI jobs on the resulting `main` commit to pass.
5. Confirm the owner has authorized publishing that exact version, then tag that commit:

   ```sh
   git tag -a vX.Y.Z <main-commit> -m "tickertape vX.Y.Z"
   git push origin vX.Y.Z
   ```

## Automatic release

The tag workflow:

1. Checks stable `vX.Y.Z` syntax, both workspace versions, tag ancestry on `main`, and successful CI on the tag commit.
2. Runs library/CLI tests and builds native Linux x86-64/ARM64 and macOS Intel/Apple Silicon binaries.
3. Authenticates through Trusted Publishing, verifies package preparation, then publishes the library and CLI in order.
4. Creates a draft GitHub release from the matching changelog section, uploads four archives plus `SHA256SUMS`,
   then publishes it. Existing release notes are preserved.
5. Verifies published package checksums and source commits, waits up to 15 minutes for both docs.rs builds, installs
   the CLI from crates.io, and downloads/checks/runs the Linux binary archive.

Monitor the [release workflow](https://github.com/thomas0903/tickertape/actions/workflows/release.yml). Publication is
complete only when its verification job passes. These are release checks, not timing benchmarks.

## Retry or backfill an existing release

From the Actions page, dispatch `release` from `main` with an existing `tag`:

- `publish=false` (default): build binaries and attach them to an existing GitHub release. No crate upload occurs.
- `verify_publishing=true`: also test the OIDC exchange and package dry run without uploading crates.
- `publish=true`: resume an authorized release. Already-published crate versions are skipped only when their
  checksum-verified source commit matches the tag. Missing crates are published in dependency order.

Equivalent binary backfill for v0.4.0:

```sh
gh workflow run release.yml --ref main -f tag=v0.4.0 -F publish=false -F verify_publishing=true
```

Runs for the same tag are serialized. Existing binary assets are reused only if their bytes match; different bytes
are refused rather than overwritten. A failed docs.rs build needs a patch release, never a yank just to fix docs.
If docs.rs is merely delayed, rerun the failed verification job. Inspect partial publication before retrying:
crates.io uploads are immutable, and rerunning does not undo published versions.

## After publishing

- Update the README and roadmap to reflect publication and test the installation instructions.
- Keep launch announcement drafts local under ignored `docs/launch/`; posting requires an explicit instruction.
- Bump to the next development version only when work on it starts.
