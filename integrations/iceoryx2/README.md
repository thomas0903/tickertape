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
what was imported). Nor can equal type names, sizes and alignments, which is all iceoryx2 compares, establish that a
subscriber's type has the recording publisher's native representation (field order, byte order, a type changed under
the same name, another architecture). The CLI says so in `replay --help` and `import --help`; the library makes the
run-time-typed replay an `unsafe fn` (below).

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

A service registered with `typed::<P, H>` must have been recorded with the same type names and sizes (payload and
user header) as `P` and `H`. Its recorded alignment may be higher than the Rust type's, if it is a power of two: a
service whose payload alignment was raised with iceoryx2's `payload_alignment` (a `[u8; 64]` aligned to 64) replays
with the type's natural alignment registered. The target, when it exists, needs at least the recorded alignment (an
existing target with the same override qualifies); a target the replayer creates gets the Rust type's alignment, so
an aligned recording needs its target created beforehand, or `tickertape-iox2 replay`, which creates targets with
the recorded alignment.

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
| `0x4932_0004` | filler: zeros that end a segment early (see [where descriptors lie](#where-descriptors-lie)) |

### Growth rule

- **Layout version** (in every descriptor, data and loss record; a filler has neither field) changes only for
  incompatible changes. A reader rejects a version it doesn't know.
- **Flags** (`u16`, in every descriptor, data and loss record): bits 0 to 7 are *required*, and a reader rejects a record with a required bit it
  doesn't know; bits 8 to 15 are *optional*, and a reader ignores those it doesn't know. Version 1 defines one bit:
  descriptor bit 0, `imported`.
- **Reserved fields** are written as 0 and ignored by readers. A later revision may use one only for a meaning that
  readers ignoring it still decode correctly, or behind a required flag bit.
- **Extension entries**: after a descriptor's recorder name, after a loss record's 48 bytes, and between a data
  record's 40-byte header and its user header (the header length field says where the user header starts):
  `u16` tag, `u16` reserved, `u32` length, then the bytes, repeated to the end of the area. Tag bit 15 means
  required: a reader rejects an unknown required tag and skips unknown optional ones; an entry that runs past its area
  is malformed. Version 1 defines no tags.

### Recording runs

A *recording run* is one run of the recorder, or one import. Its descriptors carry `run_start_seq`, the journal's next
sequence number when the run began. A service's descriptor may appear several times in a run with the same content
(below). A data or loss record names the most recent descriptor of its service before it.

### Where descriptors lie

Retention and `tickertape archive` remove segments from the front of a journal, so a record can only be decoded on its
own if its descriptor is in the same segment. The recorder and the importer keep this rule:

**Every data and loss record has the descriptor it names in its own segment, before it**, except a record too large to
share a segment with that descriptor: one with `64 + align8(40 + d) + align8(40 + r) > segment size − 8`, where `d` is
the descriptor's length and `r` the record's (a tickertape segment has a 64-byte header, records have a 40-byte header
and are 8-byte aligned, and the last 8 bytes of a segment hold no record; [format](../../docs/format.md)). With 64 KiB
segments and a 400-byte descriptor, that is a record payload larger than 64,984 bytes; with the default 128 MiB
segments, about 128 MiB.

- **Guaranteed:** in a journal written by this crate, every data and loss record whose segment is still there can be
  decoded, including after retention or `tickertape archive` removed older segments, except that exception. Every
  segment the recorder starts begins with the descriptors of the services open at that point.
- **Not guaranteed:** that a record's descriptor is still there (journals written otherwise, or the exception). Readers
  skip, count and report such records instead of stopping: `info` (`undescribed_records`), `replay` and `export`
  (`undescribed`), and the `follow` example (`undescribed` lines).

How the recorder does it, before each record that names a descriptor (and before a service's first descriptor):

1. If the record fits in what is left of the active segment, together with its service's descriptor when that isn't
   in the segment yet, they are appended there.
2. Otherwise the record must start a new segment. The new segment begins with the descriptors of all open services, in
   the order the services were configured, then the record, if they all fit in one segment together. If they don't,
   it begins with the record's own descriptor, then the record (the other services' descriptors follow before their
   next records, by rule 1). If not even that fits, the record is too large to share a segment with its descriptor and
   is appended as it is, wherever the writer puts it (the exception).
3. tickertape's writer starts a new segment only when a record doesn't fit, and has no call to start one. So that the
   first of those descriptors starts the new segment, the recorder first appends a **filler** that takes exactly the
   space left, when that descriptor would still fit there. When it wouldn't (less space left than the descriptor
   takes, including less than the 40 bytes any record needs), the descriptor starts the new segment by itself and no
   filler is written. Fillers therefore take at least as much space as a descriptor. A filler holds at most
   tickertape's largest record payload, so in a segment larger than 4 GiB the space left can take several fillers in
   a row (each but the last of the largest size whose frame is a multiple of 8, the last taking the rest); otherwise
   it is one filler, never smaller than the smallest record.

**Space left** is the segment size minus 8, minus the bytes in use: 64 for the header plus `align8(40 + payload
length)` per record. The recorder reads the newest segment when it opens the journal and then adds every record it
appends; after a record that didn't fit, the writer has started a new segment of its configured size, and the count
starts again at 64. These rules come from tickertape's public `format` module (`frame_len`, `align8`, `record_fits`,
`SEGMENT_HEADER_LEN`, `SEGMENT_TRAILER_LEN`), not from copies, and tests check them against a real writer (a filler of
exactly the space left stays in its segment; the next record, of any size, starts a new one).

