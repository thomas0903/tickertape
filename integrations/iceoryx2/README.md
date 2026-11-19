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
iox2 service publish demo/hello -m "68 65 6c 6c 6f" -t 100 -r 0 &    # a running publisher: "hello" every 100 ms
tickertape-iox2 record /tmp/iox2-demo --service demo/hello &          # record it
cargo run --manifest-path integrations/iceoryx2/Cargo.toml --example follow -- --journal /tmp/iox2-demo --count 5
kill %2                                                               # stop recording (SIGTERM: final checkpoint)
tickertape-iox2 replay /tmp/iox2-demo --speed 1 --wait-subscribers 1 & sleep 1 && iox2 service subscribe replay/demo/hello -m 5
tickertape export /tmp/iox2-demo --format parquet --output /tmp/iox2-demo.parquet
tickertape-iox2 export /tmp/iox2-demo --output /tmp/iox2-demo.iox2   # for iox2 service replay
```

The `follow` example (step 5) uses only the `tickertape` crate and the documented layout below; it builds without
iceoryx2 (`cargo build --example follow --no-default-features`). `tickertape-iox2 info /tmp/iox2-demo` summarizes the
recording. Every command has `--help`, with exit codes and the `--json` schema.

`iox2 service replay` needs the target service to exist with the recorded types (it doesn't create it), for
example while the original publisher runs: `iox2 service replay -i /tmp/iox2-demo.iox2 demo/hello`.

## Library

```rust,ignore
use tickertape_iceoryx2::recorder::{Recorder, TypedService};
use tickertape_iceoryx2::replayer::Replayer;

// Record: a typed service (public iceoryx2 API only), with the event time read from the user header.
let report = Recorder::builder("/tmp/ticks")
    .typed(TypedService::<Tick, TickHeader>::new("market/ticks").venue_ts(|h| h.venue_ts_ns))
    .build()?
    .run()?; // until SIGINT/SIGTERM, a stop flag or a limit

// Replay onto "replay/market/ticks" at the recorded pace.
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

### Features

| Feature | Default | What it adds |
|---|---|---|
| `iceoryx2` | yes | The typed recorder and replayer. iceoryx2's public API only. |
| `dynamic` | no | Services whose types are only known at run time (what `iox2 service record` does), and conversion to and from iceoryx2's recording files. |
| `cli` | no | The `tickertape-iox2` binary (enables `dynamic`). |

**The `dynamic` trade-off.** Recording a service by name, without its Rust type, needs iceoryx2 API that upstream
marks internal and hides from its documentation (`__internal_set_payload_type_details`, `loan_custom_payload`,
`TypeDetail::__internal_new_from_parts`). The upstream CLI uses the same API. An iceoryx2 minor release may change it
without notice; all such use is in one module, [`src/dynamic.rs`](src/dynamic.rs), which is also the only module with
`unsafe` code. The command-line tool needs this path (it records services by name), so it enables the feature.
Library users who know their types can stay on the default features.

## Payload layout

A recording is an ordinary tickertape journal. Records are told apart by schema id; ids `0x4932_0000` to
`0x4932_FFFF` are reserved for this crate. All integers are little-endian; strings are UTF-8 with a `u16` length in
front. A decoder must reject unknown layout versions, non-zero reserved fields and trailing bytes.

The record's own timestamps: `local_ts_ns` is when the recorder received the message (system clock, nanoseconds since
the Unix epoch); `venue_ts_ns` is what an optional extractor read from the user header, or 0.

