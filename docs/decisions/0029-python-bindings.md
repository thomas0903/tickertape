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
This is a deliberate exception to ADR 0031's rule that a binding's version follows the C ABI's version: these
bindings use the Rust API directly, not the C ABI (see the amendment note in ADR 0031).

**ABI and Python versions.** One `abi3-py311` wheel per platform serves CPython 3.11 and newer. 3.11 is the oldest
CPython still supported upstream (3.10 reached end of life in October 2026), and the buffer protocol needed to accept
`bytearray`/`memoryview`/NumPy payloads joined the stable ABI in 3.11. CI tests 3.11 and 3.15. Free-threaded Python is
**not shipped**: abi3 wheels cannot load in a free-threaded interpreter. The classes do not rely on the GIL (frozen
PyO3 classes with an internal mutex), and a source build for 3.14t passed the test suite locally with the GIL
disabled (the optional NumPy/pyarrow/pandas tests were skipped), but CI does not test it. PEP 803 (`abi3t`, 3.15+) is a later option.

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
durable_only, notify, backoff)` with `seek`, `try_next`, `next(timeout=None)`, iteration, `read_many`, `read_batch`,
`position` and `close`; `Backoff`, `Retention.delete(...)`/`Retention.archive(dir, ...)`; `verify`, `list_segments`
and `read_checkpoint` returning named tuples, and the writer's reports as named tuples too (`RecoveryReport` with a
list of `RecoveryAction`, `RetentionReport`). Both classes are context managers; `close()` is idempotent; use after
close raises `ValueError`, as for Python files. Closing a writer does **not** checkpoint, like the Rust `Drop` and like
closing a Python file (which flushes but does not `fsync`): visibility already happened at `append`. `Record`,
`Backoff` and `Retention` have value equality (returning `NotImplemented` for other types) and matching hashes.
Runtime signatures and stubs show the real defaults: `next(timeout=None)` waits forever, `read_many`/`read_batch`
default to `timeout=0`. Timeouts are seconds or a `timedelta`; `None`, `math.inf` and anything too large to represent
mean forever.

**Iteration.** `Reader` is an iterable, not an iterator: `iter(reader)` returns a new iterator that drains the records
committed so far from the reader's live cursor, never waits, and stays exhausted once it stops (Python's iterator
protocol requires that). `next(reader)` is therefore a `TypeError`, which also avoids confusing it with
`reader.next()`, which waits. `RecordBatch` supports `len`, indexing and iteration; slicing raises `TypeError` (it may
be added later without breaking callers).

**Batches cross the boundary once.** `append_many(payloads, *, schema_id, venue_ts_ns, local_ts_ns)` takes an iterable
of payloads and, for each metadata field, one int or a sequence of ints. Every argument is converted and checked
before anything is appended (a bad argument appends nothing). It returns the `range` of assigned sequence numbers.
`payloads` must not itself be a payload: `str` and every bytes-like object raise `TypeError`. A metadata iterable
that yields more values than there are payloads fails as soon as it does, so an endless iterator cannot exhaust
memory. If an append fails part-way (preflight checks conversion and lengths, but a record that does not fit or a full
disk can still fail it), it raises `BatchAppendError` with `appended`, `first_seq` and `failed_index`, and the mapped
core error as `__cause__`: `payloads[:appended]` are committed (visible, not durable), the rest are not. To retry, the
caller slices the payloads and every per-record metadata sequence at `failed_index`, so a caller who may retry keeps
replayable sequences rather than one-shot iterators. The returned `range` is built from Python ints when sequence
numbers exceed `isize` (from 2**63). `read_many(max_records, *, timeout=0)` waits up to `timeout` for the first
record (`None`: forever), then returns what is already committed, up to `max_records`. If an error occurs after it
has read some records, it returns those and raises the error on the next read (a successful `seek` clears it; a
failed one leaves it), so neither the records nor the error are lost. `read_batch` has the same semantics and
returns a columnar `RecordBatch`.

**Columnar batches.** A `RecordBatch` holds six immutable `bytes` objects: little-endian `seq`, `schema_id`,
`venue_ts_ns`, `local_ts_ns` and Arrow `large_binary` payload offsets and data. `numpy.frombuffer` and
`pyarrow.py_buffer` wrap them without copying, and the resulting arrays keep the `bytes` alive, so lifetimes are
enforced by reference counting. `to_numpy()`, `to_arrow()` and `to_pandas()` import those libraries only when called;
none is a dependency. We do not implement the buffer protocol or the Arrow PyCapsule interface ourselves: both need
`unsafe` (raw `Py_buffer` or `ArrowArray` structs) or a large Arrow dependency, for no gain over `bytes`, which already
export the buffer protocol. The PyCapsule interface is a possible follow-up through a vetted crate.

**Copies and lifetimes.** Payloads are `bytes` or buffers of unsigned bytes only (item format `B`: `bytearray`,
`memoryview`, `array('B')`, `uint8` NumPy arrays); other item types such as `array('b')` or `int8` arrays raise
`TypeError` rather than being reinterpreted. `append` borrows a `bytes` payload without copying it (bytes are immutable)
and copies other buffers once, because they could change while the GIL is released; the writer then copies the payload
into the segment. Every `Record` owns its payload as a `bytes` object, valid after the reader moves on or closes. The
binding uses only the safe `pread` reader. `Reader::open_mapped` is not exposed: its precondition (only a tickertape
writer modifies the files) cannot be checked from Python, a `SIGBUS` would kill the interpreter, and records must be
copied into `bytes` anyway, so it would save little.

**GIL, threads and signals.** The GIL is released for `Writer` open (recovery), `checkpoint`, `close`, the append
loop of `append_many`, the read loops of `read_many`/`read_batch`, `seek`, inspection, and every wait. A single
`append` or `try_next` keeps the GIL (it is shorter than releasing it, except during a segment roll). `Writer` and
`Reader` may be used from any thread: each holds its core object in a mutex, acquired without holding the GIL, so
concurrent calls on one object serialize. The mutex is never held while Python code runs: exceptions, reports and
records are converted to Python objects after it is released. `closed`, `Reader.position` and `repr` read atomics and
never wait.

Waits run in slices of at most 50 ms, and a waiting read holds its reader's mutex only during a slice. Between slices it
checks a closed flag and calls `PyErr_CheckSignals` with the mutex released, so Ctrl-C raises `KeyboardInterrupt` within
about 50 ms, signal handlers may use the reader without deadlocking, and `close()` from another thread or a handler ends
the read with `ValueError` within a slice (before a waiting read reports a timeout, it runs pending signal handlers and
checks the flag again). The remaining wait is computed after the mutex is acquired, so a read queued behind another
waiter does not wait its full timeout again. Reads from several threads on one reader interleave between slices and
share its cursor: each record is returned to exactly one caller. A reader is a cursor, so share one between threads only
if they coordinate; use one reader per consumer.

Records are staged in a buffer in the reader: copied from the journal into it with the mutex held, they leave it only
when a read delivers them. A read checks for signals, builds its complete Python result from a copy of the front of
the buffer (the list of `Record`s, the `RecordBatch`, or the single `Record`) with the mutex released, checks for
signals again, and only then removes the records, after confirming (by a counter of buffer changes) that the front
still holds the records it built from. If a handler raises, or building fails, at any of these points, the records
stay staged and the next read returns them first. If another caller (a thread, or a read in a signal handler) took
them meanwhile, the read discards its result and looks at the journal again; this terminates because it only happens
when someone else advanced the cursor, and it means a read that does not wait returns nothing only when no committed
record is left. After the second signal check no Python code runs in the call. A signal that arrives after the call
has returned its result, before Python stores it, is handled by the interpreter as after any call, and the result is
lost; no binding can prevent that, and the documentation says so.

**Fork.** A handle records the id of the process that opened it and a fork generation, a counter that a hook
registered with `os.register_at_fork(after_in_child=...)` at import advances in every child. In any process where
either differs (a child created by `fork`, which `multiprocessing` uses by default on Linux before Python 3.14, or a
later descendant that happens to get the opening process's id), every method raises `UsedAfterForkError` before
taking a lock or touching the journal: the child's copy shares the parent's mappings, descriptors and writer lock but
not its helper threads, so using it could overwrite the parent's records or fail to join threads.

`close()`, `__exit__` and garbage collection in such a process release what the child holds and nothing else. The
`flock` on `writer.lock` belongs to the open file description that parent and child share, so a child that kept its
copy of the descriptor would keep the journal locked after the parent closed its writer. For the writer the bindings
call `tickertape::Writer::release_after_fork`, a small method added to the core crate for this: it closes the child's
descriptors and unmaps its mappings without unlocking the description (that would unlock the parent), syncing,
joining or signalling the helper threads (which do not exist in the child), or removing or changing any file or the
wake-up region; what the helper threads hold is leaked until the child exits. The core reader has no threads and
writes nothing, so the child simply drops it. A mutex that was locked when the process forked (another thread was
inside a call) stays locked in the child; then `close()` leaves the copy to garbage collection, which needs no lock.
Handles must be opened after forking, or the `spawn`/`forkserver` start method used.

**Errors.** Exceptions are defined in Python (`tickertape/_errors.py`) so they can also derive from built-ins:
`TickertapeError` is the base; `JournalIOError` (also `OSError`, with `errno`, `filename`, `op`, `storage_full`) and
its subclass `StorageFullError`; `LockedError`; `CorruptJournalError` (`path`, `offset`, a stable `reason` code for
each `CorruptReason`, `details`); `UnsupportedVersionError`; `JournalNotFoundError`; `SeqUnavailableError` (also
`LookupError`); `RecordTooLargeError` and `InvalidConfigError` (also `ValueError`); `DurabilityLostError`;
`SequenceExhaustedError`; `DiscardedError`; `BatchAppendError`; and `UsedAfterForkError` for the binding's own fork
check. Every documented attribute survives `pickle` (`TickertapeError.__reduce__` rebuilds from the message and
fields, because `OSError` keeps `errno`, `strerror` and `filename` outside `__dict__`); `strerror` is the OS message
without Rust's `(os error N)` suffix. Both core enums are `#[non_exhaustive]`, so the compiler cannot flag a new
variant. A Python test parses `crates/tickertape/src/error.rs` and fails if a variant is missing from the binding's
tables; Rust unit tests in the bindings crate convert an instance of every listed `Error` and `CorruptReason` variant
and fail if one does not map as listed. The same pair of checks covers `RecoveryAction` kinds and `SegmentEnd` codes.
Unmapped variants would still raise `TickertapeError` (reason `"unknown"`).

