# 0031: Language bindings and install channels for v1.0

- **Status:** accepted (owner decision of 2026-11-19 to add bindings and install channels; the selection is the
  maintainers')
- **Date:** 2026-11-19

## Context

The roadmap already requires a C ABI (ADR 0028) and installable Python bindings (ADR 0029). On 2026-11-19 the owner
added two things to the v1.0 scope: bindings for more languages, and more ways to install. Windows stays out of
scope. Each binding and each channel costs work at every release, and several need an account, a namespace or a
signing key that only the owner can create. Registry facts below were checked on 2026-11-19.

## Options considered

**Bindings.**

1. **C++**, as a header-only wrapper over the C ABI (move-only handles, views, RAII). Almost free: no extra
   binaries, tested in the same job as the C ABI.
2. **Java**, on the Foreign Function & Memory API (final since JDK 22) over the C ABI, with no JNI code. The tools
   tickertape is measured against (Chronicle Queue, Aeron) are JVM tools, so this is the audience most likely to
   try it. Cost: Maven Central needs a verified namespace, a GPG key and a token; we found no keyless publishing for it.
3. **Go** (cgo over the C ABI). Go modules have no good way to ship a native library: either prebuilt archives
   are committed to the module at every release, or users build it themselves.
4. **Node.js** (napi-rs) and **C#/.NET** (P/Invoke). Each needs one package per platform and its own registry
   set-up. Their users rarely need a local, latency-sensitive journal.

**Install channels.** Static musl Linux builds of the CLI, cargo-binstall metadata, a Homebrew tap, PyPI wheels,
a C library archive, Maven Central, distribution packages (deb, rpm, AUR, nixpkgs), conda-forge, a container
image, vcpkg and Conan, and tool-version managers (mise, aqua).

## Decision

**Bindings for v1.0, in this order: C, Python, C++ header, Java. The cut-off is after Java.** Go, Node.js and .NET
wait for demand after v1.0; the C ABI is designed so they can be added without changing it.

A binding counts as delivered when it has: the full record and replay surface (open, append, checkpoint, seek,
next with timeout, durable-only) with errors mapped to the language's own; documented ownership and lifetimes; the
shared cross-language fixture tests; a runnable example; an install test from the built package on every
supported platform; and a version rule tied to the C ABI's version.

**Install channels for v1.0:**

- **Static musl builds of the CLI** for Linux x86-64 and aarch64, next to the glibc builds. The glibc archives are
  built on Ubuntu 22.04 and need glibc 2.35, so they do not run on RHEL 9 or Amazon Linux 2023 (glibc 2.34).
- **cargo-binstall** metadata in the CLI crate, checked by the release workflow.
- **A Homebrew tap** with a formula that installs the release archives, updated by the release workflow.
  homebrew-core is not possible yet: it requires a level of popularity the project does not have.
- **PyPI wheels**: the Python bindings, and the CLI as a wheel so `pipx` and `uvx` can install it.
- **A C library archive** per platform (shared and static library, header, pkg-config file), which the C++ and
  Java bindings and packagers build on.
- **Maven Central** for the Java binding.
- **Documentation only** for mise, which can already install from the GitHub release archives.

**Supply chain, across all channels:** build provenance attestations with a software bill of materials for every
released file, dependency information embedded in the CLI binary, and deterministic archive names.

**Not for v1.0:** deb and rpm packages, the aqua registry, conda-forge, vcpkg and Conan (after 1.0, once the
surfaces are stable); AUR and nixpkgs (left to their communities); a container image (a local shared-memory tool
gains little from one); Windows channels.

## Consequences

- The owner must set up, once: PyPI trusted publishers for both wheels; a tap repository and a token that can
  only push to it; a Maven Central namespace, GPG key and token; and the first publish of each new crate. Each is
  a release-time step, so nothing here publishes before the owner approves it.
- Every release tests each channel by installing from it. A channel that cannot be tested this way is not
  announced.
- The roadmap gains an "install channels" item and names the bindings. The release checklist grows; the release
  workflow must stay tag-driven and must not need long-lived secrets where keyless publishing exists.
- Musl builds use a different memory allocator. Parquet export speed under musl must be measured before the musl
  build is recommended for it.

## Amendment (2026-11-19): version of the Python bindings

The Python bindings (ADR 0029) bind the Rust library directly, not the C ABI, so the version rule above does not fit
them. Their version follows the Rust workspace version, and `release.py prepare` refuses a release tag whose
bindings version differs. The other criteria for a delivered binding apply unchanged. Bindings built on the C ABI
(C++, Java) keep the rule above.