**Descriptor** (schema id `0x4932_0001`), one per service per recording session, before that service's first
message:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | magic `TTI2` |
| 4 | 2 | layout version, 1 |
| 6 | 2 | flags: bit 0 = imported from an iceoryx2 file (times reconstructed); others 0 |
| 8 | 8 | session id: the journal's next sequence number when the recorder opened it |
| 16 | 8 | session start, ns since the Unix epoch |
| 24 | 6 | iceoryx2 version (major, minor, patch; `u16` each) |
| 30 | 1 | messaging pattern: 1 = publish-subscribe |
| 31 | 1 | safe overflow: 1 = a full subscriber buffer drops its oldest sample |
| 32 | 8 | `history_size` |
| 40 | 8 | `subscriber_max_buffer_size` |
| 48 | 8 | `subscriber_max_borrowed_samples` |
| 56 | 8 | `max_publishers` |
| 64 | 8 | `max_subscribers` |
| 72 | 8 | `max_nodes` |
| 80 | 8 | the recorder subscriber's buffer size (0 = unknown) |
| 88 | 1 | publisher counter location: 0 none, 1 user header, 2 payload |
| 89 | 1 | publisher counter width: 0, 4 or 8 bytes |
| 90 | 2 | reserved, 0 |
| 92 | 4 | publisher counter offset in bytes |
| 96 | | three type details: payload, user header, iceoryx2 system header |
| | | service name (`u16` length + UTF-8) |
| | | recorder (`u16` length + UTF-8), e.g. `tickertape-iceoryx2 0.1.0` |

Each type detail (iceoryx2's `TypeDetail`): variant (1 byte: 0 fixed size, 1 dynamic, i.e. a slice), reserved (1
byte, 0), type name length (`u16`), reserved (4 bytes, 0), size (`u64`, of one element for a slice), alignment
(`u64`, a power of two), then the type name.

**Data record** (schema id `0x4932_0002`), one per message:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | layout version, 1 |
| 2 | 2 | flags, 0 |
| 4 | 4 | user header length `h` |
| 8 | 8 | sequence number of this service's descriptor |
| 16 | `h` | user header bytes |
| 16 + `h` | rest | payload bytes |

Bytes are copied exactly as iceoryx2 holds them in shared memory, padding included; neither part is aligned inside the
record. A slice payload holds `payload length / element size` elements. Because each data record names its
descriptor's sequence number, a reader that starts anywhere seeks to it and decodes, with no scan from the start.

**Loss record** (schema id `0x4932_0003`, 48 bytes), written before the message that revealed a gap in a configured
publisher counter: layout version (`u16`, 1), kind (`u16`: 1 gap, 2 backwards), reserved (`u32`, 0), descriptor
sequence number (`u64`), iceoryx2 publisher id (`u128`), expected counter (`u64`), received counter (`u64`).

The normative description, with tests that check every offset, is the rustdoc of [`src/layout.rs`](src/layout.rs).

## Limits

