# 0043: The 1.0 compatibility policy

- **Status:** accepted on 2026-11-20 by the maintainers under the owner's delegation, together with the compatibility
  promise of [0033](0033-format-compatibility-fixtures.md); takes effect at the 1.0 release
- **Date:** 2026-11-20

## Context

The roadmap's v1.0 readiness item asks to review and document the supported Rust API, the binding APIs and ABI, the
CLI's machine-readable output, on-disk format compatibility and the upgrade policy, and to resolve known breaking
changes before declaring those surfaces stable. Rules existed per surface (ADR 0028 for the C ABI, 0036 for C++,
0037 for Java, 0033 for the format, `cli.md` for `status --json`), but not for the Rust crate, the Python bindings,
the other JSON outputs or the crash contract, and nowhere in one place.

## Options considered

1. **One major version for every surface, released together (chosen).** The crates, the C library, the C++ header and
   the bindings already share the workspace version and one release tag. A break in any stable surface is 2.0.
2. **Independent versions per surface.** Lets a binding break without a core major release, but every combination
   would need its own compatibility statement and tests, and the bindings' library check already ties them to the C
   ABI's version.

## Decision

- From 1.0, the surfaces in `docs/compatibility.md` follow SemVer together: a 1.x release only adds (public items,
  variants of `#[non_exhaustive]` enums and fields of `#[non_exhaustive]` structs, C functions, constants, codes and
  fields at the end of extensible structs, binding methods, CLI commands and options, JSON fields). Removing,
  renaming, changing a documented meaning or weakening a crash-contract guarantee waits for 2.0. Removals are
  deprecated first and kept until the next major version.
- **Covered:** the `tickertape` crate's public items, except the `fault-injection` feature, `cfg(fuzzing)`/`cfg(loom)`
  items and `#[doc(hidden)]` items; the C ABI (ADR 0028); the C++ header (ADR 0036, source compatibility); the
  Python bindings' `__all__` and the Java binding's exported public types; the CLI's documented JSON, JSONL,
  Prometheus and Parquet outputs and its exit statuses; format version 1 as ADR 0033 tests it; the crash contract.
- **Not covered:** human-readable text, messages and `reason` strings (in Rust, the C ABI, the bindings and the
  CLI), `Debug` output, the library half of `tickertape-cli`, the read-only Python decoder's own API, and the
  iceoryx2 integration (own version).
- **JSON:** consumers ignore unknown fields; no field is removed or changes meaning in 1.x; `status --json` keeps
  `schema_version` 1 in 1.x.
- **Format:** every 1.x writer writes format version 1, and every 1.x release reads and continues every version-1
  journal. Writer downgrade is not promised (as ADR 0033 says).
- **Minimum toolchains** (Rust 1.85, CPython 3.11, JDK 25, C++17) may rise in a minor release with a changelog
  entry, never in a patch release.
- `docs/compatibility.md` holds the per-surface statement and the list of what blocks declaring 1.0 ("Before 1.0").

## Consequences

- Changes to a covered surface need a changelog entry that says whether they are additive; after 1.0 a breaking one
  needs a major release.
- The "Before 1.0" list in `docs/compatibility.md` must be empty, or each remaining item explicitly accepted, before
  the 1.0 release; the roadmap's v1.0 readiness item points to it.
- Fixed in the pull request that added this record: `format::LaterHistory` (added since v0.4.0) is
  `#[non_exhaustive]`; `c-api.md` lists all six extensible structs; `python.md` states the bindings' stability;
  ADR 0034's status says accepted.
- Resolved when this record was accepted (2026-11-20): ADRs 0029 (PyPI project name `tickertape`), 0032, 0033, 0037
  (Maven coordinates `io.github.thomas0903:tickertape`) and 0038 are accepted; the zero-copy reader's known
  limitation ships in 1.0 as a documented limitation (`docs/compatibility.md` → Crash contract); the `format`
  module's scanning and recovery helpers are crate-private or `#[doc(hidden)]`, and the struct-like variants of
  `Error`, `CorruptReason` and `RecoveryAction` are `#[non_exhaustive]`; `tickertape_head_end`,
  `tickertape_beacon_state` and `tickertape_readers_dir_state` have an `_OTHER` value (255); the bindings use the
  Rust names in their own case convention (the renames are in the changelog); every CLI `--json` output carries
  `schema_version` 1. What remains is in `docs/compatibility.md` → Before 1.0.
