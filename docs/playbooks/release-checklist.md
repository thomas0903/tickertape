# Playbook: release checklist

Publishing a new version is a maintainer decision. Preparing a change does not authorize a release.
After the owner authorizes that version, push its tag to start `.github/workflows/release.yml` (crates, CLI archives,
C library archives, the Java binding on Maven Central) and `.github/workflows/python-wheels.yml` (the Python
bindings' wheels and sdist, and the CLI wheels).

## One-time configuration

The GitHub `release` environment allows the `main` branch and tags matching `v*`. Like the three other publishing
environments (`pypi`, `pypi-cli`, `maven-central`) it has the maintainer as required reviewer: the explicit version
tag (or manual workflow dispatch with `publish=true`) starts a release, and each publishing job waits for approval.

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

### PyPI: the CLI wheel (`tickertape-cli`, ADR 0031)

Until these steps are done, `python-wheels.yml` builds and tests the CLI wheels and keeps them as workflow
artefacts, but its `publish-cli` and `verify-cli` jobs are skipped. They mirror the bindings' steps above, with their
own project, environment and switch, so either package can be published without the other:

1. On [pypi.org](https://pypi.org/manage/account/publishing/), add a second **pending publisher**. These values were
   promised to the owner; do not change them:

   | Setting | Value |
   |---|---|
   | PyPI project name | `tickertape-cli` (the `[project].name` in `crates/tickertape-cli/pyproject.toml`) |
   | Owner | `thomas0903` |
   | Repository name | `tickertape` |
   | Workflow name | `python-wheels.yml` |
   | Environment name | `pypi-cli` |

2. Create the GitHub environment `pypi-cli`, allowing the `main` branch and tags matching `v*`, with the maintainer
   as required reviewer (as for `pypi`, step 5).
3. Add the repository variable `PYPI_CLI_PUBLISHING` with the value `enabled`. Delete it to switch publishing off.

The tag ruleset of step 4 above protects this path too.

### Maven Central (Java binding, ADR 0031 and 0037)

Until these steps are done, the release workflow builds, tests and attests the Java binding's Maven files (main,
sources and Javadoc JARs, four natives JARs, the pom) and keeps them as workflow artefacts, but its `maven-central`
job is skipped (without the variable), or skips its steps with a warning (without the secrets), and
`verify-maven-central` does not run.

1. Sign in to the [Central Portal](https://central.sonatype.com/) with the GitHub account `thomas0903` (with 2FA).
   Signing in with GitHub verifies the namespace `io.github.thomas0903`; check that it is listed as verified under
   Namespaces. Central refuses uploads for an unverified namespace.
2. Create a GPG signing key for the project, protected by a passphrase, and publish its public key to
   `keyserver.ubuntu.com` (`gpg --keyserver keyserver.ubuntu.com --send-keys <key id>`); Central checks signatures
   against public key servers. Keep a backup of the key outside GitHub.
3. In the Central Portal, generate a user token (View Account → Generate User Token). It has a username and a
   password part.
4. Create the GitHub environment `maven-central`, allowing tags matching `v*` only, with the maintainer as required
   reviewer. Add four environment secrets (environment, not repository, secrets, so that only this job can read them):

   | Secret | Value |
   |---|---|
   | `MAVEN_CENTRAL_USERNAME` | the token's username part |
   | `MAVEN_CENTRAL_PASSWORD` | the token's password part |
   | `MAVEN_GPG_PRIVATE_KEY` | `gpg --armor --export-secret-keys <key id>` |
   | `MAVEN_GPG_PASSPHRASE` | the key's passphrase |

5. Add the repository variable `MAVEN_CENTRAL_PUBLISHING` with the value `enabled`. Delete it to switch publishing
   off.

The job runs no build code: it signs the files the `java` job built and `attest-java` attested, with gpg in a
temporary keyring, and uploads them with `release.py maven-upload` for automatic publication. Central files are
immutable and cannot be deleted; a version already on Central is left alone. Renew the token before it expires
(`maven-central` then fails after everything else is published; rerun it with a new token).

The Homebrew tap is the public repository `thomas0903/homebrew-tap`. The `release` environment holds the secret
`HOMEBREW_TAP_TOKEN`: a fine-grained token for that repository only, with contents read and write. Only the step
that pushes `Formula/tickertape.rb` sees it, and only in runs that publish. Renew it before it expires; an expired
token fails the `homebrew` job after everything else is published (rerun that job with a new token).

Build provenance and SBOM attestations (archives, wheels, JARs) need no setting: artifact attestations are available
to public repositories.
They are signed through the public Sigstore instance, so each attestation is also recorded in its public
transparency log.

`main` requires pull requests and all CI checks, including release tooling. No approving review is required for this
single-maintainer repository. Administrators are subject to the checks; force pushes and branch deletion are disabled.

## Prepare

1. Set `[workspace.package].version` and `[workspace.dependencies].tickertape.version` in `Cargo.toml`,
   `[package].version` in `bindings/python/Cargo.toml`, `<version>` in `bindings/java/pom.xml` and both `VERSION`
   and `VERSION_NUMBER` in the Java binding's `NativeLibrary.java` (its tests compare them with the header); then refresh the
   bindings' lockfile with `cargo update --workspace --manifest-path bindings/python/Cargo.toml`. `release.py prepare`
   refuses a tag whose versions or bindings lockfile disagree. The CLI wheel takes its version from the workspace.
   Set the C header's version macros to match (`TICKERTAPE_VERSION_MAJOR`, `_MINOR`, `_PATCH` in
   `crates/tickertape-ffi/src/lib.rs`; the build fails until they do) and regenerate the header with cbindgen 0.29.4 (the command is in `crates/tickertape-ffi/cbindgen.toml`).
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
   a member that is neither released nor marked so. The C library ships as archives in the GitHub release, not as a
   crate (ADR 0028, 0031). From 1.0 the
   C ABI is frozen (ABI version 1): the baseline check must pass, and the changelog lists what was added. Bump `TICKERTAPE_HPP_VERSION_*` in `crates/tickertape-ffi/include/tickertape.hpp` with the crate
   version (a test fails otherwise); the C++ binding has the release's version (ADR 0036). Dry-running both packages together requires Cargo 1.90+. The release scripts require Python 3.11+; CI
   selects Python 3.12.

### Append path against the previous release (minor and major releases)

Issue #82: a change to how a reader checks the marker made appends slower when a durable-only reader blocks, and no
check showed it. The micro-benchmark and `tt-latency` with one reader do not exercise that path. Before a minor or major
release, on one quiet cloud VM with local SSD (`docs/playbooks/run-benchmark.md`, machine rules), build three binaries
of `tt-sustained` that differ only in the library:

1. **Preferred:** the previous tag's workspace, built as it is (the baseline), a second copy of that binary (the noise
   floor), and the previous tag's workspace with the candidate's `crates/tickertape` copied over it. The copied crate
   inherits `version`, `rust-version` and the lints from the old workspace root, so every binary reports the old
   version: label the runs by binary, never by what they report.
