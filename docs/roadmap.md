# Roadmap

Milestones run in order. A milestone is done only when **every** exit criterion holds and `main` is green in CI on
Linux and macOS. Tick a milestone off here and move on in `status.md`.

## v0.1.0

- [x] **M0: Repository and agent setup** (done 2026-10-31; CI green on `main`)
  - `AGENTS.md`/`CLAUDE.md` in every lane; `docs/` (status, roadmap, ADR 0001, playbooks, research);
    `.agents/skills` with the `.claude/skills` symlink and a README with pins and licences; `.claude/settings.json`; `.gitignore`.
  - The workspace builds, and the full check passes with a placeholder test on macOS and on Linux (OrbStack VM).
  - `origin` is set, `main` is pushed and CI is green on both platforms.
- [x] **M1: Specification** (owner approval gate; approved 2026-10-31)
  - `docs/format.md` (byte layout, versioning, alignment, endianness), `docs/crash-contract.md` (guarantees per
    platform and level), and the public API as documented Rust signatures (`Writer`, `Reader`, `Record`, config, errors).
  - ADRs: record format, publication protocol, durability levels, segment rolling, reader API (borrowed vs copy-out).
  - A cross-provider review of the spec, with the findings addressed.
  - The owner approves the spec. M2 does not start before that.
- [ ] **M2: Writer and in-process reader**
  - Append, sequence numbers, checksums, file lock, `checkpoint()`; a reader in the same process reads back what was written.
  - Round-trip property tests and the Miri run on the format code pass. A fuzz target exists.
- [ ] **M3: Cross-process live tailing**
  - An integration test spawns one writer and two reader processes. Both readers observe the identical, gap-free
    sequence of at least 1,000,000 records, including across a segment roll. A late reader replays from 0 and catches up.
- [ ] **M4: Rolling, recovery and crash tests**
  - Size-based rolling with preallocated segments.
  - Tests: torn final record (restart resumes after the last valid record); malformed lengths and checksums rejected;
    a `kill -9` loop of at least 200 iterations at random points, each followed by open-and-verify with no corruption
    accepted; stale lock after a crash; a slow reader lagging the writer; disk-full and roll failures return clear errors.
  - The fuzz target runs for at least 10 minutes with no crash; the command is in `playbooks/run-fuzzing.md`.
  - The `unsafe` module has been reviewed by a second provider and the findings are addressed.
- [ ] **M5: CLI**
  - `tail` (follow live), `inspect` (headers, segment list, sequence range), `verify` (full checksum scan, non-zero
    exit on corruption), `export --format jsonl`. Each command has `assert_cmd` tests and `--help` text.
- [ ] **M6: Parquet export and DuckDB**
  - `export --format parquet` with a documented schema (sequence, schema id, timestamps, payload as binary, plus an
    optional decoder hook for the example schema).
  - A CI test exports a sample journal and runs a DuckDB query on it, checking the row count and a sequence range.
- [ ] **M7: Benchmarks**
  - Append throughput; cross-process writer-to-reader *visible* latency (p50/p99/p99.9, coordinated omission handled);
    `checkpoint()` latency; warm vs cold pages; at least two record sizes.
  - `benchmarks.md` reports measured numbers only, with hardware, OS, kernel/filesystem and the exact command, on the
    owner's Mac and on Linux (OrbStack VM, labelled as such). No comparisons with other tools.
- [ ] **M8: Example application**
  - `examples/`: a producer of synthetic trades and order-book updates (documented schema), a live consumer process, an
    export-then-DuckDB step. The README shows the whole flow in under 10 commands.
- [ ] **M9: Release readiness (v0.1.0)**
  - README (what, why, quickstart, guarantees, non-goals, factual comparison with alternatives, architecture figure).
  - `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`, complete crate metadata.
  - `cargo publish --dry-run` passes for both crates; `cargo deny` is clean; docs build without warnings.
  - No `TODO(verify)` in the README or `docs/`; no secrets, absolute home paths or personal data in the repo.
  - A `v0.1.0` tag is pushed to the private repo. `playbooks/release-checklist.md` has the exact publishing steps.
  - `docs/launch/feedback-post.md` holds a draft feedback request (r/rust, r/algotrading). Agents never post it.

## Later (explicitly out of scope for v0.1)

- Replication to other hosts, and networking of any kind.
- Multiple writers to the same journal.
- Windows support; network filesystems.
- Live zero-copy readers in Python (Python reads exported Parquet, or a documented read-only decoder later).
- Typed zero-copy schemas (rkyv/zerocopy). Payloads are opaque bytes plus a schema id.
- Futex/eventfd wake-ups. Readers poll, with an optional sleep or backoff.
- Any latency claim compared with Chronicle, Aeron or other tools.
- Daily (UTC) segment rolling, unless it fits after M4 without risk.
- Retention policies or automatic segment deletion.
- Publishing to crates.io (the owner asks for this as a separate task after v0.1.0 is reviewed).