### Filler (`0x4932_0004`)

A record whose payload is all zero bytes, of any length from 0 to tickertape's largest payload. It carries no data and
names no descriptor. Fillers are the last records of their segment (usually one; several in a row only in segments
larger than 4 GiB). Readers skip them without looking at their payload. Version-1
readers written before fillers existed see an unknown reserved schema id and ignore it as well, so fillers didn't need
a new layout version. `info` reports fillers apart from data (`filler_records`, `filler_bytes`); the recorder's report
counts those it wrote (`fillers`).

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
`LossRecord::check` (and `Descriptor::encode` refuses whatever `decode` refuses), to every record in every reader
(`info`, `replay`, `export`, `follow`; loss records too, although only `info` and `follow` use them), before recording
a service, and before importing a file.

- Layout version 1; descriptor magic `TTI2`; no unknown required flag bit or extension tag; every extension entry
  inside its area; strings valid UTF-8; nothing missing.
- Enumerations as in the tables: type variant 0 or 1; messaging pattern 1; safe overflow 0 or 1; counter location 0,
  1 or 2 with width 0 exactly when the location is 0 (otherwise 4 or 8) and offset 0 without a counter; loss kind 1
  or 2.
- Each type: alignment a power of two from 1 to 4096; size at most 4,294,967,254 (the largest tickertape record
  payload); name at most 256 bytes. The user header's and system header's sizes are multiples of their alignments;
  the payload's need not be: iceoryx2's public `payload_alignment` raises a payload's alignment without changing its
  size (a `[u8]` aligned to 64 has element size 1 and alignment 64). iceoryx2 then reserves the size rounded up to
  the alignment (the *stride*) per element, but the payload bytes, and a data record's payload, are `elements × size`. The user header and system header are fixed-size. A
  slice's element size is not 0: every element count of zero-sized elements has the same bytes, so such services are
  refused. A data record with the user header and one element fits in a record.
- The iceoryx2 chunk the types imply for the largest element count a record can hold (system header; user header
  aligned to its alignment; payload aligned to its alignment; element size rounded up to the payload's alignment
  times the count; all rounded up to the largest alignment), computed without overflow, is a valid Rust `Layout`.
- Service settings at most: `history_size` 65,536; `subscriber_max_buffer_size` and the recorder's buffer 65,536;
  `subscriber_max_borrowed_samples` 65,536; `max_publishers`, `max_subscribers` and `max_nodes` 4,096;
  `max_subscribers × (buffer + borrowed) + history_size` (samples one publisher preallocates) 1,048,576;
  `max_publishers × max_subscribers × (buffer + borrowed)` (queue slots) 4,294,967,296. These keep iceoryx2's own,
  unchecked arithmetic far from overflow; a running service outside them is refused (and retried) by the recorder.