2. **If that does not build** (a new dependency of the library, a raised MSRV, `--locked` refusing the lockfile): the
   candidate's workspace for all three, with the previous tag's `crates/tickertape` copied over it for the baseline and
   its copy. This is how issue #82 was measured (the 1.0.0 library under the harness of `0fcdb9e5`).

Run them interleaved, at least 10 rounds in rotating order, each run with a new, empty `--dir` and `--out` (the tool
refuses non-empty ones), deleting the journal after each run:

```sh
tt-sustained run --dir <ssd>/tt-<build>-<round> --out <out>/<build>-<round> --json --payload 1024 --duration 15s \
  --warmup 3s --cpus 1,2,3,4,5,6 --rate 100000 --checkpoint-every 1s --checkpoint-mode background \
  --retain-bytes 68719476736 --readers mapped,safe:wait=default,safe:durable:wait=block
```

Compare, as medians of the rounds with the range: the writer's `append_latency` p50, p99, p99.9 and maximum; each
reader's CPU fraction and voluntary context switches (`readers[i].cpu`); and the writer's `hot_thread_cpu.system_cpu_s`.
A difference larger than between the two copies of the previous release, in the same direction in most rounds, needs
an explanation before the release: a fix, or an entry in the changelog that says what got slower and why. The context
switches move by orders of magnitude when a reader starts or stops sleeping, so they show such a change even where the
latency difference is small. Expected once, for the first release after 1.0.0 (ADR 0053): the durable-only reader's
context switches and CPU fall by orders of magnitude, because it no longer wakes for every append. Keep the raw
output with the other release notes; record numbers in `docs/benchmarks.md` only as the playbook says.

