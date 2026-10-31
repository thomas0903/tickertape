# tickertape examples: synthetic market data, live consumer, DuckDB

A runnable end-to-end demo of the tickertape library and CLI:

- **`demo-producer`** writes synthetic market data (trades and order-book updates) to a journal at a fixed rate.
- **`demo-consumer`**, in another process, follows the journal live and prints a summary every second.
- **`tickertape export --decode market-demo`** turns the journal into Parquet with decoded columns, which DuckDB
  (or pandas, Polars, Spark…) can query.

All data is synthetic. No network access and no API keys are needed.

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
