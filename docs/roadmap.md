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

Owner decisions of 2026-11-19:

- **More bindings and install channels** are part of v1.0 (ADR 0031). Windows stays out of scope.
- **The external integration** is an iceoryx2 recorder and replayer (ADR 0030; the owner left the choice to the
  maintainers).
- **Cloud measurements run on Google Cloud**, not AWS: x86-64 and aarch64 (Google Axion) virtual machines with
  local SSD. Bare metal is outside the project's quota and budget there, so new results are labelled as cloud
  VMs and are not compared directly with the bare-metal v0.4.0 results.

The milestones below define the work before v1.0. Intermediate releases can ship completed improvements; their
version numbers and dates are not fixed. Performance changes must preserve validation, memory safety and the crash
contract. An optimization investigation can close with a documented, measured decision that the proposed change
does not help or cannot preserve those guarantees; such a decision must be explicit in the roadmap and its ADR.

Status of each item as of 2026-11-23, after the cloud results of the sustained sessions, the checkpoint work, the
comparison, the readers, the scale run, the long verified runs and the confirming long runs were recorded in
[`benchmarks.md`](benchmarks.md):
an item is ticked only when every criterion holds; each open item says what is still missing. Version 1.0.0 was
released with seven items open, accepted by the owner as known gaps
([Released as 1.0.0 with known gaps](#released-as-100-with-known-gaps)). Version 1.1.0 was released on 2026-11-27
(`CHANGELOG.md` → 1.1.0); the lines that its work changed say what is true on that day, and what comes next is
[After 1.1](#after-11).

### Next release: sustained performance

- [x] **Make sustained performance the next release's main objective.** (done 2026-11-23: the confirming long runs
  of issue #45)
  - Establish reproducible workloads for bursty and sustained traffic, periodic checkpoints, segment rolls,
    multiple readers and a reader falling behind. Include datasets larger than RAM.
  - Report append throughput, visible and durable-only reader latency, checkpoint/roll latency, CPU usage and
    reader lag. Record p50/p99/p99.9/p99.99 and maxima for latency, with the workload's durability and wait settings.
  - Publish the remaining bottlenecks and supported operating envelope, including overload behavior, rather than
    treating a short peak-throughput result as evidence of sustainable capacity.
  - Status: the workloads exist (`tt-sustained`, ADR 0026, #11), and the sessions' results, operating envelope and
    remaining bottlenecks, including the overload behaviour of retention, are recorded in `docs/benchmarks.md`
    (2026-11-19 to 2026-11-22) for both payloads with background checkpoints (the 1,024 B scenarios in the second
    session, at `3fea1c0`, where every run finished). In the long runs at `3fea1c0` retention fell behind and the disk
    filled (issue #45); after the amendment of ADR 0044 the same runs for the full hour at `fbe5785` completed with the
    journal at its limit plus what was written since the last durable checkpoint, verified from the file afterwards
    (issue #45, closed). Recorded as not measured in the operating envelope: an archiving writer under a constant queue
    of checkpoints, and the lagging and beyond-RAM scenarios with background checkpoints (recorded with blocking
    checkpoints).
- [ ] **Sustained-load comparison benchmarks.**
  - Extend the comparison suite with repeated runs of at least 10 minutes per configuration for every compatible
    tool, covering both existing payload sizes and rates below and near sustainable storage throughput.
  - Cover steady write-back and repeated rolls on Linux ext4/xfs; record macOS results separately. Separate
    visibility-only workloads from periodic durability workloads and document unequal guarantees or unsupported cases.
  - Preserve raw results, environment, exact commands, achieved rates and late sends. Investigate the ext4 stalls
    observed in ADR 0025 and publish comparisons without coordinated omission or hidden dropped records.
  - The repeated 10-minute runs on Linux ext4 and xfs, x86-64 and aarch64, are recorded in `docs/benchmarks.md`
    (2026-11-19 to 2026-11-22), with tickertape's rows from a second session at a newer commit (the first session's
    failed on the open-files defect fixed in #38), and the ext4 stall investigation is published (ADR 0025 →
    Amendment).
  - Rerun of the whole comparison at v1.0.0 with the corrected harness (issue #57; JVM warm-up, `microTouch()`, 1 GiB
    `blockSize`, pinning; every tool in one session per machine): `docs/benchmarks.md` → "comparison rerun at v1.0.0"
    (2026-11-25 to 2026-11-26), Linux x86-64 and aarch64, ext4 and xfs, Google Cloud VMs. It supersedes the earlier
    Linux comparisons and their single-session caveat. Not run: bare metal, unpinned sustained runs. Not rerun with
    1.1.0 (as of 2026-11-27): the published comparison is of v1.0.0, and the README's "Since 1.0.0" list is measured
    against other baselines ([After 1.1](#after-11), item 3).
  - Roll and stall work released in 1.1.0 (issue #60; `CHANGELOG.md` → 1.1.0): on Linux, early write-back stops at the
    128 KiB folio boundary below the writer (ADR 0058, change 1, the default; on aarch64 at 90 % of the device's rate the
    seconds with an append of 50 ms or more went from 138–151 to 0 on ext4: `docs/benchmarks.md` → "early write-back to
    the folio boundary"); the prefault helper unmaps and closes finished segments (ADR 0059; the roll's median append
    without checkpoints 0.83–1.06 ms → 169–179 µs: `docs/benchmarks.md` → "finished segments released off the writer's
    thread"); and an opt-in spare segment on Linux (ADR 0049, off by default). Still open in #60 (its comment of
    2026-11-26): ADR 0058's change 2 (proposed), why the aarch64 device queues on that machine shape, the helper-CPU
    delay, the macOS roll candidates, and the measurements ADR 0049 needs before the spare segment could be Linux's
    default.
  - macOS, recorded 2026-11-26 on the owner's laptop (Apple M4 Pro, APFS; shared and not idle: `docs/benchmarks.md` →
    "sustained load, roll waits and checkpoint cost on macOS"): tickertape's sustained session with repeated 10-minute
    steady runs at 50 % and 90 % of the probe for both payloads, shorter bursty and visibility-only runs, and a
    comparison with Chronicle Queue and Aeron at identical absolute rates in 120-second runs.
  - Missing on macOS: the comparison at each tool's own probe rates and in 10-minute runs, and the lagging and
    beyond-RAM scenarios. Both need more free disk than the shared laptop allowed (20 GB of data at any moment).
- [x] **Performance diagnostics with eBPF.** (done 2026-11-25: the overhead and an untraced confirmation, issue #60)
  - Add a reproducible Linux diagnostic playbook using existing eBPF tools to investigate scheduler delays,
    page faults, filesystem/write-back stalls and block I/O during the sustained workloads.
  - Correlate traces with application latency and checkpoint/roll events; record tool/kernel requirements and
    tracing overhead, and confirm performance conclusions with tracing disabled.
  - Keep diagnostics optional: the library and CLI must work without eBPF, elevated tracing permissions or a
    collector. Kernel-event recording and XDP/AF_XDP ingestion remain separate proposals.
  - Done 2026-11-25 (issue #60, `docs/benchmarks.md` → "2026-11-25, roll and journal stalls" → Tracing overhead): the
    tracing overhead of the probes attached at a sustained workload on a cloud aarch64 and x86-64 Linux machine, off and
    on interleaved, and a conclusion (the aarch64 ext4 stalls) confirmed with tracing off. The playbook, with its
    tool and kernel requirements and how to measure both, is #31.
- [x] **Remove the mapped reader's validation copy.**
  - Compute CRC and validate record fields directly from atomic loads, eliminating the per-record scratch copy
    while retaining validation before creating any borrowed slice into the mapping.
  - Preserve corruption detection and publication ordering; review the safety argument and run the relevant
    concurrency, corruption and crash checks. Record replay and live-reader results on x86-64 and aarch64.
  - Follow-up: bound the zero-copy reader's loads by a separately synchronised publication frontier (a shared word
    the writer advances on every append), so a damaged length can no longer make validation load bytes a live writer
    is storing (ADR 0027 → "Which loads can race"). A protocol change: its own ADR, measurements and review. A
    reader-side frontier alone does not close the other known limitation, a zeroed commit word that lets recovery
    rewrite published records in place under a live reader.
  - The follow-up is decided in [ADR 0048](decisions/0048-publication-frontier.md) (accepted): the shared word
    is deferred in 1.x; the checkpoint marker bounds the loads instead, which helps replay and catch-up, not a reader
    tailing the head. The bound is built (issue #62).
  - The copy is gone (ADR 0027, #9); replay results for both
    readers on x86-64 and aarch64 and the live latency of both readers on both architectures (the sustained session) are
    recorded in `docs/benchmarks.md` (2026-11-19 to 2026-11-22).
- [ ] **Cheaper checkpoints with prefaulting.**
  - Measure how much prefaulted, unwritten dirty data contributes to checkpoint cost on each platform. The current
    implementation already restricts `msync` to written ranges, then performs file-wide sync/barrier operations.
  - Investigate reducing the remaining flush work without weakening data-before-marker ordering or the crash
    contract. Record before/after checkpoint and durable-only reader latency, or document platform limitations.
  - A checkpoint that does not stop appends (`Writer::request_checkpoint`, ADR 0038, accepted): merged (#28, with
    the Linux measurement the ADR describes, `tt-sustained --checkpoint-mode`, in the pull request's comments), with
    a bounded number of open segments between checkpoints (#38), `RecoveryPolicy::Fail` holding back the changes from
    the first header it would restore until the open can no longer refuse (#35), and exposed through the C ABI, C++,
    Python, Java and the iceoryx2 recorder (#39).
  - Linux measured on 2026-11-26 (issue #62; `docs/benchmarks.md` → "what prefaulted, unwritten pages add to a
    checkpoint", x86-64 and aarch64, ext4 and xfs): the `MADV_POPULATE_WRITE` window is dirty, so every checkpoint
    that covers appends writes its 256 KiB of zeros, in the whole-file `fdatasync` after the ranged `msync` (156–429
    µs), and the blocking checkpoint's median rose from 0.35–0.52 to 0.50–0.73 ms on aarch64. It also write-protects
    the window, so the writer faults it in again (33–36 faults per checkpoint). An idle writer pays once. Syncing the
    active segment by its ranged `msync` alone removed both costs in an experiment; it changes the crash contract's
    barrier step and is not built.
  - The ranged sync is [ADR 0058](decisions/0058-prefault-residual-faults.md)'s change 2: proposed as of 2026-11-27,
    not in 1.1.0 (`docs/benchmarks.md` → "Does the ranged sync still flush a volatile write cache?"). Its change 1, the
    default from 1.1, stops early write-back below the folio the writer fills; a checkpoint may find up to 128 KiB more
    to write.
  - Missing: a drive with a volatile write cache; background checkpoints with the window (on Linux). The Linux
    results of ADR 0038 and before/after checkpoint and durable-only reader latency are recorded in
    `docs/benchmarks.md` (2026-11-19 to 2026-11-22).
  - macOS, measured 2026-11-26 (`docs/benchmarks.md` → "sustained load, roll waits and checkpoint cost on macOS"):
    on APFS the prefault advice (`MADV_WILLNEED`) dirties the pages it advises, so each checkpoint's file-wide `fsync`
    writes the zeros advised since the last one (up to the 8 MiB window: +2.5–3.3 ms on a 17 ms checkpoint at 21 MB/s
    of records); without prefaulting the spare segment is off too and the first checkpoint after each roll writes about
    half of the new 128 MiB file (32–36 ms instead of 16.5–18.5 ms at 21 MB/s). A checkpoint's floor is its two
    `F_FULLFSYNC`, about 9 ms. With no early write-back on macOS a checkpoint writes everything appended since the last
    one: about 0.2 s at 380–620 MB/s.
  - Missing on macOS: an advice that does not dirty pages, measured.
- [x] **Finish eliminating avoidable work.** (done 2026-11-26: the profiles, issue #62)
  - Profile append, both readers, notification and serialization paths under the sustained workloads; document
    allocations, copies and synchronization that remain on those paths.
  - Evaluate batched append and batched notifications with explicit publication, partial-failure and latency
    semantics. Ship changes supported by measurements; document rejected approaches and their tradeoffs.
  - Exercise the existing `append_with` API in integration examples so encoders can write directly into a record.
  - Batched append with one wake-up per batch is done (ADR 0041, #32), and `append_with` is used by `demo-producer`,
    the C++ example and the iceoryx2 recorder. The profiles of both readers (spinning, blocking, durable-only), the
    notification path and the serialization paths, with the allocations, copies, system calls and synchronization that
    remain and whether each is necessary, are in `docs/benchmarks.md` (2026-11-25). They are of v1.0.0; the
    durable-only reader has not been profiled since #66 changed how it reads the checkpoint marker. Candidates found
    there and not taken: a smaller read of the safe reader at the tail (measured, mixed result, branch
    `core/safe-reader-tail-reads`), a shared clock read in the spin loop, wakes sent to readers that are registered but
    not asleep, the outlined atomic call on aarch64 (`-C target-feature=+lse`), and durable-only readers that keep their
    checkpoint descriptor and are woken only by checkpoints. Two of them are in 1.1.0: the safe reader reads 4 KiB at
    the tail while it waits for every record ([ADR 0052](decisions/0052-safe-reader-tail-reads.md)), and a blocking
    durable-only reader is woken by checkpoints only ([ADR 0053](decisions/0053-durable-only-readers-wake-on-checkpoints.md));
    both are measured in `docs/benchmarks.md` (2026-11-26). The profiles were not repeated on 1.1.
- [ ] **ARM server measurements.**
  - Run correctness/crash checks and the sustained comparison suite on an aarch64 Linux server, targeting
    Google Axion (`c4a` with local SSD), alongside the existing x86-64 and macOS evidence. (Until 2026-11-19 this
    item named bare-metal Graviton, `c7gd.metal`.)
  - Record CPU, kernel, filesystem, storage, affinity, CRC implementation and commands; resolve architecture-specific
    correctness failures and explain material performance differences before claiming cross-architecture results.
  - Missing: an explanation of the aarch64 mapped reader's constant maximum in the checkpoint runs, and confirmation of
    the platform mechanism behind the lagging view of issue #43. Investigated on 2026-11-26 (`docs/benchmarks.md` → "the
    two aarch64 observations left open by 1.0.0"): the 10 ms maximum did not reproduce on a new `c4a-standard-16-lssd`
    at `main` or at its own commit (nor on 8 vCPUs, 2026-11-25); in the old data only the spinning reader's CPU had it,
    never the writer's or the safe reader's; it is not explained, and the trace to run if it returns is
    `benches/diagnostics/reader-pauses.bt`. The lagging view still occurs on `main` on aarch64 under memory pressure
    (about one event in 5 minutes, each cleared by the first paused re-read, within 69 µs) and not in 900 s on x86-64; 3
    of 10 traced events coincide with kswapd removing the frame's pages from the page cache at the write frontier
    (consistent with reclaim's deferred TLB invalidation), 6 have no such removal near them and are not explained; with
    the journal within RAM there was no reclaim and no event in 1,000 s, too short to confirm. (The appends of 50–128 ms
    in the 1,024 B steady-90 runs on aarch64 ext4 are attributed since 2026-11-25: `docs/benchmarks.md` → "roll and
    journal stalls"; with ADR 0058's change 1, the default from 1.1.0, none remained in the measured cells:
    `docs/benchmarks.md` → "early write-back to the folio boundary".) That view no longer stops a reader (readers
    tolerate it; `crash-contract.md` → Known risks and
    limits), and the long verified runs under forced compaction (`3fea1c0`, on aarch64 and x86-64) showed no false
    corruption and no lost write (issue #43, closed). Correctness, crash and soak checks run on aarch64 Linux in CI
    (#12); the sustained sessions and comparison on Axion are recorded in `docs/benchmarks.md` (2026-11-19 to
    2026-11-22) with CPU, kernel, filesystem, storage, affinity, CRC path and commands.

### Following milestones: adoption and scale

- [x] **Build a small interoperability layer.** (done 2026-11-20: C ABI #10, Python #7, C++ #19, Java #21, iceoryx2
  #13, metrics through the bindings #30, install tests #29; publishing is the next item; the names in ADRs 0029 and
  0037 are accepted; since then, names aligned with Rust #37 and background checkpoints through the bindings #39)
  - Provide a C ABI with documented ownership, record lifetimes, errors, threading, durability and versioning, plus
    a C/C++ example. Keep bindings and framework adapters outside the core crate.
  - Provide installable Python bindings with batch operations and explicit copy/lifetime behavior. Preserve the
    dependency-free read-only decoder; live zero-copy Python access is not required.
  - Provide a header-only C++ wrapper and a Java binding (Foreign Function & Memory API), both over the C ABI
    (ADR 0031). Go, Node.js and .NET wait until after v1.0.
  - Ship one complete integration with an external library/framework, selected and named before implementation,
    demonstrating recording, live consumption, replay and export: `tickertape-iceoryx2`, a recorder and replayer
    for iceoryx2 publish/subscribe services (ADR 0030).
  - Add cross-language fixtures that verify payloads, metadata, checksums, sequence boundaries and durable-only
    behavior. Validate installation and a documented end-to-end example for each delivered binding.
- [x] **Make installation easy** (ADR 0031).
  - Static musl builds of the CLI for Linux, cargo-binstall metadata, a Homebrew tap, PyPI wheels for the Python
    bindings and for the CLI, a C library archive per platform, and Maven Central for the Java binding.
  - Build provenance attestations with a software bill of materials for every released file.
  - Every channel is tested at release by installing from it; a channel that isn't tested isn't announced.
  - Status: the release tooling for every channel exists (static builds, cargo-binstall and Homebrew in part 1; the
    CLI wheel, the C library archives and Maven Central in part 2), each publishing step switched off until the
    owner's one-time set-up (`docs/playbooks/release-checklist.md`). Done when a release has shipped through each.
  - Done with v0.5.1 (2026-11-23): crates.io, the GitHub release archives (static builds included),
    cargo-binstall, Homebrew, the C library archives, both PyPI projects and Maven Central, each installed from
    after publication (by the release workflows, and by hand where they do not test). mise is not tested.
- [ ] **Make replay and recovery excellent at scale.**
  - Measure startup, seek, catch-up, replay and crash recovery across many segments and journals larger than RAM,
    including retention, archived history and missing/corrupt segments. Publish dataset sizes and resource usage.
  - Implement rebuildable sparse indexes if measured seek costs justify them; indexes must never bypass required
    validation or allow stale positions after recovery/replacement.
  - Address measured bottlenecks while keeping memory bounded and preserving the rule that recovery does not
    silently discard committed history. Record regression workloads and remaining limits.
  - Status: `tt-scale` and the growth table are in place (#33), and the cloud run on local SSD with a journal larger
    than RAM is recorded in [`benchmarks.md`](benchmarks.md) with dataset sizes and resource usage; ADR 0042 is accepted
    with no sparse index for 1.0, since a seek costs at most one segment's scan (`docs/operations.md` → Choosing a
    segment size). The defect the run found, one open file per segment between checkpoints, is fixed (#38).
  - Measured on 2026-11-25 ([`benchmarks.md`](benchmarks.md) → Replay and recovery at scale: the gaps of 1.0.0): the
    10,000- and 20,000-segment journals (and 100,000), archived history, missing and corrupt segments, catch-up under a
    live writer, the zero-copy reader, and seeks with the seek index (issue #62). The rule that recovery does not silently
    discard committed history held in the 21 measured cases, for records still in a segment when the writer opened. Damage
    that recovery cannot tell from a torn append (C11 (3)) is not among them: it is zeroed without a copy under either
    policy.
  - What those runs found, as of 1.1.0 (2026-11-27): readers no longer stop silently at a zeroed commit word in older
    history; they report it (issue #79, closed; ADR 0050). Retention reads only what its policy needs and no longer
    opens every segment or lists the directory per removed segment (issue #80, closed; ADR 0051). Still missing: every
    seek, with or without a seek index, and every retention run still list the whole directory once, and a writer's open
    and a stand-alone `max_bytes` plan still read every file; these costs are stated in `docs/operations.md` → "How many
    segments to keep", not changed.
  - Released in 1.1.0 (issue #59, #72): the sparse, rebuildable seek index per sealed segment of
    [ADR 0045](decisions/0045-sparse-seek-index.md), built by `tickertape index` and used by readers that opt in, with
    forward seeks from the cursor (ADR 0046) and `Reader::seek_to_end` in every binding. Every record from the landing
    record on is validated; the records before it are not read, which is why it is opt-in. Measured in
    `docs/benchmarks.md` → "seeks with and without the seek index". Deferred there (issue #59 stays open for them): the
    checkpoint marker as a jump point in the newest segment, a cursor the caller saves, then building from the writer.

### v1.0 readiness

- [x] **Work toward a dependable 1.0 contract.**
  - Review and document the supported Rust API, delivered binding APIs/ABI, CLI machine-readable output, on-disk
    format compatibility and upgrade policy. Resolve known breaking changes before declaring those surfaces stable.
  - Add compatibility fixtures for previously released format-v1 journals and longer-running crash, corruption,
    retention and concurrency tests on supported platforms, including aarch64 Linux.
  - Provide inexpensive operational metrics for reader lag, checkpoint age, storage headroom and retention
    failures, with documented sampling costs and no mandatory telemetry service.
  - Exercise release/installation/upgrade instructions and the external integration against a release candidate.
    Complete the release checklist and publish guarantees, measured operating limits and known issues.
  - Done with 1.0.0 (2026-11-23): the surfaces are reviewed and documented in [`compatibility.md`](compatibility.md)
    (ADR 0043, accepted; resolved in #37), with the fixtures and soak tier (#12) and the metrics (#8, #30) in place.
    The release preparation set the C ABI to version 1 and froze its baseline; the release-candidate exercise was
    done with v0.5.1; the confirming long run (issue #45) was not repeated, since the library's code is unchanged
    since `fbe5785`. The 1.0.0 fixtures were added after the release, written by the released version. Measured operating limits are published for 64 B and 1,024 B
    ([`benchmarks.md`](benchmarks.md) → Operating envelope and remaining bottlenecks); known issues are in
    [`crash-contract.md`](crash-contract.md) → Known risks and limits.
  - Released in 1.1.0 (issue #61, [ADR 0047](decisions/0047-latency-histograms.md), all three steps built): latency
    histograms in the library, the age of records as a reader returns them, `tickertape age`, and opt-in histograms of
    the writer's appends, batches, rolls and checkpoints (`WriterConfig::latency`, off by default), readable in the
    writer's process. Their cost is in `docs/benchmarks.md` → "what the writer's latency histograms cost". Open (issue
    #61; deferred by the ADR): the bindings, and reading the writer's histograms from another process and in
    `tickertape status`.

**v1.0 release gate:** all milestones above are complete under their stated criteria, CI is green on Linux and
macOS, the aarch64 checks pass, and no known unresolved issue violates the published safety, corruption-detection,
durability or compatibility contract. Completing the feature list makes v1.0 a release candidate; satisfying this
gate makes it ready to release. Universal adoption and being fastest in every workload are not release criteria.

### Released as 1.0.0 with known gaps

Version 1.0.0 (2026-11-23) was released on the owner's decision with seven milestones above still open, so the gate's
first condition was waived for them; its other conditions held (CI green on Linux and macOS, the aarch64 checks
passing, no known unresolved issue violating the published contract). None of the seven changes what 1.0 promises:
they are measurements, explanations and one design record. They stay open and continue in 1.x, except
where a line says it is closed. Each line says where it stands on 2026-11-27, after 1.1.0 (tracked in issue #62):

- **Sustained-load comparison:** macOS results recorded on 2026-11-26 (tickertape's sustained session; a reduced
  comparison at identical low rates); still missing on macOS: the comparison at each tool's probe rates and 10-minute
  comparison runs, the lagging and beyond-RAM scenarios (`docs/benchmarks.md`, 2026-11-26, macOS). Nothing new for
  macOS in 1.1.0; on Linux the comparison is still the one of v1.0.0 ([After 1.1](#after-11), item 3).
- **eBPF diagnostics:** closed on 2026-11-25; the tracing overhead during the sustained workloads and a conclusion
  confirmed with tracing off are recorded (`docs/benchmarks.md` → "2026-11-25, roll and journal stalls").
- **Mapped reader:** closed on 2026-11-25. The publication-frontier follow-up had no ADR; it has
  [0048](decisions/0048-publication-frontier.md) (the marker bound it decides is built and released in 1.1.0; the shared
  word stays deferred in 1.x). The limitation it narrows is part of
  the 1.0 contract ([`compatibility.md`](compatibility.md); `crash-contract.md` → Readers, the writer and memory safety).
- **Cheaper checkpoints:** how much prefaulted, unwritten data adds to a checkpoint is measured on Linux since
  (2026-11-26, [`benchmarks.md`](benchmarks.md)): the 256 KiB window is written as zeros by every checkpoint that
  covers appends, 0.16–0.43 ms of the checkpoint. On macOS (2026-11-26, the same file) the 8 MiB `MADV_WILLNEED` window
  is dirtied as well, and each checkpoint writes the zeros advised since the last one (about 2.5–3.3 ms of a 17 ms
  checkpoint at 21 MB/s). Measured on both platforms; nothing in 1.1.0 removes that cost. The ranged sync that removed
  it in a Linux experiment is ADR 0058's change 2, proposed and waiting for a drive with a volatile write cache
  ([above](#next-release-sustained-performance)).
- **Avoidable work:** the profiles of the readers, notification and serialization paths were not documented; done
  since for v1.0.0 ([above](#next-release-sustained-performance), `docs/benchmarks.md` 2026-11-25); the durable-only
  reader has not been profiled since #66, nor since 1.1.0 changed how it waits (ADR 0053). Two of the candidates the
  profiles found are in 1.1.0 (ADR 0052, ADR 0053).
- **ARM servers:** one of the two latency observations on aarch64 (the 10 ms reader maximum) and the platform mechanism
  behind issue #43 are not explained; the appends of 50–128 ms are (issue #60, 2026-11-25), and with ADR 0058's change
  1, the Linux default from 1.1.0, none remained in the measured cells. On 2026-11-26 the 10 ms
  maximum did not reproduce (it belonged to one VM); part of the lagging view is consistent with reclaim removing the
  frame's pages under the readers, the rest is not explained, and neither is confirmed (`docs/benchmarks.md`,
  2026-11-26).
- **Replay and recovery at scale:** measured on 2026-11-25: archived history, missing and corrupt segments, catch-up
  under a live writer, the zero-copy reader and journals of 10,000 to 100,000 segments ([`benchmarks.md`](benchmarks.md)).
  Not measured: archiving under a live writer at this scale, the 128 MiB archive on aarch64, catch-up of a durable-only
  reader, the seek index on a journal being written, xfs (and other filesystems than ext4), and anything with #74 to #77
  (#76 changes the zero-copy reader).
  Released in 1.1.0: readers report a zeroed commit word in older history instead of stopping silently (issue #79, ADR
  0050), retention reads only what its policy needs (issue #80, ADR 0051), and the sparse seek index (ADR 0045, for
  sealed segments; opt-in), measured. Not changed: the one directory listing per seek and per retention run
  (`docs/operations.md` → "How many segments to keep"). Not built: building the index from the writer, the checkpoint
  marker as a jump point in the newest segment, a cursor the caller saves (issue #59).

## After 1.1

The order below is proposed by the maintainers (2026-11-27); the owner has not confirmed it yet.

1. **One bare-metal session.** Every Linux number published since v0.4.0 is from cloud VMs whose local SSDs write at
   most 351 MB/s; bare metal was left out for now ([owner decisions of 2026-11-19](#path-to-v10); README →
   Performance → "Standing caveats"; `benchmarks.md` → comparison rerun at v1.0.0 → "What these figures show and do
   not show"). Closed by a session on a bare-metal machine, recorded in `benchmarks.md`; the 256 B section's "What
   would be needed to compare on equal terms" names the set-up for its cells.
2. **Why warm replay is slower than Chronicle Queue's** in three of four pinned ext4 cells of the rerun at v1.0.0
   (README → Performance → "Where tickertape is ahead, and where it is behind"; `benchmarks.md` → comparison rerun →
   "Short mode: W1 and W3"). No document records a cause. 1.1.0's release check found 64 B replay level with 1.0.0
   (`benchmarks.md` → "release 1.1.0 against 1.0.0"), so 1.1 is not expected to change it. Closed by a recorded
   explanation, or a change measured in those cells.
3. **A rerun of the comparison with 1.1.** The published comparison is of v1.0.0; the README's "Since 1.0.0" list is
   measured against other baselines and changes none of its rows (README → Performance → Since 1.0.0). Closed by a
   rerun at a 1.1 release with the same harness (`benches/rerun.py`), replacing the README's rows.
4. **Two findings of the 256 B, 100,000 records/s scenario** (`benchmarks.md` → [2026-11-27, Chronicle's published
   write-to-read example](benchmarks.md#2026-11-27-chronicles-published-write-to-read-example-in-our-harness-256-b-at-100000-recordss-google-cloud-vms)):
   the zero-copy reader's tail on ext4 from p99.99 up (388–461 µs), probably its pauses at segment rolls but not tied
   to them by per-record timing; and on tmpfs its 1.65–4.66 ms between p99.7 and p99.999 from the send start, cause
   not found, also seen with the spare segment on. On Linux the spare segment is off by default, so on tmpfs a roll
   zeroes the next segment on the writer's thread (maxima 26–32 ms); those runs did not measure `spare_segment(true)`.
   Closed by per-record timing that settles the rolls, a cause for the tmpfs tail, and the cells rerun with the spare
   segment (whose default is part of issue #60).
5. **What the decision records leave open**, by their status lines:
   [ADR 0058](decisions/0058-prefault-residual-faults.md)'s change 2, the ranged sync of the active segment (proposed; waits for a drive with a volatile write cache);
   [ADR 0042](decisions/0042-replay-and-recovery-at-scale.md)'s options 3 and 4 (not built; ADR 0051 did the retention
   part of option 4 another way); [ADR 0047](decisions/0047-latency-histograms.md)'s deferred list (bindings, the
   writer's histograms from another process, and more); [ADR 0048](decisions/0048-publication-frontier.md)'s shared
   frontier word (deferred in 1.x). Also [ADR 0052](decisions/0052-safe-reader-tail-reads.md)'s "known gap": one fill
   counted as following after a reader attaches a wake-up region, with no test of that case.
6. **Open issues:**
   - #59: building indexes from the writer, the checkpoint marker as a jump point, a cursor the caller saves (ADR 0045,
     deferred).
   - #60: ADR 0058's change 2, the aarch64 device's queueing, the helper-CPU delay, the macOS roll candidates, ADR
     0049's measurements for a Linux spare-segment default ([above](#next-release-sustained-performance)).
   - #61: the bindings, and the writer's histograms from another process and in `tickertape status`
     ([above](#v10-readiness)).
   - #62: what [Released as 1.0.0 with known gaps](#released-as-100-with-known-gaps) still lists: the macOS
     comparison, the two aarch64 observations, the cheaper checkpoint.
   - #86: the M4 Pro's twofold slowdown not re-measured, and 1.9–3.9 % for 40 bytes added at the end of `Reader`
     (zero-copy, 64 B, x86-64) not explained (ADR 0057).
   - #93: a way to open at the marker after a refusal; not planned for 1.1 because readers would also need a way to
     resume past the damaged position (ADR 0056, ADR 0050).

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
