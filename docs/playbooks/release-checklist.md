# Playbook: release checklist

Publishing a new version is a maintainer decision. Preparing a change does not authorize a release.
After the owner authorizes that version, push its tag to start `.github/workflows/release.yml` (crates, CLI archives)
and `.github/workflows/python-wheels.yml` (Python wheels and sdist).

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

### PyPI (Python bindings, ADR 0029)

Until these steps are done, `python-wheels.yml` builds and smoke-tests the wheels and sdist and uploads them as
workflow artefacts, but its `publish` job is skipped. Nothing reaches PyPI without steps 1–3; steps 4 and 5 protect
the publishing path and must be done before step 3.

1. On [pypi.org](https://pypi.org/manage/account/publishing/), signed in as the maintainer (with 2FA), add a
   **pending publisher** (it creates the project on the first upload, so no token is ever needed):

   | Setting | Value |
   |---|---|
   | PyPI project name | `tickertape` (the `[project].name` in `bindings/python/pyproject.toml`) |
   | Owner | `thomas0903` |
   | Repository name | `tickertape` |
   | Workflow name | `python-wheels.yml` |
   | Environment name | `pypi` |

   A pending publisher does not reserve the name; until the first upload, anyone can register it. To use another
   name, change `[project].name` in `bindings/python/pyproject.toml` (the import name stays `tickertape`) and the
   install instructions in `bindings/python/README.md` and `docs/python.md`.
2. In the GitHub repository settings, create the environment `pypi`, allowing the `main` branch and tags matching `v*`
   (as for `release`).
3. Add the repository variable `PYPI_PUBLISHING` with the value `enabled` (Settings → Secrets and variables →
   Actions → Variables). Delete it to switch publishing off again.
4. Protect release tags: add a tag ruleset (Settings → Rules → Rulesets → New tag ruleset) targeting `v*` that
   restricts creation, update and deletion to the maintainer (bypass list: repository admin only) and blocks force
   pushes. A tag-triggered run takes its workflow files from the tagged commit, not from `main`: anyone who could
   push a `v*` tag could point it at a commit with an edited `python-wheels.yml` or `release.yml` and publish from
   it. The ruleset makes tags as protected as `main`.
5. Require a reviewer for the `pypi` environment (Settings → Environments → `pypi` → Required reviewers: the
   maintainer; also tick "Prevent self-review" if another maintainer exists). Each publish job then waits for an
   explicit approval in the Actions UI, a last check that the run, its tag and its workflow file are the expected
   ones. `release.py prepare` resolves only `refs/tags/<tag>` and the workflows check out the fully qualified tag, so
   a branch named like a tag can never pass the gate.

See PyPI's [Trusted Publishing documentation](https://docs.pypi.org/trusted-publishers/).

`main` requires pull requests and all CI checks, including release tooling. No approving review is required for this
single-maintainer repository. Administrators are subject to the checks; force pushes and branch deletion are disabled.

## Prepare

1. Set `[workspace.package].version` and `[workspace.dependencies].tickertape.version` in `Cargo.toml`, and
   `[package].version` in `bindings/python/Cargo.toml`; then refresh the bindings' lockfile with
   `cargo update --workspace --manifest-path bindings/python/Cargo.toml`. `release.py prepare` refuses a tag whose
   versions or bindings lockfile disagree. Set the C header's version macros to match (`TICKERTAPE_VERSION_MAJOR`,
   `_MINOR`, `_PATCH` in `crates/tickertape-ffi/src/lib.rs`; the build fails until they do) and regenerate the
   header with cbindgen 0.29.4 (the command is in `crates/tickertape-ffi/cbindgen.toml`).
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
2. Runs library/CLI tests and builds native Linux x86-64/ARM64 and macOS Intel/Apple Silicon binaries.
3. Authenticates through Trusted Publishing, verifies package preparation, then publishes the library and CLI in order.
4. Creates a draft GitHub release from the matching changelog section, uploads four archives plus `SHA256SUMS`,
   then publishes it. Existing release notes are preserved.
5. Verifies published package checksums and source commits, waits up to 15 minutes for both docs.rs builds, installs
   the CLI from crates.io, and downloads/checks/runs the Linux binary archive.

The same tag starts `python-wheels.yml`, which runs the same `prepare` checks, builds manylinux and musllinux
(x86-64, aarch64) and macOS (x86-64, arm64) wheels and an sdist, runs the end-to-end example with each, and, once
PyPI is configured (above), uploads them with Trusted Publishing. Files already on PyPI are skipped, so a rerun
completes a partial upload; PyPI files are immutable. To build wheels without publishing, dispatch it with any `ref`
and `publish=false`. To publish an existing tag later, dispatch it from `main` with `ref=vX.Y.Z` and `publish=true`.

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

- Add the release's format-v1 compatibility fixtures, written by the tagged version on macOS and Linux
  (`tests/compat/README.md` → "Adding the fixtures for a new release"): run `tests/compat/generate.sh vX.Y.Z` on
  both, add the tag to `RELEASED_TAGS` in `tests/compat.rs`, and merge them in a pull request. Every release
  contributes one set (ADR 0033).
- Update the README and roadmap to reflect publication and test the installation instructions.
- Keep launch announcement drafts local under ignored `docs/launch/`; posting requires an explicit instruction.
- Bump to the next development version only when work on it starts.
