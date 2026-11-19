# tickertape for Python

Python bindings for [tickertape](https://github.com/thomas0903/tickertape), a local, append-only event journal on
memory-mapped segment files. One writer process appends records; any number of readers, in the same or other
processes on the same host, follow the journal live or replay it from any sequence number. Records are opaque
payload bytes with a sequence number, a caller-defined schema id and two caller-supplied timestamps.

```bash
pip install tickertape
```

Prebuilt wheels: Linux (manylinux and musllinux, x86-64 and aarch64) and macOS (x86-64 and Apple Silicon), one wheel
per platform for CPython 3.11 and newer. No runtime dependencies. Windows is not supported (neither is the journal).

## Write, follow, replay

```python
import time
import tickertape as tt

with tt.Writer("/tmp/journal") as writer:
    seq = writer.append(b"hello", schema_id=1, local_ts_ns=time.time_ns())
    seqs = writer.append_many([b"a", b"b", b"c"], schema_id=[2, 2, 3])  # one call, returns range(1, 4)
    writer.checkpoint()  # durable, not only visible

with tt.Reader("/tmp/journal") as reader:
    for record in reader:  # the records committed so far
        print(record.seq, record.schema_id, record.payload)
    reader.seek(2)
    batch = reader.read_many(1000)  # up to 1000 records, one call
    live = reader.next(timeout=5.0)  # waits for the writer (Ctrl-C works), None on timeout
```

A complete example, with a live follower in a second process and a hand-off to pandas and DuckDB, is
[`examples/end_to_end.py`](https://github.com/thomas0903/tickertape/blob/main/bindings/python/examples/end_to_end.py).

## Visible versus durable

A record is **visible** to readers, and survives the writer process dying, as soon as `append` returns. It is
**durable** (survives power loss, under the storage assumptions in the
[crash contract](https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md)) only after a later
`checkpoint()` returns. Closing a writer does not checkpoint. Readers that must never act on a record a power loss
could erase use `Reader(path, durable_only=True)`, which returns only records covered by the last checkpoint.

## Batches and columns

`append_many(payloads, *, schema_id, venue_ts_ns, local_ts_ns)` takes an iterable of payloads; each metadata argument
is one int for all records or a sequence with one per record. Everything is checked before anything is appended. If an
append fails part-way, it raises `BatchAppendError`: `payloads[:e.appended]` were appended (from sequence number
`e.first_seq`), the rest were not, and `e.__cause__` is the error (for example `StorageFullError`).

`read_many(max_records, *, timeout=0)` waits up to `timeout` seconds for the first record (`None`: forever), then
returns what is already committed. `read_batch` does the same but returns a `RecordBatch` of columns: little-endian
`bytes` for `seq`, `schema_id`, `venue_ts_ns`, `local_ts_ns`, and Arrow-style payload offsets and data.

```python
batch = reader.read_batch(100_000)
arrays = batch.to_numpy()  # needs numpy; numeric columns share the batch's buffers
table = batch.to_arrow()  # needs pyarrow; payload is large_binary
frame = batch.to_pandas()  # needs pandas
```

NumPy, pyarrow and pandas are optional and imported only when these methods are called.

## Copies, lifetimes and threads

- `append` uses a `bytes` payload without copying it and copies other buffers (`bytearray`, `memoryview`, arrays)
  once. The payload is then copied into the journal.
- Every `Record` owns its payload as an ordinary `bytes` object; it stays valid after the reader moves on or closes.
  `RecordBatch` columns are `bytes` too; arrays made from them keep them alive.
- The reader is tickertape's safe `pread` reader, memory-safe whatever other processes do to the files. The
  zero-copy mapped reader of the Rust library is not exposed to Python.
- Blocking calls (`next`, waiting `read_many`/`read_batch`, `checkpoint`, opening a writer, batch loops) release the
  GIL. Waits check for signals at least every 50 ms, so Ctrl-C raises `KeyboardInterrupt` and consumes nothing.
- `Writer` and `Reader` objects can be used from any thread; calls on one object are serialized. A reader is one
  cursor, so give each consumer its own.
- Free-threaded Python: no free-threaded wheels are published (abi3 wheels cannot load there). Building from the
  sdist for 3.14t works and runs without the GIL, but it is not tested in CI.

## Errors

All errors derive from `tt.TickertapeError`: `JournalIOError` (also an `OSError`) and `StorageFullError`,
`LockedError` (another writer has the journal), `CorruptJournalError` (`path`, `offset`, `reason` code, `details`),
`UnsupportedVersionError`, `JournalNotFoundError`, `SeqUnavailableError` (also a `LookupError`),
`RecordTooLargeError` and `InvalidConfigError` (also `ValueError`), `DurabilityLostError`, `SequenceExhaustedError`,
`DiscardedError` (call `seek` to continue) and `BatchAppendError`. Using a closed object raises `ValueError`.

## Inspection

`tt.verify(path)`, `tt.list_segments(path)` and `tt.read_checkpoint(path)` never take the writer lock and are safe
while a writer runs.

## More

- Full Python guide, including the dependency-free read-only decoder that needs no install:
  [`docs/python.md`](https://github.com/thomas0903/tickertape/blob/main/docs/python.md)
- Design decisions: [ADR 0029](https://github.com/thomas0903/tickertape/blob/main/docs/decisions/0029-python-bindings.md)
- Byte format: [`docs/format.md`](https://github.com/thomas0903/tickertape/blob/main/docs/format.md)

Licensed under MIT OR Apache-2.0.
