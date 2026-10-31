# Command-line interface (M5–M6)

Install/build the `tickertape-cli` package; its binary is `tickertape`. Commands use the public library API,
open journals read-only, and never take the writer lock. Paths may name a journal with an active writer.

All commands support `--help`. Exit codes: **0** success (including help, exhausted input, count reached or idle
timeout), **1** usage, I/O or missing-journal error, **2** corrupt data or an unsupported journal format.
Errors and recovery warnings go to stderr. JSON results go to stdout (or the selected export file).

## Commands

- `tail <DIR> [--from <SEQ> | --from-end] [--count <N>] [--no-follow] [--format text|json]
  [--timeout-secs <S>] [--durable-only]`: default starts at the oldest record and follows forever. `--from` includes SEQ;
  `--from-end` skips records observed in a startup scan. `--count 0` writes nothing. `--no-follow` stops when
  the reader reaches the current committed end. `--timeout-secs` accepts a nonnegative decimal number and stops
  after that many idle seconds; it resets after each record. Text shows sequence, schema, timestamps, payload
  length and the first 32 payload bytes as lowercase hex, followed by `…` when truncated. JSON is the record
  JSONL contract below. Each line is flushed for live consumers. After discarded history, tail warns and seeks
  to the sequence reported by the reader.
  `--durable-only` returns only records below the current checkpoint marker's `next_seq` and waits for later
  checkpoints using the usual idle timeout. With no valid checkpoint, it returns nothing until one appears;
  `--no-follow` stops at that boundary and `--from-end` skips the checkpointed prefix observed at startup.
- `inspect <DIR> [--json]`: checksum scan with per-segment metadata, totals, checkpoint and problems. Text
  displays creation time as RFC 3339 UTC, used bytes and percent. An invalid header has unavailable metadata.
  Missing/invalid checkpoint slots yield `none` in text or `null` in JSON; a checkpoint I/O error fails the command.
- `verify <DIR> [--json]`: checksum scan and chain validation, totals and every problem. Both inspect and verify
  return 2 when the scan reports problems. On a live journal the scan describes observations during the scan,
  not an atomic snapshot.
- `export <DIR> --format jsonl|parquet [--output <FILE>|-] [--from <SEQ>] [--to <SEQ>]
  [--decode market-demo] [--durable-only]`: writes records from the
  inclusive range; JSONL defaults to stdout and `--output -` also selects stdout for JSONL. The output file is created or
  truncated, but a path naming an existing journal entry (including via symlinks/hard links), or any path inside
  the journal, is rejected. `--to` must not precede `--from`.
  A first safe-reader pass captures the committed end, then a second pass emits those records, never following
  subsequent appends. Corruption encountered in the selected startup scan is reported after the valid prefix
  has been written. An empty journal exports no records. Parquet requires a file destination (missing
  `--output` or `-` returns 1). Parquet uses Snappy compression
  and row groups of at most 100,000 records. On corruption the valid prefix is finalized as a readable file
  before returning 2. Both formats share the startup scan and inclusive range semantics.
  `--durable-only` additionally caps both formats at the checkpoint marker's exclusive `next_seq` observed before
  the startup scan, even if the writer checkpoints again during export. With no valid marker the export is empty.
  Tail and export use the durability guarantee and storage assumptions in [crash contract C13](crash-contract.md)
  and [ADR 0010](decisions/0010-durable-only-reads.md); if the writer never checkpoints, neither exposes records.

## Machine-readable contracts

All JSON is UTF-8, compact, and ends with a newline. Integer fields are unsigned JSON numbers with their Rust
widths listed below; nullable fields use JSON `null`. Consumers must preserve u64 precision. Object key order
is fixed for record JSONL; consumers of inspect/verify should use names. File names are basenames, converted to
UTF-8 with replacement characters for non-UTF-8 names. Reason strings are human-readable diagnostics, not codes.

### Record JSONL (`tail --format json`, `export --format jsonl`)

Exactly one object per record, keys in this order:

```json
{"seq":42,"schema_id":3,"venue_ts_ns":0,"local_ts_ns":17,"payload_b64":"aGVsbG8="}
```

`seq`, `venue_ts_ns`, `local_ts_ns`: u64; `schema_id`: u32; `payload_b64`: string, standard RFC 4648 base64
with padding (empty payload becomes `""`). No wrapper, metadata lines or summary appear in JSONL.

