# tickertape from Python

There are two ways to use tickertape journals from Python. Both read the same on-disk format (`format.md`).

| | Bindings: `pip install tickertape` | Decoder: `python/tickertape_reader.py` |
|---|---|---|
| Install | A wheel (built from Rust with PyO3); CPython 3.11+ on Linux and macOS | Copy one file; Python 3.9+, no dependencies |
| Import | `import tickertape` | `import tickertape_reader` |
| Write | Yes: `Writer`, the single writer, with the journal lock and crash recovery | No |
| Read | Live cursor: replay from any sequence, follow the writer, wait with a timeout | One pass over what is committed when it runs; doesn't follow or wait |
| Batches | `append_many`, `read_many`, columnar `read_batch` for NumPy/Arrow/pandas | Generator of named tuples |
| Durable-only reads | `Reader(durable_only=True)`, waits for checkpoints | `durable_only=True`, capped at the checkpoint seen at startup |
| Validation | The Rust reader's (shared with the CLI) | An independent implementation of the specification |
| Implementation | Compiled Rust | Pure Python (slower) |

Use the bindings to record events from Python, to follow a journal live, or to read large journals quickly. Use the
decoder where you cannot install a compiled package, for offline analysis of a copied journal, or as an independent
check of the format. The decoder's documentation is [`python/README.md`](../python/README.md). For bulk analysis
without Python in the loop, `tickertape export --format parquet` (see `cli.md`) is another option.

## Bindings

The package's own guide, which is also its PyPI page, is [`bindings/python/README.md`](../bindings/python/README.md).
The design decisions are in [ADR 0029](decisions/0029-python-bindings.md). In brief:

- **Visible versus durable** is exactly as in the crash contract: visible when `append` returns, durable when a later
  `checkpoint()` returns. Closing a writer does not checkpoint. `Reader(durable_only=True)` returns only checkpointed
  records. After a failed checkpoint, records appended before it may not be durable even if a writer opened later in
  the same boot checkpoints successfully.
- **Copies.** Payloads are `bytes` or buffers of unsigned bytes (uint8 only). Every record returned to Python owns its
  payload as `bytes`. Batch columns are `bytes` objects that NumPy and pyarrow wrap without another copy. There is no
  live zero-copy access; the mapped Rust reader is not exposed.
- **Batches cross the language boundary once.** `append_many` converts and checks all arguments first, then appends;
  on a failure part-way it raises `BatchAppendError` saying how many records were appended and from which sequence
  number. To retry, slice the payloads and every per-record metadata sequence at `failed_index`, so keep them in
  sequences rather than one-shot iterators. `read_many` and `read_batch` wait for the first record (up to a timeout,
  default 0), then return what is already committed.
- **Iteration.** `for record in reader` drains the records committed so far without waiting; each `iter(reader)` is a
  new iterator that stays exhausted once it stops. `reader.next(timeout=None)` waits.
- **Threads and signals.** Blocking calls release the GIL and wake at least every 50 ms to check for signals with the
  reader's lock released, so Ctrl-C works during a wait and signal handlers may use the reader. A read that raises
  consumed nothing, and a read that returns a value consumed exactly the records in it: a read delivers its records
  only once its result is complete and pending signal handlers have run, so if a handler raises (also while a large
  result is being built, or while the read waits for another thread using the reader), the records are returned by
  the next read, not skipped. The one window no binding can close is a signal that arrives once the records are
  delivered: its handler runs after the call has returned, and if it raises before Python stores the result, the
  result is lost. Objects can be shared between
  threads; a read that does not wait returns nothing only when no committed record is left; a waiting read holds its
  reader for at most 50 ms at a time, and `close()` from another thread ends it with `ValueError`.
- **Fork.** Handles belong to the process that opened them (its process id and a count of forks, so a reused process
  id does not fool the check); in a forked child they raise `UsedAfterForkError`. Closing them in the child (or
  letting them be collected) releases the child's copies of the descriptors and mappings without unlocking or
  changing anything the parent uses, so the journal can be reopened once the parent closes its writer, while the
  child still runs. A handle that another thread was using at the fork is never released in the child (its
  descriptors stay open until the child exits or calls `exec`). Open handles after forking, or use the `spawn` start
  method of `multiprocessing`.

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
maturin develop --release
pytest
```

The cross-language tests also need the Rust CLI and fixture helpers; without them they are skipped:

```bash
cargo build --release --locked -p tickertape-cli -p tickertape-examples -p tickertape-tests --bins
TICKERTAPE_BIN=../../target/release/tickertape TICKERTAPE_PRODUCER=../../target/release/demo-producer \
  TICKERTAPE_DURABLE_HELPER=../../target/release/tt-durable-only pytest
```

The bindings crate is not a member of the Cargo workspace and has its own `Cargo.lock`; lint it with
`cargo clippy --manifest-path bindings/python/Cargo.toml -- -D warnings`.
