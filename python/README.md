# Read tickertape journals from Python

`tickertape_reader.py` is a read-only, offline/near-line decoder for Python 3.9 or newer, with no required
dependencies. It opens journal files only for reading, never takes the writer lock and never repairs a journal.
The on-disk specification is [`docs/format.md`](../docs/format.md); only format version 1 is supported.

Add this directory to your Python import path (or copy the module into your project):

```python
from tickertape_reader import read_journal, read_checkpoint

for record in read_journal("/tmp/tt-demo", start_seq=10, end_seq=20):
    print(record.seq, record.schema_id, record.payload)

checkpoint = read_checkpoint("/tmp/tt-demo")  # also accepts the checkpoint file path
for record in read_journal("/tmp/tt-demo", durable_only=True):
    process(record.payload)
```

`Record` is an immutable named tuple with `seq`, `schema_id`, `venue_ts_ns`, `local_ts_ns` and `payload` (bytes).
Timestamps stay unsigned integer nanoseconds. Both sequence bounds are inclusive; the reader validates the
prefix before `start_seq` and stops after `end_seq`. `Checkpoint` exposes `generation`, `next_seq`,
`segment_base_seq` and `offset`. With `durable_only=True`, only records below the checkpoint's `next_seq`
observed at startup are returned. A missing or invalid checkpoint covers no records.

For pandas, install pandas separately and pass a generator of `record._asdict()` values from `read_journal`
to `pandas.DataFrame.from_records`. Payloads remain opaque bytes; decode them using your application's schema.
For large datasets, the Rust CLI's Parquet export is usually more convenient.

The module also prints compact JSONL with exactly the raw `tickertape export --format jsonl` schema:

```bash
python3 python/tickertape_reader.py /tmp/tt-demo --from 10 --to 20
python3 python/tickertape_reader.py /tmp/tt-demo --durable-only > events.jsonl
```

Errors go to stderr: exit status 1 for I/O or usage errors, 2 for `CorruptJournal`. The exception exposes
`path`, `offset` (the failing frame's start, or 0 for a header) and `reason`; valid records can precede an error.

This reader uses ordinary buffered reads, copies payloads, is not zero-copy and is slower than the Rust reader.
CRC32C is implemented in pure Python; if the optional `crc32c` PyPI package is installed, it is used automatically.
No optional package is needed to read journals or run the tests.

Against a live journal, the result is a snapshot of observations during the scan, not an atomic snapshot.
Segment names and the durable boundary are captured once. The reader may stop at a record being written
(commit word zero); invoke it again for later appends. A failed validation reopens and rereads the failing
segment position once, so a torn read raises `CorruptJournal` only if validation still fails on retry. Segment
replacement during repair raises an error; restart the scan after recovery. Incomplete rolls, missing initial
history, disconnected segments and persistent corruption are errors. Use an idle journal or a consistent copy
for reproducible offline analysis. The reader does not follow new segments or wait for new records/checkpoints.

Run the standard-library tests from the repository root:

```bash
cargo build --release -p tickertape-cli -p tickertape-examples --locked
TICKERTAPE_BIN=target/release/tickertape TICKERTAPE_PRODUCER=target/release/demo-producer \
    python3 -m unittest discover -s python -v
```

Without these environment variables, only the real-library integration tests are skipped.
