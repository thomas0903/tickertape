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

The Homebrew tap is the public repository `thomas0903/homebrew-tap`. The `release` environment holds the secret
`HOMEBREW_TAP_TOKEN`: a fine-grained token for that repository only, with contents read and write. Only the step
that pushes `Formula/tickertape.rb` sees it, and only in runs that publish. Renew it before it expires; an expired
token fails the `homebrew` job after everything else is published (rerun that job with a new token).

Build provenance and SBOM attestations need no setting: artifact attestations are available to public repositories.
They are signed through the public Sigstore instance, so each attestation is also recorded in its public
transparency log.

`main` requires pull requests and all CI checks, including release tooling. No approving review is required for this
single-maintainer repository. Administrators are subject to the checks; force pushes and branch deletion are disabled.

## Prepare

1. Set `[workspace.package].version` and `[workspace.dependencies].tickertape.version` in `Cargo.toml`. Set the
   C header's version macros to match (`TICKERTAPE_VERSION_MAJOR`, `_MINOR`, `_PATCH` in
   `crates/tickertape-ffi/src/lib.rs`; the build fails until they do) and regenerate the header with cbindgen 0.29.4
   (the command is in `crates/tickertape-ffi/cbindgen.toml`).
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

   The release tooling tests include the check that the CLI's cargo-binstall templates resolve to the archive names
   that `release.py package` writes (`python3 .github/scripts/release.py check-binstall --source . --tag vX.Y.Z`
   runs the same check by hand).

   If `.github/scripts/homebrew.py` changed, render the formula from the latest release and lint it from a throwaway
   local tap (never push it). Releases up to v0.4.0 have no musl archives, so stand in their glibc sums for them:

   ```sh
   gh release download vX.Y.Z --pattern SHA256SUMS --dir /tmp/sums
   sed -n 's/-gnu.tar.gz$/-musl.tar.gz/p' /tmp/sums/SHA256SUMS > /tmp/sums/musl  # v0.4.0 and older only
   cat /tmp/sums/musl >> /tmp/sums/SHA256SUMS                                      # v0.4.0 and older only
   mkdir -p /tmp/homebrew-local && git -C /tmp/homebrew-local init
   python3 .github/scripts/homebrew.py --tag vX.Y.Z --checksums /tmp/sums/SHA256SUMS --output /tmp/homebrew-local/Formula/tickertape.rb
   git -C /tmp/homebrew-local add -A && git -C /tmp/homebrew-local commit -m formula
   brew tap local/tap /tmp/homebrew-local
   brew audit --strict local/tap/tickertape && brew style local/tap/tickertape
   brew untap local/tap
   ```

   Each crate must include both licences and its README, and exclude local agent tooling. Other workspace members
   (tests, benches, examples and the C ABI crate `tickertape-ffi`) have `publish = false`; the release script refuses
   a member that is neither released nor marked so. Releases do not ship the C library yet (ADR 0028). Before 1.0 the
   C ABI is unstable (ABI version 0) and programs require the exact library version; if it changed, the changelog
   must say so. Bump `TICKERTAPE_HPP_VERSION_*` in `crates/tickertape-ffi/include/tickertape.hpp` with the crate
   version (a test fails otherwise); the C++ binding has the release's version (ADR 0036). Dry-running both packages together requires Cargo 1.90+. The release scripts require Python 3.11+; CI
   selects Python 3.12.

### C ABI at 1.0 (ADR 0028 → Versioning)

For the 1.0 release, and checked again for every later release that ships the C library:

1. In `crates/tickertape-ffi/src/lib.rs`, set `TICKERTAPE_ABI_VERSION` to 1 (only at 1.0; it changes again only
   with a major version that breaks the ABI), and regenerate the header.
2. Freeze the ABI, at 1.0 **and at every later stable release**: copy the release's
   `crates/tickertape-ffi/include/tickertape.h` (the header as shipped) over
   `crates/tickertape-ffi/abi/tickertape-baseline.h`, in the release preparation pull request, after the CI check
   against the previous baseline has passed. From then on CI fails if a later change removes or changes a
   declaration from it; additions pass. Refreshing it at every release matters: a baseline left at 1.0 would let a
   function added in 1.1 be removed in 1.2. Never edit the baseline to make CI pass; a break needs a new major
   version and ABI version.
3. Build the packaged library as packagers will (`cargo cinstall --release -p tickertape-ffi --prefix /usr/local
   --destdir "$STAGE"`, or the release archive once it exists), and check its name in the files:
   - Linux: `readelf -d "$STAGE"/usr/local/lib/libtickertape.so.1* | grep SONAME` prints `libtickertape.so.1`, and
     `libtickertape.so.1` is a link to the real file.
   - macOS: `otool -D "$STAGE"/usr/local/lib/libtickertape.1.dylib` prints an install name ending in
     `libtickertape.1.dylib` (`@rpath/` or the install prefix, never a build directory).