- **Loss before the recorder is invisible to tickertape's sequence numbers.** They prove that nothing the recorder
  *wrote* was lost; they say nothing about messages it never received. iceoryx2 delivers into a bounded subscriber
  buffer: the recorder asks for 1,024 slots (`--buffer-size`), capped by the service's `subscriber_max_buffer_size`,
  whose iceoryx2 default is **2**. When the buffer is full, a service with safe overflow (iceoryx2's default) replaces
  the oldest unread sample and the recorder never sees it; without safe overflow, the publisher's backpressure
  strategy decides. The recorder drains its buffers every poll interval (1 ms by default; the interval is a lower
  bound, and on Linux iceoryx2's WaitSet can wake noticeably less often), and it stops draining while a checkpoint
  syncs to disk, so the buffer must hold everything that arrives in that time. With iceoryx2's default of 2 slots,
  expect loss even at a few hundred messages per second; create services with a larger `subscriber_max_buffer_size`
  (or let the recorder create them) when you record them. `record` prints, and
  each descriptor stores, the buffer size it got and the overflow policy.
- **Make loss detectable with a counter.** If each publisher puts a counter in its user header or payload (an
  unsigned little-endian integer, +1 per message), pass its position (`--counter header:0:8`, or
  `TypedService::counter`). The recorder then writes a loss record for every gap and reports gaps and missing
  messages per service. Counters are tracked per publisher and per session; a gap across a recorder restart is not
  reported, but the counters in the recorded messages show it.
- **Visible is not durable**, exactly as in tickertape's [crash contract](../../docs/crash-contract.md): a recorded
  message is visible to readers as soon as it is written and survives the recorder being killed, but it is durable
  only after the next checkpoint (every `--checkpoint-interval-ms`, 1 s by default, and on SIGINT/SIGTERM). Readers
  that must not act on records a power loss could take back use `durable_only` (`follow --durable-only`).
- **Restarts.** A recorder restarted on the same journal continues it after recovery, with a new session and new
  descriptors, and no gap or duplicate in sequence numbers. Messages published while no recorder runs are not
  recorded. If the service keeps history, a reconnecting recorder receives it again (a counter shows these as
  `backwards`). Recovery after a crash zeroes the unused rest of the active segment; with the default 128 MiB
  segments that takes a moment, so `--segment-size` can be lowered for fast restarts.
- **Replay** creates the target services with the recorded types and settings, or opens them if they exist; an
  existing service with other types is refused. Names are prefixed with `replay/` by default so that a replay can't
  feed a recorder of the original service; `--original-names` replays onto the recorded names. Paced replay
  schedules each message from the first one by the recorded receive times divided by `--speed`, sleeps on a monotonic
  clock and spins for the last millisecond; it reports late sends (scheduled versus actual) but does not drop them.
  Messages sent before a subscriber has connected are lost unless the service keeps history:
  `--wait-subscribers` waits for subscribers first, and the replayer keeps its publishers open 500 ms after the last
  message (`--linger-ms`).
- **Seeking is by sequence number.** There is no seek by time yet (`--from` takes a sequence number).
- **iceoryx2's recording files** hold one service, with times in whole milliseconds relative to the start. What each
  direction loses is listed under [`export` and `import`](#iceoryx2-recording-files).

## iceoryx2 recording files

`tickertape-iox2 export` writes one service (choose it with `--service` when the journal has several) to a file that
`iox2 service replay` and the `iceoryx2-userland-record-and-replay` crate read; `import` appends a file written by
`iox2 service record` to a journal as a new session.

- **Journal to file loses:** absolute time (only the offset from the first exported message remains, in whole
  milliseconds; timestamps that went backwards are raised to the previous one, and counted); session boundaries;
  sequence numbers; `venue_ts_ns`; loss records; the service settings in the descriptors. The iceoryx2 version in the
  file header is the one this crate is built with. The system header (iceoryx2's per-sample header with publisher and
  node ids) isn't recorded by tickertape, so the file gets zeros there; `iox2 service replay` ignores it.
- **File to journal loses:** absolute time (the file never had it: `local_ts_ns` is `--start-unix-ns`, by default the
  import time, plus each message's offset; the descriptor is flagged as imported); the system header bytes; the
  service settings (unknown, stored as 0).
- Message bytes and order survive both directions. Prefer the default human-readable representation: `iox2-dump`
  stores the header as a raw memory image of a Rust struct, readable only by the same iceoryx2 version on the same
  architecture, and the upstream reader doesn't validate it, so import such files only from sources you trust.

## Supported versions

- **iceoryx2 0.10** (pinned `~0.10.0`). iceoryx2 is 0.x and changes its API between minor releases; a scheduled CI
  job builds and tests this crate against the newest iceoryx2 release and upstream `main` every week.
- **Rust 1.89** or newer (iceoryx2's minimum).
- **Linux** (x86-64 and aarch64) and **macOS**, as tickertape itself; CI tests all three. iceoryx2 0.10's README
  rates its support for Linux and macOS as tier 2 ("works with a restricted security and safety feature set"); its
  target for Linux is tier 1, for macOS tier 2.
- tickertape **0.4** (format version 1).

## Licence

MIT OR Apache-2.0, like tickertape and iceoryx2.
