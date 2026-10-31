# tickertape examples: synthetic market data, live consumer, DuckDB

A runnable end-to-end demo of the tickertape library and CLI:

- **`demo-producer`** writes synthetic market data (trades and order-book updates) to a journal at a fixed rate.
- **`demo-consumer`**, in another process, follows the journal live and prints a summary every second.
- **`tickertape export --decode market-demo`** turns the journal into Parquet with decoded columns, which DuckDB
  (or pandas, Polars, Spark…) can query.

The default demo data is synthetic. No network access and no API keys are needed.

## Quickstart

From the repository root (needs Rust and, for the last step, Python with `pip install duckdb`):

```bash
cargo build --release -p tickertape-examples -p tickertape-cli
./target/release/demo-producer --dir /tmp/tt-demo --count 200000 --rate 50000 &
./target/release/demo-consumer --dir /tmp/tt-demo --count 200000
./target/release/tickertape verify /tmp/tt-demo
./target/release/tickertape export /tmp/tt-demo --format parquet --decode market-demo --output /tmp/tt-demo.parquet
python3 -c "import duckdb; print(duckdb.sql(\"SELECT kind, count(*), min(seq), max(seq) FROM read_parquet('/tmp/tt-demo.parquet') GROUP BY kind\"))"
```

`examples/quickstart.sh` runs the same steps in temporary directories, and CI runs it on Linux and macOS. Run the
producer again with the same `--dir` to append more (it continues at the next sequence number); delete the directory
to start over. While the producer runs you can also watch it with `./target/release/tickertape tail /tmp/tt-demo`.

The consumer's `max_lag` column is the time between the producer writing a record and the consumer reading it, on
the same host's wall clock, using the default (sleeping) back-off. It is a demo readout, not a benchmark; see
`benches/` and `docs/benchmarks.md` for measured latency.

## Optional: record a live feed

`binance-recorder` records Binance's public spot trade stream. The `live` feature is off by default: ordinary
workspace builds/tests do not compile its network dependencies or connect to Binance. It needs no account or API
key. This example uses Binance's public market data subject to [Binance's terms](https://www.binance.com/en/terms)
and is not affiliated with Binance. Availability depends on your network and location.

From the repository root (Python needs `pip install duckdb` for the final command):

```bash
cargo run -p tickertape-examples --release --features live --bin binance-recorder -- --dir /tmp/tt-live --max-seconds 60
cargo build --release -p tickertape-cli
./target/release/tickertape tail /tmp/tt-live --no-follow --count 5
./target/release/tickertape export /tmp/tt-live --format jsonl --decode market-demo --output /tmp/tt-live.jsonl
./target/release/tickertape export /tmp/tt-live --format parquet --decode market-demo --output /tmp/tt-live.parquet
python3 -c "import duckdb; print(duckdb.sql(\"SELECT instrument_id, count(*), min(price), max(price) FROM read_parquet('/tmp/tt-live.parquet') GROUP BY instrument_id\"))"
```

Defaults are `--symbols btcusdt,ethusdt` and `--checkpoint-every 1000`. Use `--max-records 50` for a short capture,
or Ctrl-C to stop; all three exits checkpoint the journal. `--max-seconds` includes reconnect/backoff time.
Socket waits are bounded; the system DNS resolver may delay stopping during connection setup. The recorder
prints records, skipped messages and each symbol's last price every three seconds and on exit.

`--symbols` order defines zero-based `instrument_id` values (default: BTCUSDT = 0, ETHUSDT = 1). Preserve that
order when appending to an existing directory and keep the mapping alongside your data; the schema does not
store symbol names. Trades use schema id 1 and the **taker** side: `buy = !m`, where Binance's `m` means the buyer
is the maker. Exchange trade times are converted from milliseconds to nanoseconds; local times are sampled at
receipt. Decimal prices/quantities are parsed without floating point: i64 overflow and nonzero digits beyond
eight decimal places are skipped, while extra trailing zeros are accepted exactly.

The default URL follows [Binance's official WebSocket stream documentation](https://developers.binance.com/en/docs/products/spot/web-socket-streams).
Override it with `--url 'wss://stream.binance.com:443/stream?streams=btcusdt@trade/ethusdt@trade'`, for example.
Overrides must provide raw or combined trade messages with default millisecond timestamps (omit `timeUnit`).
Malformed/unselected messages are counted and skipped. Disconnects reconnect with backoff from one to thirty
seconds; gaps and duplicate exchange trades are possible, with no historical backfill or deduplication.

The optional client uses native TLS and platform certificate roots. On Linux it requires OpenSSL development
packages and `pkg-config` (Debian/Ubuntu: `sudo apt-get install libssl-dev pkg-config`). CI builds the client and
runs offline fixture/loopback tests; it never runs a live capture. Captured journals should stay outside the repo.

## The demo schema

Payloads are opaque bytes to tickertape; the schema id says how to read them. This demo uses two event types
(little-endian; prices and quantities are fixed point, value × 10^8):

| schema_id | Event | Payload layout |
|---|---|---|
| 1 | trade (32 bytes) | `instrument_id u32 @0`, `side u8 @4` (1 buy, 2 sell), 3 zero bytes, `price i64 @8`, `qty i64 @16`, `trade_id u64 @24` |
| 2 | book update (24 bytes) | `instrument_id u32 @0`, `side u8 @4` (1 bid, 2 ask), `action u8 @5` (1 upsert, 2 delete), 2 zero bytes, `price i64 @8`, `qty i64 @16` |

Timestamps: `venue_ts_ns` is a synthetic exchange time (0.2 to 1 ms before the local time) and `local_ts_ns` is the
producer's wall clock at write time, both in nanoseconds since the Unix epoch. The encoder and decoder are in
`src/lib.rs`; the CLI's decoder (`crates/tickertape-cli/src/market_demo.rs`) implements the same layout, documented
in `docs/cli.md`.
