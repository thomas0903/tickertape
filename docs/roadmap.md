# Roadmap

Milestones run in order. A milestone is done only when **every** exit criterion holds and `main` is green in CI on
Linux and macOS.

## v0.1.0

- [x] **M0: Repository setup** (done 2026-10-31; CI green on `main`)
  - Cargo workspace, licences, `docs/` (roadmap, ADR 0001, playbooks, research), CI and `.gitignore`.
  - The workspace builds, and the full check passes with a placeholder test on macOS and on Linux (OrbStack VM).
  - `origin` is set, `main` is pushed and CI is green on both platforms.
- [x] **M1: Specification** (owner approval gate; approved 2026-10-31)
  - `docs/format.md` (byte layout, versioning, alignment, endianness), `docs/crash-contract.md` (guarantees per
    platform and level), and the public API as documented Rust signatures (`Writer`, `Reader`, `Record`, config, errors).
  - ADRs: record format, publication protocol, durability levels, segment rolling, reader API (borrowed vs copy-out).
  - An independent review of the spec, with the findings addressed.
  - The owner approves the spec. M2 does not start before that.
- [x] **M2: Writer and in-process reader** (done 2026-10-31)
  - Append, sequence numbers, checksums, file lock, `checkpoint()`; a reader in the same process reads back what was written.
  - Round-trip property tests and the Miri run on the format code pass. A fuzz target exists.
- [x] **M3: Cross-process live tailing** (done 2026-10-31)
  - An integration test spawns one writer and two reader processes. Both readers observe the identical, gap-free
    sequence of at least 1,000,000 records, including across a segment roll. A late reader replays from 0 and catches up.
- [x] **M4: Rolling, recovery and crash tests** (done 2026-10-31)
  - Size-based rolling with preallocated segments.
  - Tests: torn final record (restart resumes after the last valid record); malformed lengths and checksums rejected;
    a `kill -9` loop of at least 200 iterations at random points, each followed by open-and-verify with no corruption
    accepted; stale lock after a crash; a slow reader lagging the writer; disk-full and roll failures return clear errors.
  - The fuzz target runs for at least 10 minutes with no crash; the command is in `playbooks/run-fuzzing.md`.
  - The `unsafe` module has been reviewed by a second provider and the findings are addressed.
- [x] **M5: CLI** (done 2026-10-31)
  - `tail` (follow live), `inspect` (headers, segment list, sequence range), `verify` (full checksum scan, non-zero
    exit on corruption), `export --format jsonl`. Each command has `assert_cmd` tests and `--help` text.
- [x] **M6: Parquet export and DuckDB** (done 2026-10-31)
  - `export --format parquet` with a documented schema (sequence, schema id, timestamps, payload as binary, plus an
    optional decoder hook for the example schema).
  - A CI test exports a sample journal and runs a DuckDB query on it, checking the row count and a sequence range.
- [x] **M7: Benchmarks** (done 2026-10-31)
  - Append throughput; cross-process writer-to-reader *visible* latency (p50/p99/p99.9, coordinated omission handled);
    `checkpoint()` latency; warm vs cold pages; at least two record sizes.
  - `benchmarks.md` reports measured numbers only, with hardware, OS, kernel/filesystem and the exact command, on the
    owner's Mac and on Linux (OrbStack VM, labelled as such). No comparisons with other tools.
- [x] **M8: Example application** (done 2026-10-31)
  - `examples/`: a producer of synthetic trades and order-book updates (documented schema), a live consumer process, an
    export-then-DuckDB step. The README shows the whole flow in under 10 commands.
- [x] **M9: Release readiness (v0.1.0)** (done 2026-10-31)
  - README (what, why, quickstart, guarantees, non-goals, factual comparison with alternatives, architecture figure).
  - `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`, complete crate metadata.
  - `cargo publish --dry-run` passes for both crates; `cargo deny` is clean; docs build without warnings.
  - No `TODO(verify)` in the README or `docs/`; no secrets, absolute home paths or personal data in the repo.
  - A `v0.1.0` tag is pushed to the private repo. `playbooks/release-checklist.md` has the exact publishing steps.
  - A draft feedback request for r/rust and r/algotrading exists (kept outside the repository).

## v0.2 (included in the first public release, v0.4.0)

Improvements after v0.1.0. Same rule: done only when every criterion holds and CI is green.

- [x] **Reader hot path without per-record allocation** (done 2026-10-31)
  - Neither reader allocates per record; the zero-copy reader still validates every record before any slice into the
    mapping exists. Independent review of the `mmap.rs` change (approved; one doc nit fixed).
  - Replay benchmarks re-run on the owner's Mac and the OrbStack VM, recorded in `docs/benchmarks.md`.