**Durability.** Unchanged and stated as in the crash contract: visible when `append` returns, durable when a later
`checkpoint` returns; `Reader(durable_only=True)` returns only records below the checkpoint marker's `next_seq`.
Including the contract's caveat: after a failed checkpoint, records appended before it may not be durable even if a
writer opened later in the same boot checkpoints successfully.

**Typing.** `_native.pyi` and `py.typed` ship in the wheel. CI runs `mypy --strict` and `mypy.stubtest` against the
built module.

**Unsafe code.** None: the crate sets `#![deny(unsafe_code)]`; PyO3's generated code compiles under it.

**Distribution.** `.github/workflows/python-wheels.yml` builds an sdist and wheels for manylinux and musllinux
(x86-64, aarch64) and macOS (x86-64, arm64), smoke-tests each native wheel, and uploads them as artefacts. It runs on
demand and on `v*` tags. Its PyPI step uses Trusted Publishing and runs only for a tag that `release.py prepare`
accepts, in the `pypi` environment, when the repository variable `PYPI_PUBLISHING` is `enabled`; it never runs on pull
requests or branches. The one-time setup is in `docs/playbooks/release-checklist.md`.

## Consequences

- Relation to [ADR 0031](0031-bindings-and-install-channels.md) (accepted the same day). Its criteria for a delivered
  binding are met as follows: the record and replay surface, mapped errors, documented ownership, the shared
  cross-language fixtures, a runnable example, and an install test of every built wheel on every supported platform.
  Its version rule ties bindings to the C ABI's version; these bindings use the Rust API, so their version follows
  the workspace version (checked by `release.py prepare`) and they declare no C ABI dependency. Wheels carry
  maturin's CycloneDX SBOM. With Trusted Publishing, the PyPI publish action uploads PEP 740 attestations by default.
  The CLI wheel that ADR 0031 also plans for PyPI can be added to `python-wheels.yml` later, with its own pending
  publisher.
- Python users can write, follow live and replay journals, and hand batches to NumPy, pandas, pyarrow or DuckDB.
- Each release must bump `bindings/python/Cargo.toml` and refresh its lockfile along with the workspace.
- A second lockfile to keep up to date; CI runs `cargo deny` on it too.
- Not covered: free-threaded wheels, Windows, PyPy, Arrow PyCapsule export, live zero-copy access.
- Tests: `bindings/python/tests/` (pytest) cover round trips, batches and partial failure, live following across
  processes, durable-only reads, rolls, corruption, Ctrl-C (including a record arriving during the interrupted wait and
  a signal while a large result is built), signal handlers that use or close the reader, cross-thread `close()`,
  threads sharing a writer or a reader, use after `fork`, a child releasing the journal while it still runs, reused
  process ids, timeouts, pickling of every exception, context managers, the writer lock and cross-language fixtures
  against the Rust CLI and the pure-Python decoder. `cargo test --manifest-path bindings/python/Cargo.toml` runs the
  conversion unit tests.