- Service name 1 to 255 bytes.
- Counter: `offset + width` inside the user header, or inside one payload element.
- Data record: `H` at least 40; the user header inside the record; user header length equal to the descriptor's user
  header size; payload length equal to the payload size, or a multiple of the element size for a slice.
- Loss record: width equal to the descriptor's, which has a counter; expected and received fit in the width and
  differ; the kind matches the distance rule.

The normative description, with tests that pin every offset, is the rustdoc of [`src/layout.rs`](src/layout.rs).

### Memory a replay target or a recorder subscriber may cost

Not a decoding rule: caps on the service settings this tool hands to iceoryx2, and upper estimates of what iceoryx2
0.10.0 then allocates, which the replayer and the recorder check before they create a publisher or subscriber.
iceoryx2 itself enforces none of these limits. They apply to the service's settings *in force*: those of the service
as opened or created (a recorded 0 means iceoryx2's default, from the node's configuration), not the recorded ones.
The settings are checked against the caps of the [validity rules](#validity-rules) before anything iceoryx2 sizes by
them (with the one exception of reading an existing service's settings, and in the order given under *What is
checked before what* below), and the estimates (formulas in [`src/layout.rs`](src/layout.rs), with checked arithmetic) cover what iceoryx2
0.10.0 allocates, with unchecked arithmetic, when a port and its connections are created and while it runs.

The estimates are checked against what iceoryx2 really allocates, not only against a reading of its source:
[`tests/allocation.rs`](tests/allocation.rs) creates ports on a real iceoryx2 node and measures the heap that goes
through Rust's global allocator (a counting global allocator: the bytes the port leaves live) and the sizes of the
shared-memory objects named after the port (the files under `/dev/shm` on Linux; on macOS, where iceoryx2 0.10.0
keeps a state file `/tmp/<name>.shm_state` per object, naming a POSIX shared-memory object that has no path, the
objects as iceoryx2 lists and opens them). iceoryx2's direct `malloc` users, which that allocator can't see, are
listed in the formula from its source: on the publish-subscribe paths, only the subscriber's two connection lists
(16 bytes per `max_publishers` and 8 per expired-connection slot, `port/subscriber.rs` near lines 288-306). It
asserts two things:

- *No term is missing*: for small, representative shapes (fixed-size and `[u8]` services; static and growing
  publishers; 0, 8 and 32 publishers present when a subscriber is created, and 8 and 32 that connect after it; 0, 8
  and 32 subscribers present when a publisher is created; history 0 and 3; a slice publisher whose data segment grew
  twice and three times with a history of 3) and for one shape per port kind with a raised setting (a publisher
  allowing 1,024 subscribers, 896 if growing; a subscriber allowing 200 publishers), each estimate is at least what
  was measured. At these shapes the totals are dominated by the fixed parts and by empty slots, so this alone
  doesn't show that each per-item constant is large enough.
- *Each per-item coefficient is bounded*: for each quantity the budget charges per item (a subscriber's heap per
  growing publisher connected, per connection slot and per expired-connection slot; a publisher's heap per
  subscriber slot, per buffer sample and per history sample; a data segment's bytes per sample; a connection's bytes
  per connection, per publisher sample and per buffer entry), the port is measured at two values and the measured
  cost per item must be at most the formula's constant; where the constant is meant to be tight, also at least a
  stated fraction of it (90% for sizes taken exactly from the source, 50% for measured sizes counted with a margin
  under 2×; a subscriber's connection slot, counted at about 2.2× what is measured, is bounded from above only,
  and a connection, counted in whole 64 KiB pages, is bounded from above by that figure and from below by the
  bytes each connection must hold).

The figures at the caps below (128 MiB of heap, 1.5 GiB of shared memory) are the formulas evaluated at the caps, not
measurements. The test runs on macOS and Linux (CI: x86-64 and aarch64);
`cargo test --all-features --test allocation -- --nocapture` prints both tables.

A publisher preallocates `samples = max_subscribers × (buffer + borrowed) + history_size + loaned samples` (the
replayer's publishers loan 2) and has `segments` data segments: 1 with a static allocation strategy (the replayer's,
for fixed-size payloads), 255 with a growing one (the replayer's power-of-two strategy for slices). `samples` is at
most 1,048,576.

| Port | Where | What | Limit |
|---|---|---|---|
| Replay publisher | heap | an 8-byte reference counter per sample for each data segment (`segments × samples × 8`), a connection slot per possible subscriber (`max_subscribers × 2 KiB`), a history entry per history sample (`history_size × 32`), 1 MiB fixed | 128 MiB |
| Replay publisher | shared memory | one connection per subscriber (`max_subscribers ×` the connection below) | 512 MiB |
| Replay publisher | shared memory | **every data segment that can be alive at once**: the first, `samples × first chunk size`; for a growing strategy, one more for every power of two above the first chunk size up to the largest sample's chunk size rounded up to a power of two (a larger sample makes iceoryx2 create a segment of such chunks, and an old segment stays while a sample in it is referenced: by the history, a subscriber or a loan), and the management object of the segments (64 KiB). Each segment adds the alignment, `4 × (samples + 1) + 3` bytes of pool index and a 4 KiB storage header, and takes whole 64 KiB pages | 1 GiB in all |
| Recorder subscriber | heap | a connection slot for every publisher the service allows and every expired connection it may keep, each with a growing publisher's segment table (`(max(subscriber_expired_connection_buffer, borrowed) + max_publishers) × (8 KiB + 384 KiB)`), the two lists iceoryx2 allocates with `malloc` directly (`max_publishers × 16 + max(subscriber_expired_connection_buffer, borrowed) × 8`), 1 MiB fixed | 128 MiB |
| Recorder subscriber | shared memory | the connections it creates, one per publisher present, sized by what each **publisher** advertises | 512 MiB |

A connection (one publisher to one subscriber) holds a used-chunk list of one byte per sample for each of the
publisher's data segments (`segments × samples`), a submission queue (`8 × (buffer + 1) + 7`), a completion queue
(`8 × (buffer + borrowed + 1) + 7`) and fixed parts (4 KiB, and 64 bytes per segment), counted in whole 64 KiB pages
(each connection and data segment is a shared-memory object of its own; 64 KiB is the largest page size of the
supported targets).

The per-slot sizes are those of iceoryx2 0.10.0's internal types, measured on macOS and Linux aarch64 (the constants
in [`src/layout.rs`](src/layout.rs) say so next to each value) and counted with a margin: 1,304 bytes per subscriber
slot of a publisher, 16 per history entry, about 330 KB of fixed publisher heap with 255 segments, 3,680 to 3,824
bytes per connection slot of a subscriber, and about 302 KB per connection to a publisher with a growing data
segment: the subscriber's view of that segment allocates a table of 255 segment entries at once (static publishers
add about 70 bytes). The last one is why the recorder's heap counts `max_publishers`, not the publishers present:
see below.

