# C API

tickertape has a C ABI for C, C++ and bindings in other languages (Go through cgo, Java through the FFM API, C#, and
others). It lives in `crates/tickertape-ffi`, builds `libtickertape` as a shared and a static library, and comes
with one header, [`crates/tickertape-ffi/include/tickertape.h`](../crates/tickertape-ffi/include/tickertape.h), that
compiles as C99 and C++11. The header documents every function; this page explains the rules they share. The design
and its reasons are in [ADR 0028](decisions/0028-c-abi.md).

> **Status:** the C ABI is not stable before tickertape 1.0. `TICKERTAPE_ABI_VERSION` is 0, which means unstable: any
> release may change it (the changelog says when), so load exactly the library version your header came from. See
> [Versioning](#versioning).

Examples: [`examples/c/`](../examples/c/README.md) (a C writer, a C reader that replays or follows live, and a C++
example). **From C++, use the C++ binding** ([`cpp-api.md`](cpp-api.md)): the header-only `tickertape.hpp` next to
`tickertape.h`, with RAII handles, borrowed record views and errors as values or exceptions.

```c
#include "tickertape.h"
#include <stdio.h>

static int fail(const char *what, tickertape_status status) {
    fprintf(stderr, "%s: %s: %s\n", what, tickertape_status_name(status), tickertape_last_error_message());
    return 1;
}

int main(void) {
    tickertape_writer *writer;
    tickertape_reader *reader;
    tickertape_record record;
    tickertape_status s;
    uint64_t seq;

    if ((s = tickertape_writer_open("/tmp/journal", NULL, &writer)) != TICKERTAPE_OK) return fail("open", s);
    s = tickertape_writer_append(writer, 1, 0, tickertape_realtime_ns(), (const uint8_t *)"hello", 5, &seq);
    if (s == TICKERTAPE_OK) s = tickertape_writer_checkpoint(writer); /* durable, not only visible */
    if (s != TICKERTAPE_OK) {
        tickertape_writer_close(writer);
        return fail("append or checkpoint", s);
    }

    if ((s = tickertape_reader_open("/tmp/journal", NULL, &reader)) == TICKERTAPE_OK) {
        if ((s = tickertape_reader_seek(reader, seq)) == TICKERTAPE_OK &&
            (s = tickertape_reader_next(reader, TICKERTAPE_NO_WAIT, &record)) == TICKERTAPE_OK) {
            /* record.payload is NULL when payload_len is 0, and borrowed until the next call on the reader. */
            printf("%.*s\n", (int)record.payload_len, record.payload_len ? (const char *)record.payload : "");
        }
        tickertape_reader_close(reader);
    }
    tickertape_writer_close(writer);
    return s < 0 ? fail("read", s) : 0;
}
```

## Building and linking

The library is built from source with Cargo (Rust 1.85 or newer). There is no prebuilt library yet.

```bash
cargo build --release -p tickertape-ffi
# target/release/libtickertape.a and libtickertape.so (Linux) or libtickertape.dylib (macOS)
# header: crates/tickertape-ffi/include/tickertape.h
```

**Shared library.**

```bash
cc -std=c99 app.c -I crates/tickertape-ffi/include -L target/release -ltickertape \
   -Wl,-rpath,"$PWD/target/release" -o app
```

A plain `cargo build` gives the Linux library no soname, and on macOS an install name that is the absolute path of
the file in `target/release/deps`. That is fine for building and testing in place. To install it, use cargo-c, which
sets a soname (`libtickertape.so.0.4` before 1.0) or install name and writes a pkg-config file:

```bash
cargo install cargo-c
cargo cinstall --release -p tickertape-ffi --prefix /usr/local     # add --destdir for packaging
cc app.c $(pkg-config --cflags --libs tickertape) -o app
```

**Static library.** Link `libtickertape.a` and the system libraries the Rust standard library needs (`pkg-config
--static --libs tickertape` lists them too):

| Platform | Command |
|---|---|
| Linux (glibc) | `cc app.c -I crates/tickertape-ffi/include target/release/libtickertape.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc -o app` |
| macOS | `cc app.c -I crates/tickertape-ffi/include target/release/libtickertape.a -liconv -lc -lm -o app` |

The static library contains its own copy of the Rust standard library, so linking two Rust static libraries into one
program can produce duplicate symbols; link one of them dynamically in that case.

**macOS and the GCC driver.** The GCC driver (Homebrew `gcc`/`g++`) passes `-no_compact_unwind` to the linker, so
`libtickertape.a` linked that way loses its unwind information. No ordinary result depends on it: an append whose
fill callback returns non-zero returns without unwinding (ADR 0039), with `TICKERTAPE_E_ABORTED` when the clean-up
succeeds and `TICKERTAPE_E_IO` when it fails (see `tickertape_writer_append_with` below). The library still unwinds to
catch a Rust panic (a bug in tickertape, otherwise reported as `TICKERTAPE_E_PANIC`), so with that link such a panic
ends the process instead. Shared linkage, or the static library linked by Apple Clang (`cc`, `c++`), keeps the unwind
information.

C++ can use the same header (it has `extern "C"` guards), but the C++ binding
[`tickertape.hpp`](../crates/tickertape-ffi/include/tickertape.hpp) (C++17, header-only, [`cpp-api.md`](cpp-api.md))
wraps it with RAII and is the supported way. `examples/c/Makefile` shows both link styles (`writer` links the static
library, `reader` and `example` the shared one); `examples/c/CMakeLists.txt` links the shared library through the
CMake package in `crates/tickertape-ffi/cmake/`, which finds the repository's build or an installed library.

## Conventions

- **Names:** functions and types start with `tickertape_`, constants with `TICKERTAPE_`.
- **Status:** every fallible function returns a `tickertape_status` (`int32_t`): `TICKERTAPE_OK` (0), or
  `TICKERTAPE_NONE` (1) when it succeeded with nothing to return, or a negative `TICKERTAPE_E_*` error. Test errors
  with `status < 0`.
- **Paths** are NUL-terminated byte strings, passed to the operating system unchanged.
- **Flags** (`int32_t enabled`) treat any non-zero value as true.
- **Timeouts** are `int64_t` nanoseconds: 0 (`TICKERTAPE_NO_WAIT`) does not wait, a positive value waits up to that
  long, a negative value (`TICKERTAPE_WAIT_FOREVER`) waits until something arrives. A timeout bounds only the wait
  for a record to become available. Argument checks, catching up after a seek (the reader validates every record it
  skips) and file I/O are not interrupted, so a call can take longer than its timeout: a 1 ns timeout right after
  seeking far ahead took about 5 ms in a test.
- **Pointers** must be valid and aligned for their type. Optional output pointers are marked as such in the header.
- **Inputs and outputs of one call must not overlap**, and inputs (payloads, record arrays, paths) must stay valid
  and unmodified, by any thread, until the call returns. Output storage (records, counts, reports) is owned by the
  caller; only a `payload` pointer returned by `tickertape_reader_next` points into library memory. Each function's
  `# Safety` section in the header names the regions that must be disjoint. Where the check is cheap (appends, batch
  appends, copy-out and batch reads, segment listing), an overlap returns `TICKERTAPE_E_INVALID_ARGUMENT`; elsewhere
  it is undefined behaviour.
- **Memory:** functions that allocate (the `*_new` functions, and the open functions for their handles) never return
  NULL or an error for lack of memory: like any Rust program, the library aborts the process if an allocation
  fails.

## Ownership and lifetimes

| Object | Create | Free |
|---|---|---|
| Writer | `tickertape_writer_open` | `tickertape_writer_close` |
| Reader | `tickertape_reader_open`, `tickertape_reader_open_mapped_unsafe` | `tickertape_reader_close` |
| Writer options | `tickertape_writer_options_new` | `tickertape_writer_options_free` |
| Reader options | `tickertape_reader_options_new` | `tickertape_reader_options_free` |
| Retention settings | `tickertape_retention_new` | `tickertape_retention_free` |

- All objects are opaque handles. Free each exactly once with its function; passing NULL there does nothing. After a
  close returns any status other than `TICKERTAPE_E_BUSY` (including `TICKERTAPE_E_PANIC`), the handle is freed:
  never retry a close that failed, or it would free twice. `TICKERTAPE_E_BUSY` (closing a writer from its own fill
  callback) closes nothing and the handle stays valid.
- Options and retention settings are copied by the functions that take them, so they can be freed right after.
- **Records returned by `tickertape_reader_next` are borrowed views.** `record.payload` points into the reader's
  buffer (safe reader) or straight into the memory-mapped segment (mapped reader). It is valid until the next call
  that takes the same reader (any `tickertape_reader_*` function except `tickertape_reader_position`), or until the
  reader is closed. Copy what you need to keep.
- **Copy-out instead:** `tickertape_reader_next_copy` copies one payload into a buffer you own, and
  `tickertape_reader_read_batch` copies many records' payloads one after another into one buffer. The returned
  records point into your buffer, and stay valid as long as it does.
- When `payload_len` is 0, `payload` is NULL (from every read function). Guard it before passing it to functions
  such as `printf("%.*s")` or `memcpy` that must not receive NULL.
- Strings returned by `tickertape_version`, `tickertape_status_name` and `tickertape_corrupt_reason_name` are static.
  `tickertape_last_error_message` returns a per-thread string, valid until the next failing call on that thread.
  Never free them.
- Extensible structs (`tickertape_verify_report`, `tickertape_retention_report`) start with a `uint64_t struct_size`:
  set it to `sizeof` the struct before the call; after the call it holds the number of bytes the library filled (see
  [Versioning](#versioning)).

**Misuse.** NULL where a pointer is required returns `TICKERTAPE_E_INVALID_ARGUMENT`, as do out-of-range values and
non-zero reserved fields. A call on a handle that is already inside a call on the same thread (for example from a
fill callback, see below) returns `TICKERTAPE_E_BUSY` and does nothing. **Using a handle after closing it, closing it
twice, and calling into one handle from two threads at the same time are undefined behaviour and are not detected.**
The C++ binding (move-only handles) and other language bindings should make these impossible by construction.

## Writing

```c
tickertape_writer_options *options = tickertape_writer_options_new();
tickertape_writer_options_set_segment_size(options, 64 * 1024 * 1024);
tickertape_writer *writer;
tickertape_status status = tickertape_writer_open("/data/journal", options, &writer);
tickertape_writer_options_free(options);
```

- `tickertape_writer_append` copies the payload into the journal and returns the record's sequence number.
- `tickertape_writer_append_with` avoids that copy: it calls your `tickertape_fill_fn` once with a zeroed buffer
  inside the mapped segment (NULL for an empty payload); you write the payload and return 0. Returning non-zero
  aborts the append (`TICKERTAPE_E_ABORTED`): nothing is committed, but the writer zeroes the buffer and syncs that to
  storage first (the format requires it), so aborting is slow. Aborting does not unwind inside the library: the
  library returns from the call as from any other error. If that sync fails, the call returns
  `TICKERTAPE_E_IO` with the OS error instead (even for `ENOSPC`), and the writer refuses further appends and
  checkpoints: a storage incident, see [A failed clean-up](#a-failed-clean-up-after-an-aborted-fill). The callback
  runs on the calling thread, must not keep the buffer, and must not call into the same writer (that, including
  closing it, returns `TICKERTAPE_E_BUSY`). It must return normally. An exception that escapes it is unsupported:
  Rust leaves it unspecified whether a foreign exception reaching its unwinding machinery terminates the process (it
  did on the platforms tested) or is caught as an opaque error (the call would then fail with the exception lost),
  so do not rely on either. `longjmp` out of the callback is undefined behaviour. In C++ catch every exception inside
  the callback and return non-zero (the wrapper's `append_with` does this and rethrows after the call; if the
  clean-up failed, it throws the library error instead, with your exception nested inside it).
- `tickertape_writer_append_batch` appends an array of `tickertape_new_record`s in order. It checks every element
  first, so an argument error appends nothing (the message names the element's index). It then stops at the first
  failure and returns that error, with `*appended_out` records already appended (sequence numbers `*first_seq_out`
  onwards; `*appended_out` is updated as each record commits). Those records stay in the journal: retry from the
  failing one, not from the start.

## Reading

```c
tickertape_reader *reader;
tickertape_record record;
tickertape_status s = tickertape_reader_open("/data/journal", NULL, &reader); /* at the oldest record */
if (s == TICKERTAPE_OK) s = tickertape_reader_seek(reader, 1000);              /* next record is seq 1000 */
while (s >= 0 && !stop_requested()) {
    s = tickertape_reader_next(reader, 100000000 /* 100 ms */, &record);
    if (s == TICKERTAPE_NONE) continue;                    /* nothing new yet: check for a stop request */
    if (s == TICKERTAPE_E_DISCARDED) {                     /* recovery or retention replaced the segment */
        uint64_t at;
        if ((s = tickertape_reader_position(reader, &at)) == TICKERTAPE_OK) s = tickertape_reader_seek(reader, at);
        if (s == TICKERTAPE_E_SEQ_UNAVAILABLE) {
            /* Retention removed the records from `at` on before they were read. Either stop here (s < 0 ends the
             * loop), or continue at the oldest record left: the first segment's base_seq from
             * tickertape_list_segments (examples/c/reader.c does this). */
        }
        continue;                                          /* never retry a failed seek in a loop */
    }
    if (s == TICKERTAPE_OK) handle(&record);               /* record.payload is valid until the next call */
}
if (s < 0) fprintf(stderr, "%s: %s\n", tickertape_status_name(s), tickertape_last_error_message());
```

There are two readers:

- `tickertape_reader_open`, the **safe reader**, reads the files with `pread` into a buffer it owns and validates
  the copy. It is memory-safe whatever other processes do to the files and never receives `SIGBUS`.
- `tickertape_reader_open_mapped_unsafe`, the **mapped reader**, returns payloads that point straight into a read-only
  memory mapping, without a copy. **Precondition: while it exists, no process other than a tickertape writer may
  modify the journal's segment files.** A tickertape writer never changes committed bytes, including during
  recovery, so this holds in normal operation. If something else writes into or truncates a segment, payloads can
  change under you, and truncation kills the process with `SIGBUS`. The `_unsafe` suffix is there so that this is
  never chosen by accident.

Both validate every record (length, CRC32C, sequence number) before returning it, follow segment rolls, and wait with
the configured back-off, blocking on the writer's wake-up region when it is available.

**Starting before the writer.** A reader that opens a journal before its writer has started gets `TICKERTAPE_E_IO`
with `tickertape_last_error_os_code() == ENOENT`, because the writer creates the directory. While the writer is
creating the first segment, the directory exists but is empty, and the open returns `TICKERTAPE_E_NOT_FOUND`. A loop
that waits for the writer should retry on exactly these two and fail on anything else.

**Cancelling a wait.** A call blocked in `tickertape_reader_next` with `TICKERTAPE_WAIT_FOREVER` cannot be
interrupted, and closing the reader from another thread while it waits is undefined behaviour. To make a reader
stoppable (a binding that offers cancellation, a service that shuts down), loop with a finite timeout and check for
a stop request between calls.

`tickertape_reader_read_batch` waits up to the timeout for the first record only, then takes what is already
committed, up to the array size or until the next payload does not fit in the buffer. A record that does not fit is
held back and returned first by the next call (`TICKERTAPE_E_BUFFER_TOO_SMALL` if even the first one does not fit,
with the size it needs in `records[0].payload_len`). If an error occurs after some records were read, the call
returns `TICKERTAPE_OK` with those records, and the next read call (`next`, `next_copy` or `read_batch`) returns the
error: records handed out are never lost to an error. A successful `tickertape_reader_seek` discards a held-back
record and a deferred error; a failed seek changes nothing. The reader stays at the failing position, so a
persistent error (corruption) is returned again by the following calls until you seek elsewhere.

## Durability

The C ABI uses the crash contract's two levels ([`crash-contract.md`](crash-contract.md)):

| Level | Reached when | Means |
|---|---|---|
| **Visible** | an append function returns `TICKERTAPE_OK` | Readers on this host can read the record, and it survives the writer process dying. It can still be lost in a power loss or OS crash, and the writer then reuses its sequence number |
| **Durable** | a later `tickertape_writer_checkpoint` returns `TICKERTAPE_OK` | The record is on stable storage, under the crash contract's storage assumptions |

Closing a writer does not checkpoint. A failed checkpoint poisons the writer: later checkpoints return
`TICKERTAPE_E_DURABILITY_LOST` while appends keep working; treat it as an incident. A failed clean-up of an aborted
fill is the same kind of incident, and also stops appends (see
[A failed clean-up](#a-failed-clean-up-after-an-aborted-fill)).

Readers return visible records by default. A reader whose options have
`tickertape_reader_options_set_durable_only(options, 1)` returns only durable records: those below the checkpoint
marker's `next_seq` (crash contract C13). It treats records after that as not written yet (`TICKERTAPE_NONE`, or it
keeps waiting) and returns nothing if there is no valid checkpoint marker. It never returns a record that a power
loss can remove **under the crash contract's conditions**: the filesystem and drive honour the sync calls (no lying
write caches); a block made durable is not damaged by an interrupted later write to the same block; and no
checkpoint, and no clean-up of an aborted fill, has failed earlier in the same boot (a failed sync can drop data that
a later successful checkpoint, even by a new writer, does not detect). See
[`crash-contract.md`](crash-contract.md) → Storage assumptions for durability, and → Errors are sticky.

## Errors

Each failing call records, for the calling thread only, what went wrong:

| Function | Returns |
|---|---|
| `tickertape_last_error_code()` | The status code (`TICKERTAPE_OK` if no call on this thread has failed) |
| `tickertape_last_error_message()` | A description with the file and operation involved |
| `tickertape_last_error_os_code()` | `errno` for `TICKERTAPE_E_IO` and `TICKERTAPE_E_STORAGE_FULL`, else 0 (also `ENOSPC` for an append refused after a failed clean-up, which is `TICKERTAPE_E_IO`) |
| `tickertape_last_error_corrupt_reason()` | A `TICKERTAPE_CORRUPT_*` reason for `TICKERTAPE_E_CORRUPT`, else 0 |

They behave like `errno`: successful calls leave them unchanged, so read them right after the call that failed.
`tickertape_status_name` and `tickertape_corrupt_reason_name` turn codes into names for logs. The message is always
UTF-8: a file name that is not valid UTF-8 appears with its invalid bytes replaced (U+FFFD), so do not parse paths
out of it.

The corruption reasons are the `tickertape_corrupt_reason` values in the header, one for each core `CorruptReason`;
new reasons get new values and existing values never change. A reason's detail (a sequence number, a length, a
segment) is in the message only. Two reasons concern an empty commit word followed by what recovery treats as committed records (ADR 0035):
`TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD` (14; the message names the sequence number the evidence carries: a later
record's, a final seal's next sequence number or a later segment's base) and `TICKERTAPE_CORRUPT_UNCHECKED_TAIL`
(15; the search stopped at its work bound before it could decide, and the writer treats the bytes as committed
history all the same). `tickertape_verify` reports both as `first_problem_reason`; a writer opened with
`TICKERTAPE_RECOVERY_FAIL` returns `TICKERTAPE_E_CORRUPT` with them.

**Binding authors:** the state is per **operating-system thread**. A runtime that can move a task to another OS
thread between two foreign calls (Go goroutines, Java virtual threads) can read another thread's state, or none. Read
the details on the same OS thread, in the same foreign call sequence as the failing call: in Go, call through a small C
function in the cgo preamble that makes the call and captures the code, message, OS error and corruption reason
together (or hold `runtime.LockOSThread` across the call and the reads); in Java, read them in the same sequence of
downcalls, with no Java code in between that can block, as the Java binding does
([ADR 0037](decisions/0037-java-binding.md)).

| Code | Meaning |
|---|---|
| `TICKERTAPE_E_INVALID_ARGUMENT` | NULL required pointer, value out of range, non-zero reserved field |
| `TICKERTAPE_E_IO` | A system call failed (see `tickertape_last_error_os_code`) |
| `TICKERTAPE_E_STORAGE_FULL` | A system call failed with `ENOSPC` or `EDQUOT`; a failed append can be retried later (never used for an append refused after a failed clean-up) |
| `TICKERTAPE_E_LOCKED` | Another writer has the journal open |
| `TICKERTAPE_E_CORRUPT` | Journal bytes failed validation; a reader's cursor does not move |
| `TICKERTAPE_E_UNSUPPORTED_VERSION` | A segment has an unsupported format version |
| `TICKERTAPE_E_NOT_FOUND` | The directory exists but has no segments yet (a missing directory is `TICKERTAPE_E_IO` with `ENOENT`) |
| `TICKERTAPE_E_SEQ_UNAVAILABLE` | The sequence number is older than the oldest segment |
| `TICKERTAPE_E_RECORD_TOO_LARGE` | The payload does not fit in a segment |
| `TICKERTAPE_E_DURABILITY_LOST` | An earlier checkpoint, or the clean-up after an aborted fill, failed |
| `TICKERTAPE_E_SEQUENCE_EXHAUSTED` | Sequence numbers are exhausted |
| `TICKERTAPE_E_DISCARDED` | The reader's segment was replaced or removed by recovery or retention; call `tickertape_reader_seek` |
| `TICKERTAPE_E_INVALID_CONFIG` | A configuration value is out of range |
| `TICKERTAPE_E_BUFFER_TOO_SMALL` | A caller buffer or array is too small (the function says what size it needs) |
| `TICKERTAPE_E_ABORTED` | A fill callback returned non-zero (and the writer cleaned up) |
| `TICKERTAPE_E_BUSY` | Re-entrant call on a handle that is already inside a call |
| `TICKERTAPE_E_PANIC` | An internal error (a bug in tickertape) was caught; the handle is broken, close it |
| `TICKERTAPE_E_BEACON_IN_USE` | A reader beacon's name is held by a running reader (no C function creates beacons yet) |
| `TICKERTAPE_E_UNKNOWN` (-255) | An error this ABI version has no code for |

### A failed clean-up after an aborted fill

When a fill callback aborts, the writer zeroes the abandoned bytes and syncs the zeroing. If that sync fails, the
aborting `tickertape_writer_append_with` returns `TICKERTAPE_E_IO` instead of `TICKERTAPE_E_ABORTED`, with the OS
error in `tickertape_last_error_os_code()` and a message saying the clean-up failed. The code is `TICKERTAPE_E_IO`
even when the OS error is `ENOSPC` or `EDQUOT`: `TICKERTAPE_E_STORAGE_FULL` would invite a retry once space is
available, and that never helps here.

Treat it as a storage incident, in the same way as a failed checkpoint ([`crash-contract.md`](crash-contract.md) →
Errors are sticky):

- The writer refuses every later append (`TICKERTAPE_E_IO` with the same OS error and a message naming the cause)
  and every checkpoint (`TICKERTAPE_E_DURABILITY_LOST`, whose message names the aborted fill).
- The durability of everything appended since the last successful checkpoint is unknown.
- Closing the writer and reopening the journal gives a writer that accepts appends again, but **no guarantee about
  that tail across a power loss**, even if the new writer's checkpoints succeed. After a write-back failure the page
  cache can hold the zeroed bytes as clean pages while the disk still holds the stale ones; recovery then reads zeros
  and has no reason to rewrite them, so reopening does not establish that the clean-up reached the disk.

Later versions may add codes; treat any unknown negative value as an error. No Rust panic unwinds into C: every
function catches panics and returns `TICKERTAPE_E_PANIC` (the library must be built with Rust's default
`panic = "unwind"`, which the crate enforces). The exception is the static library linked by the GCC driver on macOS,
where a panic ends the process instead (see [Building and linking](#building-and-linking)). The Rust panic message is
also printed to standard error.

## Threading

- A writer or reader handle may be moved to another thread between calls, but must not be used by two threads at
  the same time. Synchronise yourself if you share one; a writer is single-threaded in this sense.
- Different readers are independent and can be used from different threads, in any number of processes.
- The last-error state is per OS thread; see [Errors](#errors) for runtimes that move tasks between threads.
- Option builders follow the same rule as handles. The free-standing functions (`tickertape_verify`,
  `tickertape_list_segments`, `tickertape_read_checkpoint`, `tickertape_retention_apply`, and the version and error
  functions) can be called from any thread.
- With the default options, a writer starts background helper threads (prefaulting, on Linux early write-back, and
  on macOS preparing the next segment; on Linux each resets its CPU affinity to the process's CPUs when it starts,
  ADR 0038); `tickertape_writer_options_set_prefault(options, 0)` gives a writer that starts no threads.
- **`fork()`:** do not use or close handles created before a fork in the child. Two processes appending through one
  writer would corrupt the journal, and a writer's helper threads do not exist in the child. A child of a
  multithreaded process (as a process with a writer usually is, through its helper thread) may call only
  async-signal-safe functions until it calls `exec`, and opening a handle allocates memory: so a child opens its own
  handles only after `exec`, and a child that does not `exec` must not call any tickertape function. The library
  opens files with close-on-exec, so `exec` closes the child's copy of the writer lock's descriptor, but a child that
  neither execs nor exits keeps the journal locked once the parent has exited without closing the writer, for its
  whole life if it never execs. Closing the writer in the parent releases the lock even while such a child exists.

## Inspection and retention

- `tickertape_verify` checks every segment and fills a `tickertape_verify_report` (number of records, first and
  last sequence numbers, number of problems and the first one). Corruption is reported in the report, not as an error.
- `tickertape_list_segments` lists the segments (call it with NULL and 0 first to learn how many there are).
- `tickertape_read_checkpoint` reads the checkpoint marker (`TICKERTAPE_NONE` if there is no valid one).
- Retention (ADR 0016) removes a sealed, checkpoint-covered prefix of old segments. Build `tickertape_retention`
  settings with limits (`max_segments`, `max_bytes`, `max_age_ns`) and an action, which must be set explicitly:
  `tickertape_retention_set_delete` or `tickertape_retention_set_move_to`. Pass them to
  `tickertape_writer_options_set_retention` to run after every checkpoint (errors go to
  `tickertape_writer_last_retention_error`), or to `tickertape_retention_apply` to run once (`dry_run` to only plan).

The reports `tickertape_verify_report` and `tickertape_retention_report` start with `struct_size`: set it to
`sizeof` the struct before the call (see [Versioning](#versioning) for what the library writes back).

## Versioning

The header defines `TICKERTAPE_ABI_VERSION`, `TICKERTAPE_VERSION_MAJOR`, `_MINOR`, `_PATCH` and
`TICKERTAPE_VERSION_NUMBER` (`MAJOR * 1000000 + MINOR * 1000 + PATCH`, so 0.4.0 is 4000). The library returns its
own values from `tickertape_abi_version()` and `tickertape_version_number()` (and the string from
`tickertape_version()`). Check them at start-up:

```c
uint32_t lib = tickertape_version_number();
int compatible = tickertape_abi_version() == TICKERTAPE_ABI_VERSION &&
                 (TICKERTAPE_ABI_VERSION == 0 ? lib == TICKERTAPE_VERSION_NUMBER   /* before 1.0: exactly */
                                              : lib >= TICKERTAPE_VERSION_NUMBER); /* from 1.0: at least */
```

- **Before 1.0** the ABI version is 0, which means *unstable*: any release may change the ABI (the changelog says
  so). A program or binding must load exactly the library version its header came from.
- **From 1.0** the ABI version is 1 and the shared library's soname is `libtickertape.so.1` (macOS:
  `libtickertape.1.dylib`). No function, constant or struct field is removed or changes meaning or layout; new
  functions, constants, option setters, status codes and fields at the end of extensible structs may be added. A
  program therefore works with any 1.x library at least as new as its header, and the check above refuses an older
  library, which may lack functions or fields the program uses. A release that breaks the ABI would be 2.0, with ABI
  version 2 and a new soname.
- Configuration that may grow is behind opaque builders. The record structs (`tickertape_record`,
  `tickertape_new_record`), `tickertape_segment_info` and `tickertape_checkpoint` describe format version 1 and are
  frozen; a new format version would come with new types. Reserved fields must be zero.
- **Extensible structs** (`tickertape_verify_report`, `tickertape_retention_report`) start with a `uint64_t
  struct_size`. Each has a minimum size, its size in the first stable release (72 and 32 bytes), below which the call
  returns `TICKERTAPE_E_INVALID_ARGUMENT`. The library writes `min(struct_size, its own size)` bytes and stores the
  number of bytes it filled back into `struct_size`. So a program built against an older, smaller struct works with
  a newer library, and a program built against a newer, larger struct can tell that an older library did not write
  the newer fields: check `struct_size` after the call before reading a field added later.
- Nothing removes a frozen declaration by accident: Rust and the C tests assert every struct's size and field
  offsets, and from 1.0 CI compares the header with the released one (`crates/tickertape-ffi/abi/`), failing if a
  frozen declaration is removed or changed.

## Testing

`cargo test -p tickertape-ffi` runs Rust tests of the exported functions and compiles and runs the C test program
(as C99 and C11), a C++11 and C++20 check of the C header, the C++ binding's tests ([`cpp-api.md`](cpp-api.md) →
Testing) and the examples against the committed headers, including cross-language fixtures with the Rust reader and
the Python decoder ([`python/`](../python/README.md)). Set `TICKERTAPE_FFI_SANITIZE=1` to build the C programs with
AddressSanitizer and UBSan, or `TICKERTAPE_FFI_VALGRIND=1` to run them under Valgrind. After changing the C ABI,
regenerate the header with cbindgen 0.29.4 (the command is in `crates/tickertape-ffi/cbindgen.toml`); CI fails if it
differs.