### Replay against the previous release (every release)

Issue #86 (ADR 0057): replay speed changed with the layout of `Reader`, and CI measures no timings. Before every
release, on one quiet cloud VM of each architecture (x86-64 and aarch64, local SSD; `docs/playbooks/run-benchmark.md`,
machine rules), build three `replay` benchmark binaries (`cargo bench -p tickertape-bench --bench replay --no-run`;
the binary is `target/release/deps/replay-*`), from the same workspaces as for the append path above: the previous
tag's workspace (the baseline), a second copy of that binary (the noise floor), and the previous tag's workspace with
the candidate's `crates/tickertape` copied over it (or the fallback described there). Run them interleaved, at least 10
rounds in rotating order, each in a new process, with `TMPDIR` on the SSD:

```sh
TMPDIR=<ssd>/tmp <replay binary> --bench 'replay/records/' --warm-up-time 1 --measurement-time 2 --noplot
```

Compare, per case (`safe` and `mapped`, 64 and 1,024 B), the median of the per-round ratios against the baseline,
beside the same ratio for the second copy. A difference larger than the second copy's, in the same direction in most
rounds, needs an explanation before the release: a fix, or a changelog entry that says what got slower and why. A
reader on the stack lands at a different address in each process on Linux, so look at the spread too: a build that is
slower in a few rounds by 15 % or more, and normal in the others, may again have a hot access that straddles a page
(ADR 0057). The test `reader::tests::reader_layout` must pass unchanged, or the ADR must say why the layout changed.
Keep the raw output with the other release notes; record numbers in `docs/benchmarks.md` only as the playbook says.

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
   --destdir "$STAGE"`), and check its name in the files. The release archive's soname and install name are checked
   by `crates/tickertape-ffi/scripts/install-test.sh` in CI and in the release workflow; check them once by hand in
   an archive built with `release.py c-build` and `c-package` too:
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

1. Checks stable `vX.Y.Z` syntax, both workspace versions and those of the Python and Java bindings, tag ancestry on
   `main`, and successful CI on the tag commit.
2. Runs library/CLI tests and builds native binaries for Linux x86-64/ARM64 (glibc and static musl) and macOS
   Intel/Apple Silicon, each on its own architecture, through `cargo auditable` so that each binary carries its
   dependency list. It checks that the musl binaries have no dynamic loader or shared libraries and runs them in an
   empty root directory, then packages each archive and writes its CycloneDX SBOM (`release.py sbom`).
3. Attests build provenance for every archive and SBOM, and the SBOM for every archive, in a job that runs no build
   code.
4. Builds the C library archive on Linux x86-64/ARM64 (glibc) and macOS Intel/Apple Silicon, after the C ABI tests
   (`release.py c-build`, `c-package`), runs the archive's install test (C and C++ examples through pkg-config,
   shared and static) and writes its SBOM; attests both. Builds the Java binding's Maven files from the C archives'
   libraries, unsigned, runs its install test with the Linux x86-64 JARs and attests every file.
5. Only when all of the above passed: authenticates through Trusted Publishing, verifies package preparation, then
   publishes the library and CLI in order.
6. Creates a draft GitHub release from the matching changelog section, uploads six CLI archives, four C library
   archives, their ten SBOMs and `SHA256SUMS`, then publishes it. Existing release notes are preserved.
7. Verifies published package checksums and source commits, waits up to 15 minutes for both docs.rs builds, installs
   the CLI from crates.io, checks `SHA256SUMS` and the attestations of every released file
   (`gh attestation verify`), runs the Linux glibc and static archives, runs the install test on the downloaded Linux
   x86-64 C archive, and installs with `cargo binstall` (if the tag has the metadata), checking that it installed
   the static archive's binary.
8. Writes `Formula/tickertape.rb` from the release's `SHA256SUMS` and pushes it to `thomas0903/homebrew-tap` (only in
   runs that publish, and never replacing a formula for a newer version), then installs it on macOS with
   `brew install thomas0903/tap/tickertape` and runs `brew test`.
9. Once Maven Central is configured (above), signs the Maven files and uploads them through the Central Portal,
   waits until Central reports them published, then downloads every file from Maven Central (waiting up to an hour
   for it to appear), compares it with the attested build and runs the Java install test with the downloaded JARs.

How each channel is tested at release:

| Channel | Test in the release workflow |
|---|---|
| crates.io | `cargo install tickertape-cli --locked`, then `--version` and `--help` |
| Archives | `SHA256SUMS`, provenance and SBOM attestations; Linux x86-64 glibc and static archives run |
| Static Linux builds | CLI tests with the musl binary on x86-64 and ARM64, `file`/`readelf`, run in an empty root |
| cargo-binstall | Installs on Linux x86-64; the binary must equal the static archive's |
| Homebrew | `brew install` and `brew test` on macOS Apple Silicon (the Linux formula blocks are not installed) |
| C library archives | Install test on each platform before the release; again on the published Linux x86-64 archive |
| Maven Central | Every file downloaded from Central equals the attested build; install test with the downloaded JARs |
| PyPI, CLI wheel | `python-wheels.yml`: `pipx install tickertape-cli==X.Y.Z`, then `--version` and `--help` |
| PyPI, Python bindings | Not tested from PyPI by the workflow; test by hand (below) |
| mise | Not tested by the workflow; documented after a manual check |

Test the channels the workflows do not test from the published files by hand after every release, from a clean
environment, and do not announce a channel that fails (ADR 0031):

```sh
python3 -m venv /tmp/tt-py && /tmp/tt-py/bin/pip install tickertape==X.Y.Z && \
  /tmp/tt-py/bin/python bindings/python/examples/end_to_end.py /tmp/tt-py-journal