### `verify --json`

```text
{"segments":u64,"records":u64,"first_seq":u64|null,"last_seq":u64|null,
 "problems":[{"file":string,"offset":u64,"reason":string}, ...]}
```

`records` counts valid records found across the scan; sequence bounds are null when there are none. A damaged
journal may contain valid records in later segments; totals do not imply a continuous valid prefix.

### `inspect --json`

```text
{"segments":[{"file":string,"base_seq":u64|null,"size_bytes":u64|null,
 "created_unix_ns":u64|null,"created_utc":string|null,"records":u64,"end_seq":u64,
 "used_bytes":u64,"used_percent":number|null,"end_state":"sealed"|"open"|"damaged"|"unknown"}, ...],
 "totals":{"segments":u64,"records":u64,"first_seq":u64|null,"last_seq":u64|null},
 "checkpoint":{"generation":u64,"next_seq":u64,"segment_base_seq":u64,"offset":u64}|null,
 "problems":[{"file":string,"offset":u64,"reason":string}, ...]}
```

`created_utc` always uses nine fractional digits and `Z`. Missing metadata and percent are null for an invalid
header. `end_seq` is exclusive, after the last valid record in that segment (or its filename's base sequence if
none). `used_bytes` includes header and seal when validated; it is zero for an invalid header. `used_percent`
is `100 * used_bytes / size_bytes`. `unknown` reserves handling of future library end states.

### Parquet (`export --format parquet`)

The base schema is exactly:

```text
message tickertape_record {
  required int64 seq (INTEGER(64,false));
  required int32 schema_id (INTEGER(32,false));
  required int64 venue_ts_ns (INTEGER(64,false));
  required int64 local_ts_ns (INTEGER(64,false));
  required binary payload;
}
```

Unsigned integer annotations preserve every bit of u64/u32 values. Timestamps are raw unsigned nanoseconds
since the Unix epoch, with 0 meaning not set. They are not Parquet timestamp logical types, whose signed range
cannot represent all u64 values. In DuckDB 2.0.0 (exercised by `duckdb_parquet_export`), convert values within
BIGINT's range with:

```sql
SELECT seq, make_timestamp_ns(local_ts_ns::BIGINT) AS local_time
FROM read_parquet('events.parquet');
```

### Market demo decoder (`export --decode market-demo`)

Decoding is opt-in for JSONL and Parquet and never changes the original payload. Parquet appends these flat
columns in order, all optional:

```text
optional binary kind (UTF8);
optional int32 instrument_id (INTEGER(32,false));
optional binary side (UTF8);
optional double price;
optional double qty;
optional int64 trade_id (INTEGER(64,false));
optional binary action (UTF8);
```

JSONL appends a `"decoded"` key after `payload_b64`: either `null` or an object with keys `kind`,
`instrument_id`, `side`, `price`, `qty`, `trade_id`, `action` in that order. For example:

```json
{"kind":"trade","instrument_id":7,"side":"buy","price":123.5,"qty":2.0,"trade_id":42,"action":null}
```

`kind` is `trade` or `book`. Trades have `buy`/`sell` sides, a u64 `trade_id`, and null `action`. Book updates
have `bid`/`ask` sides, `upsert`/`delete` actions, and null `trade_id`. Unknown schema ids, wrong payload lengths,
unknown side/action values, or nonzero reserved bytes are not decodable: the JSON object and all decoded Parquet
columns are null. These records still export successfully.

The producer layout is little-endian:

| Schema id | Length | Fields at byte offsets |
| --- | --- | --- |
| 1 (trade) | 32 bytes | instrument_id u32 @0; side u8 @4 (1 buy, 2 sell); three zero bytes @5; price i64 @8; qty i64 @16; trade_id u64 @24 |
| 2 (book) | 24 bytes | instrument_id u32 @0; side u8 @4 (1 bid, 2 ask); action u8 @5 (1 upsert, 2 delete); two zero bytes @6; price i64 @8; qty i64 @16 |

Price and quantity are signed fixed-point integers (value × 10^8); decoded doubles are `raw as f64 / 1e8`.
Large fixed-point integers can lose precision in the decoded doubles; raw payload bytes remain lossless.
