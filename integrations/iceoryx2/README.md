# tickertape-iceoryx2

Record [iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2) publish/subscribe services into a tickertape journal,
follow the recording live from other processes, replay it back onto iceoryx2, and export it, to Parquet with the
`tickertape` CLI and to iceoryx2's own recording files for `iox2 service replay`.

It is a library and a command-line tool, `tickertape-iox2`. It lives outside the tickertape Cargo workspace, with its
own lockfile and version (0.1.0), and is not published to crates.io yet. Why iceoryx2 and what this integration
covers: [ADR 0030](../../docs/decisions/0030-iceoryx2-integration.md).

What tickertape adds to iceoryx2's own recorder (`iox2 service record`, one file per service): several services in
one journal, readers in other processes that follow the recording while it is written (in any language with a
tickertape reader), replay from any sequence number, segment rolling and retention, checksums on every record, and
tickertape's [crash contract](../../docs/crash-contract.md).

## Quick start

Run these from the repository root. They record a running iceoryx2 publisher (here the upstream `iox2` tool),
follow the recording from another process, replay it, and export it.

```bash
cargo install --locked iceoryx2-cli@0.10.0 tickertape-cli          # iox2 (upstream) and tickertape
cargo install --locked --path integrations/iceoryx2 --features cli  # tickertape-iox2
iox2 service publish demo/hello -m "68 65 6c 6c 6f" -t 100 -r 0 &    # job 1, a publisher: "hello" every 100 ms
tickertape-iox2 record /tmp/iox2-demo --service demo/hello &          # job 2, the recorder
cargo run --manifest-path integrations/iceoryx2/Cargo.toml --example follow -- --journal /tmp/iox2-demo --count 5
kill %2                                                               # stop recording (SIGTERM: final checkpoint)
tickertape-iox2 replay /tmp/iox2-demo --speed 1 --wait-subscribers 1 & sleep 1 && iox2 service subscribe replay/demo/hello -m 5
tickertape export /tmp/iox2-demo --format parquet --output /tmp/iox2-demo.parquet
tickertape-iox2 export /tmp/iox2-demo --output /tmp/iox2-demo.iox2   # for iox2 service replay
kill %1                                                               # stop the publisher
```

The `follow` example (step 5) uses only the `tickertape` crate and the documented layout below; it builds without
iceoryx2: `cargo build --manifest-path integrations/iceoryx2/Cargo.toml --example follow --no-default-features`.
`tickertape-iox2 info /tmp/iox2-demo` summarizes the recording. Every command has `--help`, with exit codes and the
`--json` schema.