Examples. An existing slice target with 1,024 subscribers, buffer 30 and 2 borrowed samples has 32,770 samples and
63.75 MiB of reference counters, but its publisher's 1,024 connections would take 8 GiB of used-chunk lists (255 ×
32,770 bytes each): refused. 8 subscribers with buffers of 4,096 (the publisher example's) fit for slices: 63.8 MiB of
reference counters and 64.5 MiB of connections; its data segments take 129.8 MiB with the first 4 KiB chunks, 386 MiB
once a message has needed 8 KiB chunks and 899 MiB after 16 KiB ones, and a message that needs 32 KiB chunks (1.9 GiB
in all) is refused. A static segment of 32,764 chunks of 32,768 bytes came to exactly 1 GiB without its storage
header, and is refused with it. A service the recorder records may allow at most 331 connection slots: with the
default expired-connection buffer of 128, at most 203 publishers (the segment tables of 2,048 publishers alone would come to
about 590 MiB, at the 302 KB measured for each). The recorder's refusal says how many publishers would fit with the
node's expired-connection buffer and the service's other settings, and names the 203 of the default buffer.

The replayer checks the data segments when it creates a publisher (its first slice holds about 4 KiB) and, for
slices, before each message larger than the first chunks, which may make iceoryx2 create another segment; a message
over the limit stops the replay with an error.

**What the recorder can and cannot bound.** iceoryx2 sizes each connection by what the publisher advertises (its
sample count and data segments; `Connection::new` in `port/details/receiver.rs`), not by the recorder's settings, and
gives a subscriber no way to decline a publisher. The recorder first checks the service's settings and its own heap,
from the settings alone; only then does it list the publishers present (iceoryx2 allocates that list in proportion to
`max_publishers`) and check the connections it will create to them, sized by what each of them advertises, just
before it creates its subscriber. A service over a limit is refused (reported and retried).

- *Heap: bounded, for publishers present and later ones alike.* A publisher that connects after the subscriber exists
  costs the subscriber a connection slot and, if its data segment can grow, the 255-entry segment table, on the
  subscriber's next receive, and the recorder can't decline it. So the check counts both for every publisher the
  service allows (`max_publishers`) and for every expired connection (which keeps its table), whether or not those
  publishers exist yet. Fixed-size services are not exempt: a fixed-size publisher takes its allocation strategy
  from its own process's iceoryx2 configuration (`publisher_allocation_strategy`), which may be growing, and iceoryx2
  then gives it a growing data segment, and the subscriber the full table, though its payload never grows.
- *Shared memory of connections to later publishers: not bounded.* Each port takes its snapshot of the other side's
  ports before it registers itself (`Publisher::new` in `port/publisher.rs` near line 587, `Subscriber::new` in
  `port/subscriber.rs` near line 286), and whichever of the two refreshes its connections first creates the
  connection between them. Normally a publisher created later sees the recorder's subscriber in its snapshot and
  creates the connection itself; the recorder only maps it. But a publisher that registers after the recorder's
  check, and whose own snapshot was taken before the recorder's subscriber registered, may be missed by both
  snapshots, or seen by the subscriber's; the subscriber then creates the connection to it, at creation or on its
  next receive (if that comes before the publisher refreshes), sized by that publisher's values, which the recorder
  never checked. Their number is bounded by the connections the subscriber can hold at once, which iceoryx2 sizes
  as `max_publishers` active ones plus `max(subscriber_expired_connection_buffer, subscriber_max_borrowed_samples)`
  expired ones (`Subscriber::new` near line 280; 1 + 128 for a service allowing one publisher, with iceoryx2's
  defaults), the same slots the heap check counts. `max_publishers` alone does not bound them: it limits the
  publishers registered at one time, and a publisher that leaves unread data behind keeps its connection in the
  expired buffer (`prepare_connection_removal` in `port/details/receiver.rs` near line 260). With `max_publishers =
  1`, two publishers that both took their snapshot before the recorder's subscriber registered can register one
  after the other; if the first sends and exits between two polls of the recorder, its connection stays while the
  subscriber creates the one to the second. Their size is not bounded: a publisher's sample count includes its own
  loaned samples, which the service doesn't limit. Publishers' data segments are theirs; the recorder maps them and
  allocates none.
- *Before the check.* To read an existing service's settings at all, iceoryx2 lists the service's nodes when it
  reads its details or opens it (two vectors in proportion to `max_nodes`, 24 bytes a slot, plus each registered
  node's state), before any setting can be seen through its public API. The recorder recording by name reads the
  details, checks the settings and only then opens the service; the typed recorder opens it and checks the settings
  right after. Either way the nodes are listed once before a refusal; a refused service is attempted again on each
  retry, and the memory is freed after each attempt.

**What is checked before what, for a replay target.** For a target that exists, the replayer reads its details (the
one listing of its nodes above) and checks, in this order and without opening it: the settings against the caps,
the types against the recording, and what its publisher would allocate (heap, connections, first data segment). A
target refused by any of these costs that one listing. Only when all of them pass does it open the target
(iceoryx2's clean-up on open lists the nodes again), check the settings in force once more (the target may have been
replaced meanwhile), and create its publisher. A target that doesn't exist yet is created with the recorded settings
and, for each setting the recording leaves at 0, the default of the node's iceoryx2 configuration
(`defaults.publish_subscribe`, which may come from a configuration file); iceoryx2 sizes the new service's shared
configuration by them (`max_publishers`, `max_subscribers`, `max_nodes`) when it creates it. So those settings, as
iceoryx2 will create them, are checked against the caps before the service is created: a configured default over a
cap (say `max_publishers = 40000`) is refused and nothing is created. The publisher's budget is checked after the
service is created, against the settings in force. The recorder creates a missing service the same way (with its
buffer as `subscriber_max_buffer_size`), with the same check first.

So one replay target costs at most 128 MiB of heap and 1.5 GiB of shared memory: 512 MiB of connections and 1 GiB
for all its data segments together, during and after growth. The recorder's subscriber of one service costs at most
128 MiB of heap, including what later publishers cost it, and 512 MiB of connections it creates to the publishers
present at its check (the race above excepted), whatever a journal says. These figures are the formulas evaluated at
the caps. What backs the formulas: `tests/allocation.rs` shows that no term is missing (each estimate covers what the
shapes above measured, the grown slice publisher and the subscribers with publishers connecting later among them) and
bounds each per-item coefficient at small shapes; the unit tests `retained_data_segments_count` and
`data_segment_headers_count` in `src/layout.rs` cover the data segments' sum and headers; the direct `malloc` lists
come from iceoryx2's source. A target, or a service to record, over these limits is refused with the quantity and the
limit. Raising them later is compatible; lowering them would refuse recordings that replay today.

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
- **Replay** creates the target services with the recorded types and settings, or opens them if they exist; a target
  whose settings in force exceed the [memory limits](#memory-a-replay-target-or-a-recorder-subscriber-may-cost) is
  refused before its publisher is created, and so is an existing service with other types, and so is a second recorded descriptor with other types for the same target (a
  type change between recording runs, or two `--map` rules onto one name). Names are prefixed with `replay/` by default
  so that a replay can't feed a recorder of the original service; `--original-names` replays onto the recorded names.
  While a writer (a recorder or an import) holds the journal, a target name that any descriptor in the journal names is
  refused unless `--allow-live-targets` is given: the replayer reads the whole journal once, before replaying, to learn
  them, so it doesn't matter which segments hold the descriptors. A service the recorder is still waiting for (it hasn't
  appeared yet) has no descriptor and isn't known, so a replay that creates it is recorded; the replay still ends at the
  journal's committed end when it started, so that is one pass, not a loop. Paced replay schedules each message from the
  first one by the recorded receive times divided by `--speed` (from 0.000001 to 1,000,000,000), sleeps on a monotonic
  clock and spins for the last millisecond; it reproduces gaps between recording runs and forward jumps of the recording
  clock in real time (scaled), and reports late sends (scheduled versus actual) without dropping them. Messages sent
  before a subscriber has connected are lost unless the service keeps history: `--wait-subscribers` waits for
  subscribers first, and the replayer keeps its publishers open 500 ms after the last message (`--linger-ms`).
- **Signals.** By default (`handle_signals(true)`; the CLI always), while a recording or replay runs, the first
  `SIGINT` or `SIGTERM` its signal thread takes stops it cleanly (the recorder takes a final checkpoint); a signal
  taken after that ends the process at once with status 130 (`SIGINT`) or 143 (`SIGTERM`), without a final
  checkpoint. Signals of one kind that arrive before the thread has taken the previous one are merged into one (the
  operating system keeps at most one pending signal of each kind), so two of the same kind sent back to back can stop
  cleanly instead of ending the process; a `SIGINT` and a `SIGTERM` are never merged. When the run returns, and no
  other run in the process handles signals, both signals take their default action again (they end the process): the
  first run installs, for the rest of the process's life, an action that emulates the default whenever no run handles
  signals. Handlers the process installs for these signals, before or after the first run, don't prevent that (one
  installed before, or later through signal-hook, still runs, and then the process ends; one installed later with
  `sigaction` directly replaces this action instead), and an inherited "ignore" disposition (`SIGINT` in a background
  job of a non-interactive shell, or under `nohup`) is replaced too. So a process that handles or ignores these
  signals itself should use `handle_signals(false)` and the stop flag.
- **Seeking is by sequence number.** There is no seek by time yet (`--from` takes a sequence number).

### Known limitations

- **User headers through a zero-sized reference.** For a runtime-typed service iceoryx2 0.10 hands out the user header
  only as a reference to a zero-sized marker type, with no accessor that yields a pointer or slice derived from the
  sample's chunk. Like upstream's own `iox2 service record`, `replay`, `publish` and `subscribe`, the `dynamic` module
  accesses the header bytes through a pointer cast from that reference. The pointer's provenance is the shared-memory
  mapping, but the reference covers zero bytes: the Tree Borrows aliasing model accepts such accesses, Stacked Borrows
  doesn't, and Rust hasn't settled its model. Services without a user header never take this path. A supported API is
  in the requests drafted for upstream.
- **The recorder by name refuses samples from its own process.** It reads every byte of a sample, and padding written
  by a publisher in the *same* process may be uninitialised in Rust's abstract machine. So before it views any byte,
  it compares the process id in the sending node's id (in the sample's system header; iceoryx2 0.10 builds node ids
  from the host process id) with its own, and refuses a sample from its own process as a per-service failure
  (reported and retried). Record such services from another process, or with the typed recorder, which only accepts
  types without padding. Samples from other processes arrive as their publishers wrote them into shared memory. The
  check needs iceoryx2 handles not to be used across `fork`, as iceoryx2 requires: a child process that keeps using
  a node or publisher inherited from its parent stamps the parent's process id, so a recorder started in that child
  would not recognise those samples as its own.
