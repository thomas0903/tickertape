# 0029: Python bindings

- **Status:** proposed (the distribution name and the PyPI publishing step need the owner's agreement)
- **Date:** 2026-11-19

## Context

The v1.0 roadmap asks for installable Python bindings with batch operations and explicit copy/lifetime behaviour,
while keeping the dependency-free read-only decoder (ADR 0013). Live zero-copy access from Python is not required.
The owner wants prebuilt wheels on PyPI at release time. A C ABI crate is being built separately; the bindings must
not depend on it. Repository rules: `unsafe` only in `mmap.rs` and `crc.rs`, permissive licences, MSRV 1.85.

## Options considered

1. **ctypes/cffi over the C ABI:** one native surface for all languages, but couples this work to a crate that does not
   exist yet, and needs hand-written ownership glue in Python.
2. **PyO3 + maturin over the Rust API:** idiomatic classes and exceptions, no `unsafe` in our code, mature wheel tooling
   (manylinux/musllinux, abi3). Adds PyO3 as a dependency of a separate crate only.
3. **Extend the pure-Python decoder to write:** no build step, but would duplicate the writer, recovery and
   publication protocol, the hardest parts to get right.

## Decision

Option 2.

**Layout.** `bindings/python/` holds the crate `tickertape-python` (`publish = false`, cdylib `tickertape._native`)
and the Python package `tickertape` (`python/tickertape/`). It is excluded from the Cargo workspace and has its own
`Cargo.lock`, like `fuzz/` and `compare/`, so PyO3 does not enter the workspace lockfile or MSRV check. It depends on
`crates/tickertape` by path. Its version is kept equal to the workspace version; `release.py prepare` checks that.

**ABI and Python versions.** One `abi3-py311` wheel per platform serves CPython 3.11 and newer. 3.11 is the oldest
CPython still supported upstream (3.10 reached end of life in October 2026), and the buffer protocol needed to accept
`bytearray`/`memoryview`/NumPy payloads joined the stable ABI in 3.11. CI tests 3.11 and 3.15. Free-threaded Python is
**not shipped**: abi3 wheels cannot load in a free-threaded interpreter. The classes do not rely on the GIL (frozen
PyO3 classes with an internal mutex), and a source build for 3.14t passes the test suite locally, but CI does not test
it. PEP 803 (`abi3t`, 3.15+) is a later option.

**Name.** PyPI distribution `tickertape` (import name `tickertape`). On 2026-11-19 the PyPI JSON API and simple index
returned 404 for `tickertape`, `ticker-tape`, `tickertape-journal`, `pytickertape`, `py-tickertape`,
`tickertape-python`, `tickertape-py` and `tickertape-rs`, so the name matching the crate appears free. Nothing was
registered. The name is set only in `bindings/python/pyproject.toml` (`[project].name`); the workflows do not repeat
it, and changing it does not change the import name.

**The two Python options.** The pure-Python decoder stays at `python/tickertape_reader.py`, unchanged, as the
no-install, read-only, offline option and as an independent check of the format. The wheel does not bundle it: one
install would then carry two readers with different semantics (a one-pass offline scan and a live cursor), a packaged
copy could drift from the file users copy, and people who want the decoder want it because it needs no install.
Docs present both (`docs/python.md`); the import names differ (`tickertape_reader` versus `tickertape`).

**API.** `Writer(path, *, segment_size, recovery, notify, prefault, retention)` with `append`, `append_many`,
`checkpoint`, `next_seq`, `recovery_report`, `retention_report`, `last_retention_error` and `close`; `Reader(path, *,
durable_only, notify, backoff)` with `seek`, `try_next`, `next(timeout)`, iteration, `read_many`, `read_batch`,
`position` and `close`; `Backoff`, `Retention.delete(...)`/`Retention.archive(dir, ...)`; `verify`, `list_segments`
and `read_checkpoint` returning named tuples. Both classes are context managers; `close()` is idempotent; use after
close raises `ValueError`, as for Python files. Closing a writer does **not** checkpoint, like the Rust `Drop` and like
closing a Python file (which flushes but does not `fsync`): visibility already happened at `append`.

**Batches cross the boundary once.** `append_many(payloads, *, schema_id, venue_ts_ns, local_ts_ns)` takes an iterable
of payloads and, for each metadata field, one int or a sequence of ints. Every argument is converted and checked
before anything is appended (a bad argument appends nothing). It returns the `range` of assigned sequence numbers.
If an append fails part-way, it raises `BatchAppendError` with `appended`, `first_seq` and `failed_index`, and the
mapped core error as `__cause__`: `payloads[:appended]` are committed (visible, not durable), the rest are not, and
the caller can retry `payloads[appended:]`. `read_many(max_records, *, timeout=0)` waits up to `timeout` for the first
record (`None`: forever), then returns what is already committed, up to `max_records`. If an error occurs after it
has read some records, it returns those and raises the error on the next read (`seek` clears it), so neither the
records nor the error are lost. `read_batch` has the same semantics and returns a columnar `RecordBatch`.

**Columnar batches.** A `RecordBatch` holds six immutable `bytes` objects: little-endian `seq`, `schema_id`,
`venue_ts_ns`, `local_ts_ns` and Arrow `large_binary` payload offsets and data. `numpy.frombuffer` and
`pyarrow.py_buffer` wrap them without copying, and the resulting arrays keep the `bytes` alive, so lifetimes are
enforced by reference counting. `to_numpy()`, `to_arrow()` and `to_pandas()` import those libraries only when called;
none is a dependency. We do not implement the buffer protocol or the Arrow PyCapsule interface ourselves: both need
`unsafe` (raw `Py_buffer` or `ArrowArray` structs) or a large Arrow dependency, for no gain over `bytes`, which already
export the buffer protocol. The PyCapsule interface is a possible follow-up through a vetted crate.

**Copies and lifetimes.** `append` borrows a `bytes` payload without copying it (bytes are immutable) and copies other
buffers once, because they could change while the GIL is released; the writer then copies the payload into the
segment. Every `Record` owns its payload as a `bytes` object, valid after the reader moves on or closes. The
binding uses only the safe `pread` reader. `Reader::open_mapped` is not exposed: its precondition (only a tickertape
writer modifies the files) cannot be checked from Python, a `SIGBUS` would kill the interpreter, and records must be
copied into `bytes` anyway, so it would save little.

**GIL, threads and signals.** The GIL is released for `Writer` open (recovery), `checkpoint`, `close`, the append
loop of `append_many`, the read loops of `read_many`/`read_batch`, `seek`, inspection, and every wait. A single
`append` or `try_next` keeps the GIL (it is shorter than releasing it, except during a segment roll). Waits run in
slices of at most 50 ms; between slices the binding calls `PyErr_CheckSignals`, so Ctrl-C raises `KeyboardInterrupt`
within about 50 ms and consumes no record. `Writer` and `Reader` may be used from any thread: each holds its core
object in a mutex, acquired without holding the GIL, so concurrent calls on one object serialize. A reader is a cursor,
so share one between threads only if they coordinate; use one reader per consumer.

**Errors.** Exceptions are defined in Python (`tickertape/_errors.py`) so they can also derive from built-ins:
`TickertapeError` is the base; `JournalIOError` (also `OSError`, with `errno`, `filename`, `op`, `storage_full`) and
its subclass `StorageFullError`; `LockedError`; `CorruptJournalError` (`path`, `offset`, a stable `reason` code for
each `CorruptReason`, `details`); `UnsupportedVersionError`; `JournalNotFoundError`; `SeqUnavailableError` (also
`LookupError`); `RecordTooLargeError` and `InvalidConfigError` (also `ValueError`); `DurabilityLostError`;
`SequenceExhaustedError`; `DiscardedError`; `BatchAppendError`. Both core enums are `#[non_exhaustive]`, so the
compiler cannot flag a new variant; a test parses `crates/tickertape/src/error.rs` and fails if any variant is missing
from the binding's mapping tables. Unmapped variants would still raise `TickertapeError` (reason `"unknown"`).

**Durability.** Unchanged and stated as in the crash contract: visible when `append` returns, durable when a later
`checkpoint` returns; `Reader(durable_only=True)` returns only records below the checkpoint marker's `next_seq`.

**Typing.** `_native.pyi` and `py.typed` ship in the wheel. CI runs `mypy --strict` and `mypy.stubtest` against the
built module.

**Unsafe code.** None: the crate sets `#![deny(unsafe_code)]`; PyO3's generated code compiles under it.

**Distribution.** `.github/workflows/python-wheels.yml` builds an sdist and wheels for manylinux and musllinux
(x86-64, aarch64) and macOS (x86-64, arm64), smoke-tests each native wheel, and uploads them as artefacts. It runs on
demand and on `v*` tags. Its PyPI step uses Trusted Publishing and runs only for a tag that `release.py prepare`
accepts, in the `pypi` environment, when the repository variable `PYPI_PUBLISHING` is `enabled`; it never runs on pull
requests or branches. The one-time setup is in `docs/playbooks/release-checklist.md`.

## Consequences

- Python users can write, follow live and replay journals, and hand batches to NumPy, pandas, pyarrow or DuckDB.
- Each release must bump `bindings/python/Cargo.toml` and refresh its lockfile along with the workspace.
- A second lockfile to keep up to date; CI runs `cargo deny` on it too.
- Not covered: free-threaded wheels, Windows, PyPy, Arrow PyCapsule export, live zero-copy access.
- Tests: `bindings/python/tests/` (pytest) cover round trips, batches and partial failure, live following across
  processes, durable-only reads, rolls, corruption, Ctrl-C, context managers, the writer lock and cross-language
  fixtures against the Rust CLI and the pure-Python decoder.