- [x] **Durable-only reads** (ADR 0010; done 2026-10-31)
  - `ReaderConfig::durable_only`: readers return only records covered by the checkpoint marker. `tail` and `export`
    get `--durable-only`. Crash contract entry with its test.
- [x] **Optional live feed recorder** (the M8 stretch goal; ADR 0011; done 2026-10-31)
  - An example that records a public exchange trade feed (Binance, no API key) into a journal with the market-demo
    schema. Not built by default; CI only builds it and never touches the network.

## v0.3

Owner decisions of 2026-11-01: build the former "Later" items below, benchmark against other tools, push performance.

- [x] **Retention and archiving** (ADR 0016): `retention` module, writer policy, `tickertape archive`, crash contract
  C14
- [x] **Reader wake-ups** (ADRs 0017, 0021): shared memory that can't shrink, futex / `os_sync_wait_on_address`
- [x] **Comparison suite** (ADR 0018): Chronicle Queue, Aeron, yaque, queue-file and plain files; recorded on Linux
  (AWS) and macOS
- [x] **Performance work** (ADRs 0019, 0020, 0022, 0023): hardware CRC32C; on macOS, background prefaulting and a
  prepared next segment; on Linux, neither, as measured
- [x] Tag v0.3.0 (included in the first public release, v0.4.0)

## v0.4

- [x] Three-lane hardware CRC32C for every record length (ADR 0024)
- [x] Linux prefaulting with a 256 KiB `MADV_POPULATE_WRITE` window (ADR 0025)
- [x] Bare-metal Linux benchmarks (AWS `z1d.metal`, ext4 and xfs)
- [x] Release v0.4.0: repository made public, both crates published to crates.io, and GitHub release created (2026-11-02)

## Path to v1.0

Owner direction of 2026-11-03: make tickertape the default local, replayable event journal that libraries in
different languages and frameworks can integrate. Priorities are predictable sustained performance, a small core,
easy integration and a dependable compatibility contract. This does not require replacing networking transports,
databases or every kind of queue.

The milestones below define the work before v1.0. Intermediate releases can ship completed improvements; their
version numbers and dates are not fixed. Performance changes must preserve validation, memory safety and the crash
contract. An optimization investigation can close with a documented, measured decision that the proposed change
does not help or cannot preserve those guarantees; such a decision must be explicit in the roadmap and its ADR.

### Next release: sustained performance

- [ ] **Make sustained performance the next release's main objective.**
  - Establish reproducible workloads for bursty and sustained traffic, periodic checkpoints, segment rolls,
    multiple readers and a reader falling behind. Include datasets larger than RAM.
  - Report append throughput, visible and durable-only reader latency, checkpoint/roll latency, CPU usage and
    reader lag. Record p50/p99/p99.9/p99.99 and maxima for latency, with the workload's durability and wait settings.
  - Publish the remaining bottlenecks and supported operating envelope, including overload behavior, rather than
    treating a short peak-throughput result as evidence of sustainable capacity.
- [ ] **Sustained-load comparison benchmarks.**
  - Extend the comparison suite with repeated runs of at least 10 minutes per configuration for every compatible
    tool, covering both existing payload sizes and rates below and near sustainable storage throughput.
  - Cover steady write-back and repeated rolls on Linux ext4/xfs; record macOS results separately. Separate
    visibility-only workloads from periodic durability workloads and document unequal guarantees or unsupported cases.
  - Preserve raw results, environment, exact commands, achieved rates and late sends. Investigate the ext4 stalls
    observed in ADR 0025 and publish comparisons without coordinated omission or hidden dropped records.
- [ ] **Performance diagnostics with eBPF.**
  - Add a reproducible Linux diagnostic playbook using existing eBPF tools to investigate scheduler delays,
    page faults, filesystem/write-back stalls and block I/O during the sustained workloads.
  - Correlate traces with application latency and checkpoint/roll events; record tool/kernel requirements and
    tracing overhead, and confirm performance conclusions with tracing disabled.
  - Keep diagnostics optional: the library and CLI must work without eBPF, elevated tracing permissions or a
    collector. Kernel-event recording and XDP/AF_XDP ingestion remain separate proposals.
- [ ] **Remove the mapped reader's validation copy.**
  - Compute CRC and validate record fields directly from atomic loads, eliminating the per-record scratch copy
    while retaining validation before creating any borrowed slice into the mapping.
  - Preserve corruption detection and publication ordering; review the safety argument and run the relevant
    concurrency, corruption and crash checks. Record replay and live-reader results on x86-64 and aarch64.
