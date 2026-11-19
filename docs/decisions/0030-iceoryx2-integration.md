# 0030: The v1.0 external integration is an iceoryx2 recorder and replayer

- **Status:** accepted (the owner left the choice to the maintainers on 2026-11-19)
- **Date:** 2026-11-19

## Context

The roadmap requires one complete integration with an external library or framework, "selected and named before
implementation, demonstrating recording, live consumption, replay and export". It must live outside the core
crate. The owner's direction is that tickertape becomes the default local, replayable event journal that libraries
in different languages and frameworks can integrate.

Facts about other projects below were checked on 2026-11-19 against their repositories, crates.io and their
documentation. They will go out of date; the decision records what was true then.

## Options considered

1. **[iceoryx2](https://github.com/eclipse-iceoryx/iceoryx2)** (zero-copy shared-memory publish/subscribe;
   Apache-2.0 OR MIT; version 0.10.0). All four verbs are natural, including replay back onto the bus. It is
   entirely local, runs on Linux and macOS, and needs no account, key or paid data, so CI can test everything.
   iceoryx2 already ships a small recorder (`iox2 service record` and `replay`, on the
   `iceoryx2-userland-record-and-replay` crate): one file, one service per run, no live readers of the recording,
   no rolling, retention, checksums or crash contract. An open request asks for
   [several services per recording](https://github.com/eclipse-iceoryx/iceoryx2/issues/1747). That shows demand
   for what tickertape adds. Against it: the audience is robotics and embedded more than trading; iceoryx2 is 0.x
   with breaking releases; and recording a service without knowing its Rust type uses an API upstream marks as
   internal.
2. **[Databento DBN](https://github.com/databento/dbn)** records as payloads. The best fit for trading: DBN's two
   timestamps map to tickertape's, and export to DBN's own encoders is natural. Against it: DBN is a data format,
   not a framework; recording a live feed needs a paid subscription; and the crate has had breaking changes in
   most months, in the APIs an adapter would use.
3. **[NautilusTrader](https://github.com/nautechsystems/nautilus_trader).** The largest trading audience. Against
   it: LGPL-3.0, its own event store and catalog whose crate says the API is not stable, release candidates every
   two weeks, and heavy builds.
4. **Others** (Zenoh, a rosbag2 storage plugin, a DuckDB extension or Polars plugin, a `tracing` layer,
   OpenTelemetry, Aeron, Kafka-compatible pipelines, barter-rs, hftbacktest). Each fails at least one requirement:
   networking is out of scope for tickertape, a reader-only plugin has no recording or live side, the upstream
   already records and replays, CI cannot cover macOS, or the project is inactive.

## Decision

Ship **`tickertape-iceoryx2`**: a recorder and replayer for iceoryx2 publish/subscribe services, as a library and a
small command-line tool, in `integrations/iceoryx2/`, outside the Cargo workspace's core crates and versioned
separately.

- **Record.** One journal can hold several services. Each recording run writes a service descriptor record
  (service name, payload and user-header type details, service settings) under a reserved schema id before the
  service's first message, and again at the start of every journal segment; data records carry the publisher id,
  the user header and the payload. `local_ts_ns` is the receive time; `venue_ts_ns` comes from an optional extractor
  over the user header.
- **Live consumption.** Any tickertape reader, in any language with a binding, follows the journal with no
  iceoryx2 dependency.
- **Replay.** Back onto iceoryx2 services, from any sequence number, at the recorded pace (scaled) or as fast as
  possible, with service names remapped by default so a replay cannot feed its own recorder. Late sends are
  reported.
- **Export.** Parquet through the existing CLI, and iceoryx2's own recording file format, so the integration works
  with upstream's tools instead of competing with them: export to both representations, import of the
  human-readable one (see the amendment below).
- **Typed first.** A typed recorder uses only iceoryx2's public API. The recorder for services whose type is not
  known at compile time is behind a feature flag, because it depends on an internal upstream API.

Databento DBN is the runner-up and the intended next integration after v1.0.

## Consequences

- The integration pins an iceoryx2 minor version and needs a scheduled CI job against upstream's main branch to
  catch API changes early. Its version number is independent of the core crates'.
- CI tests it on Linux x86-64, Linux aarch64 and macOS with separate processes: publisher, recorder, live reader,
  a killed and restarted recorder, replay to a subscriber with byte comparison, and a round trip through
  upstream's file format.
- Message loss between publisher and recorder cannot be detected unless the publisher numbers its messages. The
  documentation must say so; the journal's own sequence numbers only prove that nothing recorded was lost.
- The roadmap item names iceoryx2. The README's alternatives table already describes iceoryx2 as something that
  "could feed a tickertape recorder"; it should point to the integration once it exists.

## Amendment: review round 1 (2026-11-19)

Three reviews of the first implementation (pull request 13) reproduced memory-safety and integrity defects on the
replay and import paths and asked for two format questions to be settled before the record layout is frozen.
Nothing had been released, so the layout changed in place (still version 1). What was decided, and why:

- **Nothing read is trusted until validated.** One function (`layout::validate_types`, with
  `ServiceSettings::validate`) checks type sizes, alignments (a power of two up to 4096), the iceoryx2 chunk layout
  they imply (checked arithmetic, accepted by `Layout::from_size_align`), a fixed-size user header, non-zero slice
  elements and service settings against documented caps, before any value reaches iceoryx2, an allocation or
  `unsafe` code: when a descriptor is decoded or encoded, when the recorder opens a service, when the replayer opens a
  target and when a file is imported. The caps (history 65,536; buffers and borrowed samples 65,536; ports 4,096; and
  two products) are far above realistic services and keep iceoryx2's own unchecked arithmetic from overflowing.
- **Trust.** The bytes themselves can't be checked to be valid values of their types. Typed replay rebuilds every
  value through `zerocopy::FromBytes` and stays safe; replay by recorded type details is `Replayer::run_dynamic`, an
  `unsafe fn` whose contract is that the journal holds valid values (true for a recording of correct publishers that
  hasn't been altered). The CLI keeps replaying without an extra flag, as upstream's tool does, and says "replay only
  recordings you trust" in its help. The binary has one `unsafe` call for this, next to the scoped exception for
  `src/dynamic.rs`.
- **Import uses a parser of this crate's own**, reads and validates the whole file before appending anything, and
  accepts only the human-readable representation. Upstream's readers are not meant for untrusted input (a panic on
  multi-byte characters; the `iox2dump` reader treats file bytes as an initialised struct). `iox2dump` export stays.
- **Repeated descriptors.** One descriptor per run did not survive retention: removing the segment that held it made
  every reader abort for the whole journal. The recorder now follows the writer's segment rolls (the documented
  format: record sizes, 8-byte alignment, segment size) and writes every open service's descriptor before the first
  record of each new segment, so every retained segment decodes on its own. A rule by bytes appended (a descriptor
  every quarter segment, say) would have left part of the oldest retained segment undecodable; following the rolls
  leaves only records too large to share a segment with the descriptors. Readers skip, count and report records whose
  descriptor is gone instead of aborting. "Session" became "recording run", and `session_id` became `run_start_seq`
  (its value, the run's first sequence number, and its position are unchanged; the old name was flagged by code
  scanning as sensitive data in logs).
- **Growth rule.** The layout version changes only for incompatible changes. Flag bits 0 to 7 must be understood,
  bits 8 to 15 may be ignored. Reserved fields are written as 0 and ignored. Extension entries (tag, length, bytes;
  tag bit 15 = must understand) may follow a descriptor and a loss record, and sit between a data record's header and
  its user header, whose start the data record now states. A version-1 reader keeps reading records written by later
  compatible revisions.
- **Publisher identity in data records: yes.** iceoryx2's sample origin (16 bytes) is in every data record. Loss
  records already name publishers; without the id in data records, a reader can't attribute messages or check counters
  of services with several publishers, and the recorder tracks counters per publisher anyway. The growth rule would
  have allowed adding it later as an extension entry, but every recording made before that would have lacked it; 16
  bytes per message (the data record header is now 40 bytes) is the price. Imported messages have id 0 (unknown).
- **Checks against the descriptor.** A data record must have the descriptor's user header size and a payload of its
  size (or whole elements); a loss record's counter width must be the descriptor's, and its kind must match the
  modular distance between expected and received counters, so a wrapping four-byte counter is no loss.
- **Exact pin.** iceoryx2 is pinned to `=0.10.0` while internal APIs are used; the weekly job against upstream is
  informational.


## Amendment: review round 2 (2026-11-19)

The first review of round 2 reproduced that the repeated descriptors of round 1 did not hold for ordinary message
sizes: the recorder wrote the descriptors when the next *data record* would start a new segment, but a ~300-byte
descriptor usually still fitted in the old segment, so the descriptors landed at its end and the data record opened
the new segment with nothing before it (with 1,000-byte messages and 64 KiB segments, only segment 0 started with a
descriptor). Retained segments then decoded as "undescribed", and the replayer's live-target guard, which read
descriptors from the newest segment only, found no services and didn't refuse. Nothing was released; the layout stays
version 1. What was decided:

- **The placement rule is stated as what readers need:** every data and loss record has the descriptor it names in its
  own segment, before it, except a record too large to share a segment with that descriptor (`64 + align8(40 + d) +
  align8(40 + r) > segment size − 8`). New segments start with the descriptors of all open services; when they don't
  all fit with the record, with the record's own descriptor (the others follow before their next records).
- **A filler record, not a core change.** tickertape's `Writer` starts a new segment only when a record doesn't fit
  and has no call to start one, and the core stays unchanged. So when the descriptors must start a new segment and the
  first of them would still fit in the old one, the recorder first appends a filler (schema id `0x4932_0004`, all
  zeros) of exactly the space left. A filler is written only when that descriptor fits, so it is never smaller than a
  record; with less room than that, the descriptor rolls by itself. Version-1 readers already ignore unknown reserved
  schema ids, so this needs no new version; `info` reports fillers apart from data. The alternative, writing the
  descriptors and the record as one unit, would need a record format of its own or a core call to roll, and would make
  every reader unpack it.
- **No copied constants.** The recorder computes the space left with tickertape's public `format` module (frame
  length, 8-byte alignment, `record_fits`, header and trailer sizes); tests check against a real writer that a filler
  of exactly that size stays in its segment and that the next record rolls. The importer uses the same code (it used
  to write one descriptor per run).
- **The live-target guard reads the whole journal.** While a writer holds the journal, the replayer reads it once and
  refuses every target name that any descriptor names, wherever it lies. A service the recorder is still waiting for
  has no descriptor and is not known; the replay ends at the journal's end as it was at start, so that case is one
  pass, not a loop. Documented rather than solved: the journal holds nothing that names it.
- **Signals.** A run with signal handling (the default) installs, the first time, a conditional default action for
  `SIGINT` and `SIGTERM` that is armed whenever no run handles signals, so a library user's process can be stopped with
  Ctrl-C again after a run. Same-kind signals arriving before the run's signal thread has taken the previous one are
  merged (as the operating system merges pending signals), and the documentation now says so instead of promising
  that a second signal always exits.
- **Every reader checks loss records** against their descriptor (`replay` and `export` did not).

## Amendment: review round 3 (2026-11-19)

The third reviews measured that a descriptor which passed validation could still make replay allocate tens of
gigabytes, and found a few more integrity gaps. Nothing was released; the layout stays version 1. What was decided:

- **Binary (`iox2dump`) export is removed**, so neither direction supports that representation (binary import was
  removed in round 1). Two reasons, both in upstream's writer: it serialises the native memory image of its
  `RecordHeader`, including alignment padding and unused `StaticString` capacity that it leaves uninitialised (two
  exports of one journal differ in their header bytes, and the file can carry whatever was in that memory), and it
  takes any `Ok(n)` from `write` as complete, so a full volume could leave a short file that the export then linked
  into place and reported as a success. Upstream's *text* writer has the same short-write behaviour, so the
  human-readable file is now written by this crate (byte for byte what upstream writes for ordinary names, checked
  against a file upstream's recorder wrote; see round 4 for other names) with complete, checked writes and a sync, under a uniquely named temporary file created
  exclusively. The crate no longer depends on `iceoryx2-userland-record-and-replay`.
- **Memory limits in force, before any port.** iceoryx2 0.10 preallocates per publisher, with unchecked arithmetic,
  one reference counter per sample for each of up to 255 data segments (growing strategies) on the heap and one chunk
  per sample in shared memory, and substitutes its defaults for settings left at 0. The replayer and the recorder now
  read the settings the opened or created service actually has, and before creating a publisher or subscriber bound
  the sample count (1,048,576, lowered from 16,777,216, which is also the validity rule for descriptors), the
  reference counters (64 MiB) and, for publishers, the data segment (1 GiB, checked again before a slice message makes
  it grow). The limits are documented with the worst case they allow; raising them later is compatible.
- **The recorder by name refuses samples from its own process.** A same-process publisher of a padded type may leave
  padding uninitialised, which the recorder can't read safely. iceoryx2 0.10 puts the sending node's id, which encodes
  the host process id, into every sample's system header; the recorder compares it with its own before taking any
  byte view and reports a match as a per-service failure. Preferred over making the entry point `unsafe`, because the
  process id is reliably available and the check costs one comparison per sample.
- **Payload alignment overrides are accepted.** iceoryx2's public `payload_alignment` gives a payload an alignment
  above its size's (a `[u8]` aligned to 64 bytes). The payload's size no longer has to be a multiple of its alignment
  (the headers' still do); chunk arithmetic uses the rounded stride, and a data record's payload stays `elements ×
  size`, so no layout change was needed.
- **Imports are stricter**: every record line and the empty line after the last record must end with a newline, as
  upstream writes them (a truncated last line was accepted as a shorter message); an absolute time that overflows is
  refused; service and type names go through iceoryx2's own constructors before the journal is opened.
- **Typed replay checks every registration** against its descriptor before the target is opened or a cached target
  reused, and the trust statements say that equal names, sizes and alignments don't establish compatible native
  representations.

## Amendment: review round 4 (2026-11-20)

- **The memory limits count every allocation a port and its connections make.** Round 3 bounded a publisher's
  reference counters but not its connections: iceoryx2 0.10 gives every connection a used-chunk list of one byte per
  sample for each of the publisher's data segments (so `max_subscribers × 255 × samples` bytes of shared memory for a
  growing publisher), and a subscriber a heap slot per publisher and per expired connection it may keep. An existing
  target with 1,024 subscribers passed round 3's check yet would have made the replay publisher request about 8 GiB.
  The limits are now per port: 128 MiB of heap (reference counters, connection slots, history; slot sizes measured on
  iceoryx2-internal types and counted with a margin), 512 MiB of connections (counted in 64 KiB pages, the largest
  page size of the supported targets) and, for a publisher, the 1 GiB data segment. The heap limit was raised from
  round 3's 64 MiB of counters so that services such as the publisher example (8 subscribers, buffers of 4,096) still
  replay as slices.
- **The recorder bounds what publishers advertise only where iceoryx2 lets it.** A subscriber's connection is sized
  by the publisher's advertised sample count and data segments. Those are public in the service's dynamic
  configuration, and the recorder checks them just before creating its subscriber, which is when it creates the
  connections to the publishers already there. A later publisher creates its connections before it registers, and
  iceoryx2 0.10 gives a subscriber no way to decline one, so the recorder can't bound those; the README says so,
  with the one race in which the recorder itself creates such a connection.
- **Typed replay accepts a raised payload alignment again.** A registered Rust type accepts a recorded alignment that
  is a power of two not below its own (iceoryx2 accepts a service alignment above the type's); name and size must
  still match. An existing or opened target still needs at least the recorded alignment.
- **Exports**: the temporary file name no longer contains the destination's name (so a destination name of 255
  bytes works), and the directory is synced after the link. The written file has upstream's bytes for names without
  quotes, backslashes or control characters; other names are written as double-quoted TOML strings with `\uXXXX`
  escapes where upstream's serialiser may choose short escapes or single quotes, which both parsers read alike.
- **`fork`**: the same-process check compares process ids stamped by the sending node, so it relies on iceoryx2
  handles not being used across `fork`, as iceoryx2 requires; stated where the precondition is.