`iox2 service replay` needs the target service to exist with the recorded types (it doesn't create it), for
example while the original publisher runs: `iox2 service replay -i /tmp/iox2-demo.iox2 demo/hello`.

## Trust: replay and import only recordings you trust

Replaying publishes recorded bytes as values of the recorded types. Every size, alignment, element count and
service setting read from a journal or a file is validated before it reaches iceoryx2, an allocation or `unsafe`
code (see [validity rules](#validity-rules)), so a damaged or hostile recording can't make the replayer access memory
out of bounds. What can't be checked is whether the bytes are valid *values*: a typed subscriber of a type with
invalid bit patterns (`bool`, an enum) has undefined behaviour in its own safe code if they are not. tickertape's
checksums detect accidental damage, not a journal or file written with wrong contents. So, as with upstream's own
`iox2 service replay`: **replay only recordings you trust**, and import only files you trust (a replay publishes
what was imported). The CLI says so in `replay --help` and `import --help`; the library makes the run-time-typed
replay an `unsafe fn` (below).

## Library

```rust,ignore
use tickertape_iceoryx2::recorder::{Recorder, TypedService};
use tickertape_iceoryx2::replayer::Replayer;

// Record: a typed service (public iceoryx2 API only), with the event time read from the user header.
let report = Recorder::builder("/tmp/ticks")
    .typed(TypedService::<Tick, TickHeader>::new("market/ticks").venue_ts(|h| h.venue_ts_ns))
    .build()?
    .run()?; // until SIGINT/SIGTERM, a stop flag or a limit

// Replay onto "replay/market/ticks" at the recorded pace. Safe: every value is rebuilt with zerocopy::FromBytes.
let report = Replayer::builder("/tmp/ticks")
    .typed::<Tick, TickHeader>("market/ticks")
    .speed(1.0)
    .build()?
    .run()?;
```

Typed payloads (`T` or `[T]`) and user headers must implement iceoryx2's `ZeroCopySend` and zerocopy's `IntoBytes` and
`Immutable` (for replay also `FromBytes`), so the bytes can be read and written without `unsafe`. Types with padding
don't qualify; record those with the `dynamic` feature. The examples in [`examples/`](examples/) are complete
programs: a typed publisher, subscriber and recorder, and the `follow` reader.

`Replayer::run` refuses a recorded service that isn't registered with `typed`. `Replayer::run_dynamic` (feature
`dynamic`) replays such services by their recorded type details, as the CLI does; it is an `unsafe fn` whose contract
is that the journal's payloads are valid values of the advertised types, which holds when the journal was recorded
from correct publishers of those types and hasn't been altered.

### Features

| Feature | Default | What it adds |
|---|---|---|
| `iceoryx2` | yes | The typed recorder and replayer. iceoryx2's public API only, no `unsafe`. |
| `dynamic` | no | Services whose types are only known at run time (what `iox2 service record` does), `Replayer::run_dynamic`, and conversion to and from iceoryx2's recording files. |
| `cli` | no | The `tickertape-iox2` binary (enables `dynamic`). |

Without default features the crate has the layout, `Catalog` and `info` only, and no iceoryx2 dependency.

**The `dynamic` trade-off.** Recording a service by name, without its Rust type, needs iceoryx2 API that upstream
marks internal and hides from its documentation (`__internal_set_payload_type_details`, `loan_custom_payload`,
`TypeDetail::__internal_new_from_parts`). The upstream CLI uses the same API. An iceoryx2 release may change it
without notice; all such use is in one module, [`src/dynamic.rs`](src/dynamic.rs), the only module of the library
with `unsafe` code (the crate denies `unsafe_code` everywhere else; the binary has one `unsafe` call, which passes the
user's trust on to `run_dynamic`). The command-line tool needs this path (it records services by name), so it enables
the feature. Library users who know their types can stay on the default features.

## Payload layout (version 1)

A recording is an ordinary tickertape journal. Records are told apart by schema id; ids `0x4932_0000` to
`0x4932_FFFF` are reserved for this crate, and other schema ids are left alone. All integers are little-endian;
strings are UTF-8 with a `u16` length in front. **User header and payload bytes keep the publisher's native
representation** (byte order, padding, the layout of its Rust or C types), which this layout does not describe.

The record's own timestamps: `local_ts_ns` is when the recorder received the message (system clock, nanoseconds since
the Unix epoch); `venue_ts_ns` is what an optional extractor read from the user header, or 0.

| Schema id | Record |
|---|---|
| `0x4932_0001` | descriptor: one recorded service |
| `0x4932_0002` | data record: one message |
| `0x4932_0003` | loss record: a gap in a publisher counter |

### Growth rule

- **Layout version** (in every record) changes only for incompatible changes. A reader rejects a version it doesn't
  know.
- **Flags** (`u16`, in every record): bits 0 to 7 are *required*, and a reader rejects a record with a required bit it
  doesn't know; bits 8 to 15 are *optional*, and a reader ignores those it doesn't know. Version 1 defines one bit:
  descriptor bit 0, `imported`.
- **Reserved fields** are written as 0 and ignored by readers. A later revision may use one only for a meaning that
  readers ignoring it still decode correctly, or behind a required flag bit.
- **Extension entries**: after a descriptor's recorder name, after a loss record's 48 bytes, and between a data
  record's 40-byte header and its user header (the header length field says where the user header starts):
  `u16` tag, `u16` reserved, `u32` length, then the bytes, repeated to the end of the area. Tag bit 15 means
  required: a reader rejects an unknown required tag and skips unknown optional ones; an entry that runs past its area
  is malformed. Version 1 defines no tags.

### Recording runs and repeated descriptors

A *recording run* is one run of the recorder, or one import. Its descriptors carry `run_start_seq`, the journal's next
sequence number when the run began. A service's descriptor may appear several times in a run with the same content:
the recorder writes each open service's descriptor again at the start of every journal segment, before the segment's
first data or loss record. A data or loss record names the most recent descriptor of its service before it.

- **Guaranteed:** in a journal written by this crate, every data and loss record whose segment is still there can be
  decoded, including after retention or `tickertape archive` removed older segments; except a record too large to
  share a segment with the descriptors (it then starts a segment of its own).
- **Not guaranteed:** that a record's descriptor is still there (journals written otherwise, or that exception).
  Readers skip, count and report such records instead of stopping: `info` (`undescribed_records`), `replay` and
  `export` (`undescribed`), and the `follow` example (`undescribed` lines).

### Descriptor (`0x4932_0001`)

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | magic `TTI2` |
| 4 | 2 | layout version, 1 |
| 6 | 2 | flags: bit 0 `imported`: the run was imported from a file with only relative times, so `local_ts_ns` is the chosen start time plus the offset |
| 8 | 8 | `run_start_seq` |
| 16 | 8 | run start, ns since the Unix epoch |
| 24 | 6 | iceoryx2 version of the recording side (major, minor, patch; `u16` each) |
| 30 | 1 | messaging pattern: 1 = publish-subscribe (the only value) |
| 31 | 1 | safe overflow: 1 = a full subscriber buffer drops its oldest sample; 0 = it doesn't, the publisher's backpressure strategy applies |
| 32 | 8 | `history_size` |
| 40 | 8 | `subscriber_max_buffer_size` |
| 48 | 8 | `subscriber_max_borrowed_samples` |
| 56 | 8 | `max_publishers` |
| 64 | 8 | `max_subscribers` |
| 72 | 8 | `max_nodes` |
| 80 | 8 | the recorder subscriber's buffer size |
| 88 | 1 | publisher counter location: 0 none, 1 user header, 2 payload |
| 89 | 1 | publisher counter width: 0 (exactly when there is no counter), 4 or 8 bytes; unsigned little-endian |
| 90 | 2 | reserved |
| 92 | 4 | publisher counter offset in bytes (0 without a counter) |
| 96 | | three type details: payload, user header, iceoryx2 system header |
| | | service name (`u16` length + UTF-8) |
| | | recorder (`u16` length + UTF-8), e.g. `tickertape-iceoryx2 0.1.0` |
| | | extension entries, to the end (none in version 1) |

Settings of 0 mean unknown (an import); the recorder buffer size is then 0 too.

Each type detail (iceoryx2's `TypeDetail`):

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | variant: 0 fixed size, 1 dynamic (a slice) |
| 1 | 1 | reserved |
| 2 | 2 | type name length |
| 4 | 4 | reserved |
| 8 | 8 | size (of one element, for a slice) |
| 16 | 8 | alignment |
| 24 | | type name |

### Data record (`0x4932_0002`)

| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | layout version, 1 |
| 2 | 2 | flags; none defined |
| 4 | 2 | header length `H`: where the user header starts; 40 in version 1 |
| 6 | 2 | reserved |
| 8 | 4 | user header length `h` |
| 12 | 4 | reserved |
| 16 | 8 | sequence number of the descriptor it names |
| 24 | 16 | iceoryx2 publisher id (`u128`, the sample's origin); 0 if unknown (imported) |
| 40 | | extension entries up to `H` (none in version 1) |
| `H` | `h` | user header bytes |
| `H + h` | rest | payload bytes |

Bytes are copied exactly as iceoryx2 holds them in shared memory, padding included; neither part is aligned inside the
record. A slice payload holds `payload length / element size` elements. Because each data record names its
descriptor, a reader that starts anywhere seeks to it and decodes, with no scan from the start. Data records carry the
publisher id so that a reader can attribute messages, and loss records, to publishers, and check counters itself.

### Loss record (`0x4932_0003`)

Written before the message that revealed a gap in a configured publisher counter.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | layout version, 1 |
| 2 | 2 | flags; none defined |
| 4 | 1 | kind: 1 gap (counter jumped forward), 2 backwards (repeated or reset) |
| 5 | 1 | counter width, 4 or 8 (the descriptor's) |
| 6 | 2 | reserved |
| 8 | 8 | sequence number of the descriptor it names |
| 16 | 16 | iceoryx2 publisher id |
| 32 | 8 | expected counter |
| 40 | 8 | received counter |
| 48 | | extension entries (none in version 1) |

Counters are compared modulo 2^(8 × width): with `d = (received − expected) mod 2^(8 × width)`, a gap of `d` missing
messages if `0 < d < 2^(8 × width − 1)`, backwards otherwise. A four-byte counter wrapping from `0xFFFF_FFFF` to 0 is
neither.

### Validity rules

A decoder must apply all of these; this crate applies them in `Descriptor::decode`, `DataRecord::check` and
`LossRecord::check` (and `Descriptor::encode` refuses whatever `decode` refuses), in every reader, before recording a
service, and before importing a file.

- Layout version 1; descriptor magic `TTI2`; no unknown required flag bit or extension tag; every extension entry
  inside its area; strings valid UTF-8; nothing missing.
- Enumerations as in the tables: type variant 0 or 1; messaging pattern 1; safe overflow 0 or 1; counter location 0,
  1 or 2 with width 0 exactly when the location is 0 (otherwise 4 or 8) and offset 0 without a counter; loss kind 1
  or 2.
- Each type: alignment a power of two from 1 to 4096; size a multiple of the alignment and at most 4,294,967,254 (the
  largest tickertape record payload); name at most 256 bytes. The user header and system header are fixed-size. A
  slice's element size is not 0: every element count of zero-sized elements has the same bytes, so such services are
  refused. A data record with the user header and one element fits in a record.
- The iceoryx2 chunk the types imply for the largest element count a record can hold (system header; user header
  aligned to its alignment; payload aligned to its alignment; element size rounded up to the payload's alignment
  times the count; all rounded up to the largest alignment), computed without overflow, is a valid Rust `Layout`.
- Service settings at most: `history_size` 65,536; `subscriber_max_buffer_size` and the recorder's buffer 65,536;
  `subscriber_max_borrowed_samples` 65,536; `max_publishers`, `max_subscribers` and `max_nodes` 4,096;
  `max_subscribers × (buffer + borrowed) + history_size` (samples one publisher preallocates) 16,777,216;
  `max_publishers × max_subscribers × (buffer + borrowed)` (queue slots) 4,294,967,296. These keep iceoryx2's own,
  unchecked arithmetic far from overflow; a running service outside them is refused (and retried) by the recorder.
- Service name 1 to 255 bytes.
- Counter: `offset + width` inside the user header, or inside one payload element.
- Data record: `H` at least 40; the user header inside the record; user header length equal to the descriptor's user
  header size; payload length equal to the payload size, or a multiple of the element size for a slice.
- Loss record: width equal to the descriptor's, which has a counter; expected and received fit in the width and
  differ; the kind matches the distance rule.

The normative description, with tests that pin every offset, is the rustdoc of [`src/layout.rs`](src/layout.rs).

## Behaviour and limits

- **Loss before the recorder is invisible to tickertape's sequence numbers.** They prove that nothing the recorder
  *wrote* was lost; they say nothing about messages it never received. iceoryx2 delivers into a bounded subscriber
  buffer: the recorder asks for 1,024 slots (`--buffer-size`, at most 65,536), capped by the service's
  `subscriber_max_buffer_size`, whose iceoryx2 default is **2**. When the buffer is full, a service with safe overflow
  (iceoryx2's default) replaces the oldest unread sample and the recorder never sees it; without safe overflow, the
  publisher's backpressure strategy decides. The recorder drains its buffers every poll interval (1 ms by default; on
  Linux iceoryx2's WaitSet can wake noticeably less often), taking at most 4,096 messages per service per pass so a
  busy service can't starve the others.
- **A checkpoint pauses recording.** tickertape has no background checkpoint yet, so the recorder syncs on its own
  thread (every `--checkpoint-interval-ms`, 1 s by default), and every message that arrives meanwhile must fit in the
  subscriber buffer. With iceoryx2's default of 2 slots, expect loss even at a few hundred messages per second;
  create services with a larger `subscriber_max_buffer_size` (or let the recorder create them). `record` prints, and
  each descriptor stores, the buffer size it got and the overflow policy.
- **Make loss detectable with a counter.** If each publisher puts a counter in its user header or payload (an
  unsigned little-endian integer, +1 per message, wrapping at its width), pass its position (`--counter header:0:8`,
  or `TypedService::counter`). The recorder then writes a loss record for every gap and reports gaps and missing
  messages per service. Counters are tracked per publisher and per recording run.
- **The recorder trusts a publisher's element count** for slice payloads, as any iceoryx2 subscriber does.
- **Failures.** A service that doesn't exist yet, can't be opened (types or settings outside the validity rules,
  other types than expected) or fails while receiving is reported (on stderr, and per service in the report) and
  retried every 100 ms; the other services keep recording. On a full volume, or a message too large for a journal
  record (larger than a segment), the recorder stops entirely, after a final checkpoint of what it has.
- **The recorder keeps a service alive.** While it runs, its subscriber keeps the service it opened, with those
  types: a publisher restarted with different types is refused by iceoryx2, and the recorder's types win until it
  stops.
- **Visible is not durable**, exactly as in tickertape's [crash contract](../../docs/crash-contract.md): a recorded
  message is visible to readers as soon as it is written and survives the recorder being killed, but it is durable
  only after the next checkpoint (and on SIGINT/SIGTERM). Readers that must not act on records a power loss could take
  back use `durable_only` (`follow --durable-only`).
- **Restarts.** A recorder restarted on the same journal continues it after recovery as a new recording run, with no
  gap or duplicate in sequence numbers. Messages published while no recorder runs are not recorded. If the service
  keeps history, a reconnecting recorder receives it again and records it again; since counters are tracked per run,
  these repeats are not reported as losses, but the counters in the recorded messages show them. Recovery after a
  crash zeroes the unused rest of the active segment; with the default 128 MiB segments that takes a moment, so
  `--segment-size` can be lowered for fast restarts.
- **Replay** creates the target services with the recorded types and settings, or opens them if they exist; an
  existing service with other types is refused, and so is a second recorded descriptor with other types for the same
  target (a type change between recording runs, or two `--map` rules onto one name). Names are prefixed with `replay/`
  by default so that a replay can't feed a recorder of the original service; `--original-names` replays onto the
  recorded names. While a writer holds the journal, a target name the journal is recording (a descriptor in its newest
  segment) is refused unless `--allow-live-targets` is given, and the replay ends at the journal's committed end when
  it started. Paced replay schedules each message from the first one by the recorded receive times divided by
  `--speed` (from 0.000001 to 1,000,000,000), sleeps on a monotonic clock and spins for the last millisecond; it
  reproduces gaps between recording runs and forward jumps of the recording clock in real time (scaled), and reports
  late sends (scheduled versus actual) without dropping them. Messages sent before a subscriber has connected are
  lost unless the service keeps history: `--wait-subscribers` waits for subscribers first, and the replayer keeps its
  publishers open 500 ms after the last message (`--linger-ms`).
- **Seeking is by sequence number.** There is no seek by time yet (`--from` takes a sequence number).

### Known limitations

- **User headers through a zero-sized reference.** For a runtime-typed service iceoryx2 0.10 hands out the user header
  only as a reference to a zero-sized marker type, with no accessor that yields a pointer or slice derived from the
  sample's chunk. Like upstream's own `iox2 service record`, `replay`, `publish` and `subscribe`, the `dynamic` module
  accesses the header bytes through a pointer cast from that reference. The pointer's provenance is the shared-memory
  mapping, but the reference covers zero bytes: the Tree Borrows aliasing model accepts such accesses, Stacked Borrows
  doesn't, and Rust hasn't settled its model. Services without a user header never take this path. A supported API is
  in the requests drafted for upstream.
- **A padded type published from the recorder's own process.** The `dynamic` recorder reads every byte of a sample.
  Padding written by a publisher in the *same* process may be uninitialised in Rust's abstract machine, so don't
  record padded types with the `dynamic` recorder from the process that publishes them (other processes, and the
  typed recorder, are fine).
- **`iox2 service replay` 0.10 mis-sizes slices** whose elements are wider than one byte: it loans the byte count as
  an element count, so subscribers see too many elements, the extra ones unwritten. Exported files of fixed-size or
  byte-slice services replay correctly with it; replay others with `tickertape-iox2 replay`.

## iceoryx2 recording files

`tickertape-iox2 export` writes one service (choose it with `--service` when the journal has several) to a file that
`iox2 service replay` and the `iceoryx2-userland-record-and-replay` crate read, in either representation. The file
is written under a temporary name and appears only when complete. `import` appends a **human-readable** file written by
`iox2 service record` to a journal as a new recording run.

- **Import is all or nothing and uses a parser of this crate's own.** Upstream's readers aren't meant for untrusted
  files (the text reader panics when a multi-byte character crosses a fixed byte index; the `iox2dump` reader treats
  file bytes as an initialised Rust struct and allocates from unvalidated lengths). The importer checks UTF-8 and
  ASCII, the file format version (1), the messaging pattern (publish-subscribe), the header's type details (validity
  rules), every record's lengths against them, non-decreasing timestamps, and that the file doesn't end inside a
  record, before it appends anything. `iox2dump` files can be exported but not imported, until that can be done with a
  parser of this crate's own.
- **Journal to file loses:** absolute time (only the offset from the first exported message remains, in whole
  milliseconds; timestamps that went backwards are raised to the previous one, and counted); recording-run boundaries;
  sequence numbers; publisher ids; `venue_ts_ns`; loss records; the service settings in the descriptors. The iceoryx2
  version in the file header is the one this crate is built with. The system header (iceoryx2's per-sample header)
  isn't recorded by tickertape, so the file gets zeros there; `iox2 service replay` ignores it. Records whose
  descriptor was removed are skipped and counted.
- **File to journal loses:** absolute time (the file never had it: `local_ts_ns` is `--start-unix-ns`, by default the
  import time, plus each message's offset; the descriptor is flagged as imported); the system header bytes, and with
  them the publisher ids (stored as 0); the service settings (unknown, stored as 0).
- Message bytes and order survive both directions.

## Supported versions

- **iceoryx2 0.10.0**, pinned exactly (`=0.10.0`), because the `dynamic` feature uses API upstream marks internal.
- **Rust 1.89** or newer (iceoryx2's minimum).
- **Linux** (x86-64 and aarch64) and **macOS**, as tickertape itself; CI tests all three. iceoryx2 0.10's README
  rates its support for Linux and macOS as tier 2 ("works with a restricted security and safety feature set"); its
  target for Linux is tier 1, for macOS tier 2.
- tickertape **0.4** (format version 1).

## Maintenance

- **Tests.** `cargo test --all-features` runs the unit, property and cross-process tests (publishers, recorders, live
  readers, subscribers and the CLI as separate processes, on local shared memory). One test plays an exported file
  with the upstream `iox2 service replay`: it is skipped when `iox2` isn't on `PATH`, unless `TICKERTAPE_REQUIRE_IOX2`
  is set, which makes a missing `iox2` a failure. Pull-request CI installs `iceoryx2-cli@0.10.0` and sets it, so the
  test always runs there. CI also tests the default features and no default features.
- **Upstream changes.** A weekly, informational workflow (`.github/workflows/iceoryx2-upstream.yml`) builds and tests
  this crate against the newest iceoryx2 release and against upstream `main`; its failures don't fail the workflow.
  Against `main` it is expected to fail today: `WaitSetBuilder::new` became crate-private (it now takes the node's
  clock) and `Node::waitset_builder` replaces it (checked on 2026-11-19). Porting means creating the WaitSet with
  `node.waitset_builder()` in `src/recorder.rs`, re-checking every internal API `src/dynamic.rs` uses (and its safety
  comments), and moving the exact pin, in one reviewed change.

## Licence

MIT OR Apache-2.0, like tickertape and iceoryx2.