- [ ] **Cheaper checkpoints with prefaulting.**
  - Measure how much prefaulted, unwritten dirty data contributes to checkpoint cost on each platform. The current
    implementation already restricts `msync` to written ranges, then performs file-wide sync/barrier operations.
  - Investigate reducing the remaining flush work without weakening data-before-marker ordering or the crash
    contract. Record before/after checkpoint and durable-only reader latency, or document platform limitations.
- [ ] **Finish eliminating avoidable work.**
  - Profile append, both readers, notification and serialization paths under the sustained workloads; document
    allocations, copies and synchronization that remain on those paths.
  - Evaluate batched append and batched notifications with explicit publication, partial-failure and latency
    semantics. Ship changes supported by measurements; document rejected approaches and their tradeoffs.
  - Exercise the existing `append_with` API in integration examples so encoders can write directly into a record.
- [ ] **ARM server measurements.**
  - Run correctness/crash checks and the sustained comparison suite on bare-metal aarch64 Linux, targeting
    Graviton (`c7gd.metal`), alongside the existing x86-64 and macOS evidence.
  - Record CPU, kernel, filesystem, storage, affinity, CRC implementation and commands; resolve architecture-specific
    correctness failures and explain material performance differences before claiming cross-architecture results.

### Following milestones: adoption and scale

- [ ] **Build a small interoperability layer.**
  - Provide a C ABI with documented ownership, record lifetimes, errors, threading, durability and versioning, plus
    a C/C++ example. Keep bindings and framework adapters outside the core crate.
  - Provide installable Python bindings with batch operations and explicit copy/lifetime behavior. Preserve the
    dependency-free read-only decoder; live zero-copy Python access is not required.
  - Ship one complete integration with an external library/framework, selected and named before implementation,
    demonstrating recording, live consumption, replay and export.
  - Add cross-language fixtures that verify payloads, metadata, checksums, sequence boundaries and durable-only
    behavior. Validate installation and a documented end-to-end example for each delivered binding.
- [ ] **Make replay and recovery excellent at scale.**
  - Measure startup, seek, catch-up, replay and crash recovery across many segments and journals larger than RAM,
    including retention, archived history and missing/corrupt segments. Publish dataset sizes and resource usage.
  - Implement rebuildable sparse indexes if measured seek costs justify them; indexes must never bypass required
    validation or allow stale positions after recovery/replacement.
  - Address measured bottlenecks while keeping memory bounded and preserving the rule that recovery does not
    silently discard committed history. Record regression workloads and remaining limits.

### v1.0 readiness

- [ ] **Work toward a dependable 1.0 contract.**
  - Review and document the supported Rust API, delivered binding APIs/ABI, CLI machine-readable output, on-disk
    format compatibility and upgrade policy. Resolve known breaking changes before declaring those surfaces stable.
  - Add compatibility fixtures for previously released format-v1 journals and longer-running crash, corruption,
    retention and concurrency tests on supported platforms, including aarch64 Linux.
  - Provide inexpensive operational metrics for reader lag, checkpoint age, storage headroom and retention
    failures, with documented sampling costs and no mandatory telemetry service.
  - Exercise release/installation/upgrade instructions and the external integration against a release candidate.
    Complete the release checklist and publish guarantees, measured operating limits and known issues.

**v1.0 release gate:** all milestones above are complete under their stated criteria, CI is green on Linux and
macOS, the aarch64 checks pass, and no known unresolved issue violates the published safety, corruption-detection,
durability or compatibility contract. Completing the feature list makes v1.0 a release candidate; satisfying this
gate makes it ready to release. Universal adoption and being fastest in every workload are not release criteria.

## Later (explicitly out of scope for v0.1)

- Replication to other hosts, and networking of any kind.
- Multiple writers to the same journal.
- Windows support; network filesystems.
- Live zero-copy readers in Python (the planned bindings use explicit copy/batch semantics; the read-only decoder
  and Parquet export remain available).
- Typed zero-copy schemas (rkyv/zerocopy). Payloads are opaque bytes plus a schema id.
- ~~Futex/eventfd wake-ups~~ (moved into v0.3, ADR 0017).
- ~~Any latency claim compared with other tools~~ (owner decision: comparison suite in v0.3, ADR 0018).
- Daily (UTC) segment rolling, unless it fits after M4 without risk.
- ~~Retention policies~~ (moved into v0.3, ADR 0016).
- ~~Publishing to crates.io~~ (done 2026-11-02 with the first public release, v0.4.0).
