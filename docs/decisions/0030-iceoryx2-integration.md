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

- **Record.** One journal can hold several services. Each recording session starts with a service descriptor
  record (service name, payload and user-header type details) under a reserved schema id; data records carry the
  user header and payload. `local_ts_ns` is the receive time; `venue_ts_ns` comes from an optional extractor over
  the user header.
- **Live consumption.** Any tickertape reader, in any language with a binding, follows the journal with no
  iceoryx2 dependency.
- **Replay.** Back onto iceoryx2 services, from any sequence number, at the recorded pace (scaled) or as fast as
  possible, with service names remapped by default so a replay cannot feed its own recorder. Late sends are
  reported.
- **Export.** Parquet through the existing CLI, and iceoryx2's own recording file format in both directions, so
  the integration works with upstream's tools instead of competing with them.
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