uvx --from tickertape-cli==X.Y.Z tickertape --version        # the CLI wheel through uv
gh release download vX.Y.Z --pattern 'tickertape-c-*-aarch64-apple-darwin.tar.gz' --dir /tmp/tt-c
crates/tickertape-ffi/scripts/install-test.sh /tmp/tt-c/tickertape-c-vX.Y.Z-aarch64-apple-darwin.tar.gz /tmp/tt-c/work
```

For Maven Central, also resolve the dependency from a fresh Maven project (`mvn dependency:get
-Dartifact=io.github.thomas0903:tickertape:X.Y.Z:jar:natives-macos-aarch64`) and check that the artefact page on
[central.sonatype.com](https://central.sonatype.com/) shows the signatures and the sources and Javadoc JARs.

The same tag starts `python-wheels.yml`, which runs the same `prepare` checks, builds manylinux and musllinux
(x86-64, aarch64) and macOS (x86-64, arm64) wheels and an sdist, runs the end-to-end example with each, builds the
CLI wheels for the same platforms and runs `--version`, `verify` and `tail` with each, attests every wheel and the
sdist with its SBOM (`attest`), and, once PyPI is configured (above), uploads the bindings (`publish`) and the CLI
(`publish-cli`, then `verify-cli`) with Trusted Publishing. Files already on PyPI are skipped, so a rerun
completes a partial upload; PyPI files are immutable. To build wheels without publishing, dispatch it with any `ref`
and `publish=false`. To publish an existing tag later, dispatch it from `main` with `ref=vX.Y.Z` and `publish=true`.

Monitor the [release workflow](https://github.com/thomas0903/tickertape/actions/workflows/release.yml). Publication is
complete only when its verification job passes. These are release checks, not timing benchmarks.

## Retry or backfill an existing release

From the Actions page, dispatch `release` from `main` with an existing `tag`:

- `publish=false` (default): build binaries and SBOMs, attest them and attach them to an existing GitHub release. No
  crate upload occurs, and neither the Homebrew tap nor Maven Central is touched. Tags from before the C library
  archive (v0.4.0 and older) build no C archives or Maven files. Missing files are added and `SHA256SUMS` is replaced
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

## What the v0.5.1 release found

v0.5.1 (2026-11-23) was the first release through every channel and the release-candidate exercise for 1.0. The
`v0.5.0` tag before it was pushed but never published: its runs stopped at the first two points, before the
publishing jobs. All four defects were in steps that had only ever run for a release tag:

- The Linux C library jobs build on Ubuntu 22.04, whose compilers (GCC 11.4, Clang 14) CI did not use:
  `tickertape.hpp` gave a warning with GCC 11, and the tests passed `-std=c++23` to a compiler that only knows
  `-std=c++2b`. CI's `c abi` job now also runs on that image.
- Every SBOM attestation failed: the attestation action requires a `serialNumber` that cargo-cyclonedx does not
  write under `SOURCE_DATE_EPOCH`. `release.py sbom` adds one.
- `github-release` created its draft and then looked it up by tag, which never finds a draft. Rerunning the failed
  job completed the release (the listing found the draft); `release.py` now uses the listing.
- In preparation, the Java binding's `NativeLibrary.VERSION_NUMBER` was missed, and the library then refused to
  load (step 1 above names it now).

A tag whose run fails before `publish` has published nothing (`publish` needs every build and attestation job). Fix
the defect in a pull request and release the next patch version; leave the failed tag in place. After `publish`,
resume with "Rerun failed jobs" or the dispatch below; never tag again for the same version.

## What the v1.0.0 release found

Two steps failed after the publication they follow; nothing published was wrong:

- `github-release`, again: the listing that v0.5.1's fix introduced did not yet show the draft when it was asked
  immediately after the creation, so the job failed after `publish`. "Rerun failed jobs" completed the release.
  `release.py` now asks the listing for up to a minute.
- `verify-cli` saw only the previous version of `tickertape-cli` on PyPI for its five minutes of retries, although
  the upload had succeeded (cause not confirmed). "Rerun failed jobs" then skipped the job instead of running it,
  so the check was made by hand: `uvx --from tickertape-cli==1.0.0 tickertape --version` on macOS and Linux. The
  job now installs without pip's cache and retries for ten minutes. If it is skipped again, check by hand.

## After publishing

- Add the release's format-v1 compatibility fixtures, written by the tagged version on macOS and Linux
  (`tests/compat/README.md` → "Adding the fixtures for a new release"): run `tests/compat/generate.sh vX.Y.Z` on
  both, add the tag to `RELEASED_TAGS` in `tests/compat.rs`, and merge them in a pull request. Every release
  contributes one set (ADR 0033).
- Update the README and roadmap to reflect publication and test the installation instructions.
- Keep launch announcement drafts local under ignored `docs/launch/`; posting requires an explicit instruction.
- Bump to the next development version only when work on it starts.