4. Link a program against the installed package, not the build tree, and run it: install into a scratch prefix,
   then build `examples/c/writer.c` and `reader.c` with `pkg-config --cflags --libs tickertape` (and the static
   variant with `pkg-config --static`), with `PKG_CONFIG_PATH` pointing at the scratch prefix, and run
   `examples/c/README.md`'s record-and-replay commands. Check that the program loads the installed library (`ldd` on
   Linux, `otool -L` on macOS).
4. Merge the release preparation pull request and wait for all CI jobs on the resulting `main` commit to pass.
5. Confirm the owner has authorized publishing that exact version, then tag that commit:

   ```sh
   git tag -a vX.Y.Z <main-commit> -m "tickertape vX.Y.Z"
   git push origin vX.Y.Z
   ```

## Automatic release

The tag workflow:

1. Checks stable `vX.Y.Z` syntax, both workspace versions, tag ancestry on `main`, and successful CI on the tag commit.
2. Runs library/CLI tests and builds native binaries for Linux x86-64/ARM64 (glibc and static musl) and macOS
   Intel/Apple Silicon, each on its own architecture, through `cargo auditable` so that each binary carries its
   dependency list. It checks that the musl binaries have no dynamic loader or shared libraries and runs them in an
   empty root directory, then packages each archive and writes its CycloneDX SBOM (`release.py sbom`).
3. Attests build provenance for every archive and SBOM, and the SBOM for every archive, in a job that runs no build
   code.
4. Authenticates through Trusted Publishing, verifies package preparation, then publishes the library and CLI in order.
5. Creates a draft GitHub release from the matching changelog section, uploads six archives, six SBOMs and
   `SHA256SUMS`, then publishes it. Existing release notes are preserved.
6. Verifies published package checksums and source commits, waits up to 15 minutes for both docs.rs builds, installs
   the CLI from crates.io, checks `SHA256SUMS` and the attestations of every released file
   (`gh attestation verify`), runs the Linux glibc and static archives, and installs with `cargo binstall` (if the
   tag has the metadata), checking that it installed the static archive's binary.
7. Writes `Formula/tickertape.rb` from the release's `SHA256SUMS` and pushes it to `thomas0903/homebrew-tap` (only in
   runs that publish, and never replacing a formula for a newer version), then installs it on macOS with
   `brew install thomas0903/tap/tickertape` and runs `brew test`.

How each channel is tested at release:

| Channel | Test in the release workflow |
|---|---|
| crates.io | `cargo install tickertape-cli --locked`, then `--version` and `--help` |
| Archives | `SHA256SUMS`, provenance and SBOM attestations; Linux x86-64 glibc and static archives run |
| Static Linux builds | CLI tests with the musl binary on x86-64 and ARM64, `file`/`readelf`, run in an empty root |
| cargo-binstall | Installs on Linux x86-64; the binary must equal the static archive's |
| Homebrew | `brew install` and `brew test` on macOS Apple Silicon (the Linux formula blocks are not installed) |
| mise | Not tested by the workflow; documented after a manual check |

Monitor the [release workflow](https://github.com/thomas0903/tickertape/actions/workflows/release.yml). Publication is
complete only when its verification job passes. These are release checks, not timing benchmarks.

## Retry or backfill an existing release

From the Actions page, dispatch `release` from `main` with an existing `tag`:

- `publish=false` (default): build binaries and SBOMs, attest them and attach them to an existing GitHub release. No
  crate upload occurs and the Homebrew tap is not touched. Missing files are added and `SHA256SUMS` is replaced
  only by a superset of itself. The run rebuilds every archive, so it can add files only to a release whose existing
  archives it reproduces byte for byte (same compiler and tools); otherwise it stops before uploading anything.
  The v0.4.0 archives were built without `cargo auditable`, so v0.4.0 cannot gain static archives this way.
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

- Add the release's format-v1 compatibility fixtures, written by the tagged version on macOS and Linux
  (`tests/compat/README.md` → "Adding the fixtures for a new release"): run `tests/compat/generate.sh vX.Y.Z` on
  both, add the tag to `RELEASED_TAGS` in `tests/compat.rs`, and merge them in a pull request. Every release
  contributes one set (ADR 0033).
- Update the README and roadmap to reflect publication and test the installation instructions.
- Keep launch announcement drafts local under ignored `docs/launch/`; posting requires an explicit instruction.
- Bump to the next development version only when work on it starts.
