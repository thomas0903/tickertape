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
  records.
- **Copies.** Every record returned to Python owns its payload as `bytes`. Batch columns are `bytes` objects that
  NumPy and pyarrow wrap without another copy. There is no live zero-copy access; the mapped Rust reader is not
  exposed.
- **Batches cross the language boundary once.** `append_many` checks all arguments first, then appends; on a failure
  part-way it raises `BatchAppendError` saying how many records were appended and from which sequence number.
  `read_many` and `read_batch` wait for the first record (up to a timeout), then return what is already committed.
- **Threads and signals.** Blocking calls release the GIL and wake at least every 50 ms to check for signals, so
  Ctrl-C works during a wait. Objects can be shared between threads; calls on one object are serialized.

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
