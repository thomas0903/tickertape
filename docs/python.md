# tickertape from Python

There are two ways to use tickertape journals from Python. Both read the same on-disk format (`format.md`).

| | Bindings: `pip install tickertape` | Decoder: `python/tickertape_reader.py` |
|---|---|---|
| Install | A wheel (built from Rust with PyO3); CPython 3.11+ on Linux and macOS | Copy one file; Python 3.9+, no dependencies |
| Import | `import tickertape` | `import tickertape_reader` |
| Write | Yes: `Writer`, the single writer, with the journal lock and crash recovery | No |
| Read | Live cursor: replay from any sequence, follow the writer, wait with a timeout | One pass over what is committed when it runs; doesn't follow or wait |
| Batches | `append_batch`, `read_many`, columnar `read_batch` for NumPy/Arrow/pandas | Generator of named tuples |
| Durable-only reads | `Reader(durable_only=True)`, waits for checkpoints | `durable_only=True`, capped at the checkpoint seen at startup |
| Validation | The Rust reader's (shared with the CLI) | An independent implementation of the specification |
| Implementation | Compiled Rust | Pure Python (slower) |

Use the bindings to record events from Python, to follow a journal live, or to read large journals quickly. Use the
decoder where you cannot install a compiled package, for offline analysis of a copied journal, or as an independent
check of the format. The decoder's documentation is [`python/README.md`](../python/README.md). For bulk analysis
without Python in the loop, `tickertape export --format parquet` (see `cli.md`) is another option.

## Bindings