- **`iox2 service replay` 0.10 mis-sizes slices** whose elements are wider than one byte: it loans the byte count as
  an element count, so subscribers see too many elements, the extra ones unwritten. Exported files of fixed-size or
  byte-slice services replay correctly with it; replay others with `tickertape-iox2 replay`.

## iceoryx2 recording files

`tickertape-iox2 export` writes one service (choose it with `--service` when the journal has several) to a
human-readable file that `iox2 service replay` and the `iceoryx2-userland-record-and-replay` crate read. It is written
by this crate, with every write checked to be complete, synced, under a temporary name of its own in the
destination's directory (a short fixed prefix and unique numbers, created exclusively, so concurrent exports never
share one and any valid destination name works), and linked into place only when complete; an existing file is never
replaced. A successful export has synced the file and the directory entry of its name, so both survive a power loss,
**provided the path of the output directory was not re-pointed while the export ran** (a symbolic link on the way
switched to another directory); before it returns, a power loss may lose the name, never leave a partial file under
it. Without `openat`/`linkat` (not available without `unsafe` or another dependency) the export finds the directory
by its path: it opens it before creating anything, remembering its device and inode, and looks it up again by the
same path after the link. The same directory is synced and the export succeeds; a different directory (or none that
opens) is a change the export can see, so it syncs every directory handle it holds and fails with an error saying
that the file is complete but its name may not survive a power loss because the output directory changed during
the export, whichever directory took the name. A change it can't see (re-pointed and back again between the two
lookups) is outside what a path-based export can detect. For service and type names
without quotes, backslashes or control characters the file has the same bytes upstream's recorder writes (a test
compares them with a file it wrote); other names are written as double-quoted TOML strings with `\uXXXX` escapes for
control characters, where upstream's serialiser may choose short escapes (`\n`) or single-quoted strings: different
bytes, both valid TOML that both parsers read alike. `import` appends a
human-readable file written by `iox2 service record` to a journal as a new recording run. The `iox2dump` (binary)
representation is not supported in either direction: upstream's writer puts the memory image of its header struct,
uninitialised padding included, into the file and takes a short write as complete (ADR 0030, review round 3).

- **Import is all or nothing and uses a parser of this crate's own.** Upstream's readers aren't meant for untrusted
  files (the text reader panics when a multi-byte character crosses a fixed byte index; the `iox2dump` reader treats
  file bytes as an initialised Rust struct and allocates from unvalidated lengths). The importer checks UTF-8 and
  ASCII, the file format version (1), the messaging pattern (publish-subscribe), the header's type details (validity
  rules), the service and type names with iceoryx2's own constructors (as replay and export use them), every record's
  lengths against the types, non-decreasing timestamps whose absolute time (`--start-unix-ns` plus the offset) fits in
  64 bits, and that every line of every record, and the empty line after it, ends with a newline as upstream writes
  them (a file cut anywhere is refused), before it appends anything. It holds the whole file, and all its decoded
  messages, in memory while it does.
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
  comments), the allocation formulas behind the [memory limits](#memory-a-replay-target-or-a-recorder-subscriber-may-cost)
  and the file format, updating `ICEORYX2_VERSION` in `src/upstream.rs`, and moving the exact pin, in one reviewed
  change.

## Licence

MIT OR Apache-2.0, like tickertape and iceoryx2.
