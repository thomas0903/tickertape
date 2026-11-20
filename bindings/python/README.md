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
    seqs = writer.append_batch([b"a", b"b", b"c"], schema_id=[2, 2, 3])  # one call, returns range(1, 4)
    writer.checkpoint()  # durable, not only visible

with tt.Reader("/tmp/journal") as reader:
    for record in reader:  # the records committed so far; doesn't wait
        print(record.seq, record.schema_id, record.payload)
    reader.seek(2)
    batch = reader.read_many(1000)  # up to 1000 records, one call
    live = reader.next(timeout=5.0)  # waits for the writer (Ctrl-C works), None on timeout
```

`iter(reader)` returns a new iterator over the records committed so far. It never waits, and once it stops it stays
exhausted; a later `for` loop over the reader starts a new iterator at the reader's current position. To follow the
writer, call `reader.next(timeout=...)` (default `None`: wait forever) or `read_many`/`read_batch` with a timeout.
The reader itself is not an iterator: `next(reader)` raises `TypeError`.

A complete example, with a live follower in a second process and a hand-off to pandas and DuckDB, is
[`examples/end_to_end.py`](https://github.com/thomas0903/tickertape/blob/main/bindings/python/examples/end_to_end.py).

## Visible versus durable

A record is **visible** to readers, and survives the writer process dying, as soon as `append` returns. It is
**durable** (survives power loss, under the storage assumptions in the
[crash contract](https://github.com/thomas0903/tickertape/blob/main/docs/crash-contract.md)) only after a later
`checkpoint()` returns. Closing a writer does not checkpoint. Readers that must never act on a record a power loss
could erase use `Reader(path, durable_only=True)`, which returns only records covered by the last checkpoint.

A failed `checkpoint()` poisons the writer: later checkpoints raise `DurabilityLostError`, appends keep working. The
failure is not undone by reopening: records appended before a failed checkpoint may not be durable even if a writer
opened later in the same boot checkpoints successfully (on Linux a failed `fsync` can drop the dirty pages). Treat a
checkpoint error as an incident.

## Batches and columns

`append_batch(payloads, *, schema_id, venue_ts_ns, local_ts_ns)` takes an iterable of payloads (not a single `bytes`,
`bytearray`, `memoryview` or other bytes-like object, and not a `str`: those raise `TypeError`). Each metadata
argument is one int for all records or an iterable with exactly one per record. All arguments are converted and
their lengths checked before anything is appended, so a bad argument appends nothing. A record can still fail during
the append, for example one that doesn't fit in a segment (`RecordTooLargeError`) or a full disk
(`StorageFullError`). Then `append_batch` raises `BatchAppendError`: `payloads[:e.appended]` were appended (from
sequence number `e.first_seq`), the rest were not, and `e.__cause__` is the error.

To retry after resolving the cause, pass `payloads[e.failed_index:]` and slice every per-record metadata sequence at
the same index (`schema_id=schema_ids[e.failed_index:]`, ...); a single-int metadata value needs no slicing.
`append_batch` consumes its arguments, so if you may retry, pass lists, tuples or arrays, not one-shot iterators or
generators. An exception raised by an argument's iterator (including `KeyboardInterrupt`) propagates as it is, with
nothing appended. A closed writer, or one used in a forked child, raises before consuming any argument.

`read_many(max_records, *, timeout=0)` waits up to `timeout` seconds for the first record, then returns what is
already committed. `read_batch` does the same but returns a `RecordBatch` of columns: little-endian `bytes` for
`seq`, `schema_id`, `venue_ts_ns`, `local_ts_ns`, and Arrow-style payload offsets and data. A `RecordBatch` has a
length, can be indexed (`batch[i]`, `batch[-1]`) and iterated (each item a `Record`); slicing raises `TypeError` (use
`batch.records()[a:b]`, or slice the arrays from `to_numpy()`). As for a list, an index must be an int (or have
`__index__`): other types raise `TypeError`, and only an int out of range raises `IndexError`.

Timeouts are seconds as an int, a float or a `datetime.timedelta`. `None`, `math.inf` and any value too large to
represent (such as `sys.maxsize`) wait forever; negative values and NaN raise `ValueError`. The defaults differ on
purpose: `next()` waits forever, `read_many` and `read_batch` don't wait.

```python
batch = reader.read_batch(100_000)
arrays = batch.to_numpy()  # needs numpy; numeric columns share the batch's buffers
table = batch.to_arrow()  # needs pyarrow; payload is large_binary
frame = batch.to_pandas()  # needs pandas
```

NumPy, pyarrow and pandas are optional and imported only when these methods are called.

## Copies, lifetimes and threads

- Payloads are `bytes` or any buffer of unsigned bytes (`bytearray`, `memoryview`, `array('B')`, `uint8` NumPy
  arrays); buffers of other item types (`array('b')`, `int8` or wider arrays) raise `TypeError`, view them as bytes
  first (`memoryview(a).cast('B')`, `a.view(np.uint8)`). `append` uses a `bytes` payload without copying it and copies
  other buffers once. The payload is then copied into the journal.
- Every `Record` owns its payload as an ordinary `bytes` object; it stays valid after the reader moves on or closes.
  `RecordBatch` columns are `bytes` too; arrays made from them keep them alive.
- A reader copies records into a staging buffer before building a result. Once all staged records have been
  delivered, it keeps at most 6 MiB (6,291,456 bytes) of that buffer for reuse (enough for `read_batch(10_000)` with
  payloads of a few hundred bytes); a larger one is freed, with the GIL released, by the read that delivers the last
  of them, before the call returns (or by a `seek`). Records that a read took from the journal but did not deliver,
  because it raised (on a signal, for example), stay staged in their buffer, however large, until a later read
  returns them.
- The reader is tickertape's safe `pread` reader, memory-safe whatever other processes do to the files. The
  zero-copy mapped reader of the Rust library is not exposed to Python.
- Blocking calls (`next`, waiting `read_many`/`read_batch`, `checkpoint`, opening a writer, batch loops) release the
  GIL.
- `Writer` and `Reader` objects can be used from any thread; calls on one object are serialized. A waiting read
  holds its reader for at most 50 ms at a time, so other calls on that reader (`seek`, `close()`, another read) run
  in between; `closed`, `position` and `repr` never wait at all. Reads from several threads share the one cursor:
  each record goes to exactly one of them, in order, and which one gets it is unspecified. A read that does not wait
  returns nothing only when no committed record is left, even if another thread took the records it had found.
  Give each consumer its own reader. Changing the class of an argument (for example adding or removing its
  `__iter__`) from another thread while a call is using that argument is not supported.
- `close()` from another thread (or from a signal handler) ends a read that is waiting on that reader within about
  50 ms; the waiting call raises `ValueError`, also when its timeout has passed meanwhile.
- Fork: a `Writer` or `Reader` belongs to the process that opened it. In a child created by `fork()` (including
  `multiprocessing`'s default start method on Linux before Python 3.14), every method raises `UsedAfterForkError`
  without touching the journal. A handle is the child's own only if it was opened in the child; a process id that
  matches is not enough (the bindings also count forks, so a reused process id is still recognised as foreign).
  `close()`, leaving a `with` block and garbage collection in the child release the child's copies of the file
  descriptors and memory mappings and nothing else: the parent keeps the writer's lock, and nothing is synced,
  changed or removed. So once the parent closes its writer, the journal can be opened again while the child is
  still running. Close inherited handles early in the child (until then its copy of the lock file's descriptor
  keeps the journal locked after the parent closes it). A handle that another thread was using at the moment of the
  fork is never released in the child: its state may be half-updated, and the dead thread's frame keeps a reference
  to it anyway, so its descriptors (for a writer, including the lock) stay open until the child exits or calls
  `exec`. A read that a signal handler interrupted to fork raises `UsedAfterForkError` in the child when the handler
  returns, having consumed nothing (also if its timeout passed meanwhile or the handler closed the reader). More
  generally, the owner check is the last thing the bindings do before they hand a result or an exception back to the
  interpreter (an exception object is built before it): if Python code forks during a call (a signal handler, or a
  finalizer run by the garbage collector), the call raises `UsedAfterForkError` in the child, whatever it would have
  returned or raised. A fork that happens after that, while the interpreter delivers the result (stores it, adds a
  traceback entry, or runs a signal handler), is after the call. That last check compares only the bindings' fork
  count, which CPython's fork hooks advance (the full check, process id included, is made as each call starts): a
  child forked during a call by C code that bypasses those hooks (a direct `fork()` that does not call
  `PyOS_AfterFork_Child`, as CPython requires) is detected only when it next calls a method. Arguments are converted
  before a method runs, so an invalid argument raises its `TypeError` (or `OverflowError`) in any process. Open
  handles in the child after it starts, or use the `spawn` or `forkserver` start method.
- Free-threaded Python: no free-threaded wheels are published (abi3 wheels cannot load there). Building from the
  sdist for 3.14t works and runs without the GIL, but it is not tested in CI.

### Ctrl-C and other signals

Waits check for signals at least every 50 ms, with the reader's lock released, so Ctrl-C raises `KeyboardInterrupt`
promptly and signal handlers may use the reader (even close it). For every read (`next`, `try_next`, `read_many`,
`read_batch`, iteration), in any thread:

- If it raises (a signal handler raised, the reader was closed, or anything else failed), it consumed nothing: the
  records it had already taken from the journal stay with the reader and the next read returns them first, in order.
- If it returns a value, exactly the records in that value were consumed.

Python runs signal handlers only in the main thread, so what follows about handlers concerns reads in the main
thread. A read there delivers its records only after its result is complete and pending signal handlers have run.
That covers a signal that arrives while the read builds a large result, or while it waits for another thread that is
using the reader: the read runs the handlers immediately before every attempt to take the reader's lock, and nothing
can run or wait between its final signal check and the delivery. While other threads keep taking the reader's lock
(a stream of `seek` calls, for example), the delivery waits and the read may start over; it stays interruptible
throughout, because every attempt is preceded by a signal check.

The boundary is that final signal check, and no binding can remove it: a signal that arrives after it is handled when
the call returns, after the read has delivered its records (and freed a staging buffer too large to keep). If its
handler raises before Python stores the result, the returned value is lost. A program that must not lose a record on
Ctrl-C should handle `SIGINT` itself (for example, set a flag in the handler and stop after the current record).

A read in another thread never runs handlers, and a signal does not interrupt it; the two rules above still hold for
it. A handler that runs in the main thread meanwhile may use the reader: if it closes it, the other thread's read
raises `ValueError` and consumes nothing, or returns its records if it delivered them before the close took effect.

## Errors

All errors derive from `tt.TickertapeError`: `JournalIOError` (also an `OSError`) and `StorageFullError`,
`LockedError` (another writer has the journal), `CorruptJournalError` (`path`, `offset`, `reason` code, `details`),
`UnsupportedVersionError`, `JournalNotFoundError`, `SeqUnavailableError` (also a `LookupError`),
`RecordTooLargeError` and `InvalidConfigError` (also `ValueError`), `DurabilityLostError`, `SequenceExhaustedError`,
`DiscardedError` (call `seek` to continue), `BatchAppendError` and `UsedAfterForkError`. Using a closed object raises
`ValueError`. Every exception keeps its message and the current values of its attributes across `pickle` (for
example from a `multiprocessing` worker); `JournalIOError.strerror` is the OS message, as `os.strerror(errno)`.

`Writer.recovery_report` (a `RecoveryReport` with a list of `RecoveryAction`) and `Writer.retention_report` (a
`RetentionReport`) are named tuples, like the results of the inspection functions below.

## Inspection

`tt.verify(path)`, `tt.list_segments(path)` and `tt.read_checkpoint(path)` never take the writer lock and are safe
while a writer runs. So is `tt.JournalStatus.sample(path)`, the sample that `tickertape status` prints (head, checkpoint age,
exposure, storage headroom, reader beacons); `Writer.metrics()` and `Reader.metrics(status)` add what only the writer
knows and a reader's lag. Unknown values are `None`, never guesses.

## More

- Full Python guide, including the dependency-free read-only decoder that needs no install:
  [`docs/python.md`](https://github.com/thomas0903/tickertape/blob/main/docs/python.md)
- Design decisions: [ADR 0029](https://github.com/thomas0903/tickertape/blob/main/docs/decisions/0029-python-bindings.md)
- Byte format: [`docs/format.md`](https://github.com/thomas0903/tickertape/blob/main/docs/format.md)

Licensed under MIT OR Apache-2.0.