> **Status:** like the C ABI, the bindings are not stable before tickertape 1.0. What they promise from 1.0 is in
> [`compatibility.md`](compatibility.md#python-and-java).

The package's own guide, which is also its PyPI page, is [`bindings/python/README.md`](../bindings/python/README.md).
The design decisions are in [ADR 0029](decisions/0029-python-bindings.md). In brief:

- **Visible versus durable** is exactly as in the crash contract: visible when `append` returns, durable when a later
  `checkpoint()` returns, or once a later `request_checkpoint()` has finished (`checkpoint_progress()`; ADR 0038:
  the checkpoint runs on a library thread while appends go on, and a failure is raised once by the next
  `request_checkpoint()`, `checkpoint_progress()`, `checkpoint()` or `close()`, then `DurabilityLostError`). Closing
  a writer does not checkpoint; `close()` waits for a background checkpoint in flight and raises a failure as Rust's
  `Writer::close` does. `Writer(early_writeback=..., helper_cpus=...)` set the
  Linux-only writer options of ADR 0038, and `Writer(spare_segment=...)` (from 1.1) the spare segment (ADR 0049; `None`,
  the default, is on on macOS and off on Linux, and nothing reports the value in effect).
  `Reader(durable_only=True)` returns only checkpointed
  records. After a failed checkpoint, records appended before it may not be durable even if a writer opened later in
  the same boot checkpoints successfully.
- **Copies.** Payloads are `bytes` or buffers of unsigned bytes (uint8 only). Every record returned to Python owns its
  payload as `bytes`. Batch columns are `bytes` objects that NumPy and pyarrow wrap without another copy. There is no
  live zero-copy access; the mapped Rust reader is not exposed.
- **Batches cross the language boundary once.** `append_batch` converts and checks all arguments first, then appends
  them as one batch (`Writer::append_batch`, ADR 0041: if at least one record is appended and wake-ups are enabled,
  blocked readers are woken once after the last record appended, plus once at each roll inside the batch);
  on a failure part-way it raises `BatchAppendError` saying how many records were appended and from which sequence
  number. To retry, slice the payloads and every per-record metadata sequence at `failed_index`, so keep them in
  sequences rather than one-shot iterators. `read_many` and `read_batch` wait for the first record (up to a timeout,
  default 0), then return what is already committed.
- **Iteration.** `for record in reader` drains the records committed so far without waiting; each `iter(reader)` is a
  new iterator that stays exhausted once it stops. `reader.next(timeout=None)` waits.
- **Threads and signals.** Blocking calls release the GIL and wake at least every 50 ms to check for signals with the
  reader's lock released, so Ctrl-C works during a wait and signal handlers may use the reader. In every thread, a read
  that raises consumed nothing, and a read that returns a value consumed exactly the records in it. Python runs signal
  handlers only in the main thread; a read there delivers its records only once its result is complete and pending
  signal handlers have run, so if a handler raises (also while a large result is being built, or while the read waits
  for another thread using the reader), the records are returned by the next read, not skipped. The boundary no binding
  can remove is the read's final signal check: a signal that arrives after it is handled when the call returns, after
  the records are delivered (and a staging buffer too large to keep is freed), and if its handler raises before Python
  stores the result, the result is lost. A read in another thread is not interrupted by signals. Other threads that keep
  taking the reader's lock (a stream of `seek` calls) can delay a delivery, which stays interruptible. Objects can be
  shared between threads; a read that does not wait returns nothing only when no committed record is left; a waiting
  read holds its reader for at most 50 ms at a time, and `close()` from another thread ends it with `ValueError`.
- **Fork.** Handles belong to the process that opened them (its process id and a count of forks, so a reused process
  id does not fool the check); in a forked child they raise `UsedAfterForkError`. Closing them in the child (or
  letting them be collected) releases the child's copies of the descriptors and mappings without unlocking or
  changing anything the parent uses, so the journal can be reopened once the parent closes its writer, while the
  child still runs. A handle that another thread was using at the fork is never released in the child (its
  descriptors stay open until the child exits or calls `exec`). Open handles after forking, or use the `spawn` start
  method of `multiprocessing`.

### Operational metrics

`tickertape.JournalStatus.sample(path)` takes the read-only sample that `tickertape status` prints (ADR
[0032](decisions/0032-operational-metrics.md); meaning and accuracy of each value:
[`operations.md`](operations.md#monitoring)). It needs no writer and releases the GIL. `Writer.metrics()` returns what
only the writer knows, and `Reader.metrics(status)` a reader's cursor and lag behind a sample:

```python
s = tickertape.JournalStatus.sample("/data/journal")
print(s.exposure.records, s.checkpoint and s.checkpoint.estimated_age_ns, s.storage.estimated_rolls_left)
lag = reader.metrics(s).lag          # None, with reader.metrics(s).lag_unknown, if it can't be established
w = writer.metrics()                 # in the writer's process: no system calls
```

- Results are named tuples (`JournalStatus` is a frozen dataclass); unknown values are `None`, never guesses, often
  with a reason code (`age_unknown`, `lag_unknown`: the codes of the CLI's JSON). Durations are integer nanoseconds.
- Corruption is `s.head.end == "invalid"`, a `s.checkpoint.problem`, or `s.head.chain_broken`. `s.head.verified` false
  means the segments between the checkpoint and the newest were not read: no corruption reported then does not mean
  that a writer would open the journal.
- `Reader.metrics` measures against the sample, so take it after the reads you want counted. Records the reader has
  read from the journal but not returned yet (`buffered`) count in the lag.
- Cost: `JournalStatus.sample()` lists the directory, reads the checkpoint marker, scans the newest segment from the
  checkpoint (so it grows with what was appended since the last checkpoint), reads two segment headers, `writer.lock`
  and the beacons, and calls `statvfs`; the metrics calls make no system calls. The Python bindings call the core crate
  directly (ADR 0029), not the C ABI.

The latency histograms (the writer's, too) and record ages of the Rust crate (`tickertape::latency`, from 1.1, ADR
[0047](decisions/0047-latency-histograms.md)) are not available from Python yet; `tickertape age` (see [`cli.md`](cli.md))
reports ages from the command line.

### End-to-end example

[`bindings/python/examples/end_to_end.py`](../bindings/python/examples/end_to_end.py) writes market-style events in
batches with a checkpoint after each, follows the journal live from a second process, verifies it, replays the
durable records from a chosen sequence number in one batch, and summarizes them with pandas and DuckDB:

```bash
pip install tickertape pandas duckdb pyarrow
python bindings/python/examples/end_to_end.py /tmp/tt-python-demo
```

The core of the hand-off to analysis:

```python
import duckdb
import tickertape as tt

with tt.Reader("/tmp/tt-python-demo", durable_only=True) as reader:
    frame = reader.read_batch(1_000_000).to_pandas()
print(duckdb.sql("SELECT schema_id, count(*) FROM frame GROUP BY schema_id"))
```

### Building from source

```bash
cd bindings/python
python -m venv .venv && . .venv/bin/activate
pip install maturin pytest
maturin develop --release --features test-hooks
pytest
```

The `test-hooks` feature adds the test-only methods that some tests need to order races and simulate a handle locked at
a fork; without it those tests are skipped. Release wheels are built without it.

The cross-language tests also need the Rust CLI and fixture helpers; without them they are skipped:

```bash
cargo build --release --locked -p tickertape-cli -p tickertape-examples -p tickertape-tests --bins
TICKERTAPE_BIN=../../target/release/tickertape TICKERTAPE_PRODUCER=../../target/release/demo-producer \
  TICKERTAPE_DURABLE_HELPER=../../target/release/tt-durable-only pytest
```

The bindings crate is not a member of the Cargo workspace and has its own `Cargo.lock`; lint it with
`cargo clippy --manifest-path bindings/python/Cargo.toml -- -D warnings`.
