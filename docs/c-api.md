# C API

tickertape has a C ABI for C, C++ and bindings in other languages (Go through cgo, Java through the FFM API, C#, and
others). It lives in `crates/tickertape-ffi`, builds `libtickertape` as a shared and a static library, and comes
with one header, [`crates/tickertape-ffi/include/tickertape.h`](../crates/tickertape-ffi/include/tickertape.h), that
compiles as C99 and C++11. The header documents every function; this page explains the rules they share. The design
and its reasons are in [ADR 0028](decisions/0028-c-abi.md).

> **Status:** the C ABI is not stable before tickertape 1.0. `TICKERTAPE_ABI_VERSION` is 0, and any minor release may
> change it (the changelog says when). See [Versioning](#versioning).

Examples: [`examples/c/`](../examples/c/README.md) (a C writer, a C reader that replays or follows live, and a C++
example with a small RAII wrapper header, `tickertape.hpp`).

```c
#include "tickertape.h"
#include <stdio.h>

int main(void) {
    tickertape_writer *writer;
    tickertape_reader *reader;
    tickertape_record record;
    uint64_t seq;

    if (tickertape_writer_open("/tmp/journal", NULL, &writer) != TICKERTAPE_OK) {
        fprintf(stderr, "open: %s\n", tickertape_last_error_message());
        return 1;
    }
    tickertape_writer_append(writer, 1, 0, tickertape_realtime_ns(), (const uint8_t *)"hello", 5, &seq);
    tickertape_writer_checkpoint(writer);         /* durable, not only visible */

    tickertape_reader_open("/tmp/journal", NULL, &reader);
    tickertape_reader_seek(reader, seq);
    if (tickertape_reader_next(reader, TICKERTAPE_NO_WAIT, &record) == TICKERTAPE_OK) {
        printf("%.*s\n", (int)record.payload_len, (const char *)record.payload);
    }
    tickertape_reader_close(reader);
    tickertape_writer_close(writer);
    return 0;
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

C++ uses the same header (it has `extern "C"` guards). [`examples/c/tickertape.hpp`](../examples/c/tickertape.hpp)
is a header-only C++11 wrapper you can copy. `examples/c/Makefile` and `examples/c/CMakeLists.txt` show both link
styles; the CMake file uses pkg-config when the library is installed.

## Conventions

- **Names:** functions and types start with `tickertape_`, constants with `TICKERTAPE_`.
- **Status:** every fallible function returns a `tickertape_status` (`int32_t`): `TICKERTAPE_OK` (0), or
  `TICKERTAPE_NONE` (1) when it succeeded with nothing to return, or a negative `TICKERTAPE_E_*` error. Test errors
  with `status < 0`.
- **Paths** are NUL-terminated byte strings, passed to the operating system unchanged.
- **Flags** (`int32_t enabled`) treat any non-zero value as true.
- **Timeouts** are `int64_t` nanoseconds: 0 (`TICKERTAPE_NO_WAIT`) does not wait, a positive value waits up to that
  long, a negative value (`TICKERTAPE_WAIT_FOREVER`) waits until something arrives.
- **Pointers** must be valid and aligned for their type. Optional output pointers are marked as such in the header.

## Ownership and lifetimes

| Object | Create | Free |
|---|---|---|
| Writer | `tickertape_writer_open` | `tickertape_writer_close` |
| Reader | `tickertape_reader_open`, `tickertape_reader_open_mapped_unsafe` | `tickertape_reader_close` |
| Writer options | `tickertape_writer_options_new` | `tickertape_writer_options_free` |
| Reader options | `tickertape_reader_options_new` | `tickertape_reader_options_free` |
| Retention settings | `tickertape_retention_new` | `tickertape_retention_free` |

- All objects are opaque handles. Free each exactly once with its function; passing NULL there does nothing.
- Options and retention settings are copied by the functions that take them, so they can be freed right after.
- **Records returned by `tickertape_reader_next` are borrowed views.** `record.payload` points into the reader's
  buffer (safe reader) or straight into the memory-mapped segment (mapped reader). It is valid until the next call
  that takes the same reader (any `tickertape_reader_*` function except `tickertape_reader_position`), or until the
  reader is closed. Copy what you need to keep.
- **Copy-out instead:** `tickertape_reader_next_copy` copies one payload into a buffer you own, and
  `tickertape_reader_read_batch` copies many records' payloads one after another into one buffer. The returned
  records point into your buffer, and stay valid as long as it does.
- When `payload_len` is 0, `payload` may be NULL or dangling; do not dereference it.
- Strings returned by `tickertape_version`, `tickertape_status_name` and `tickertape_corrupt_reason_name` are static.
  `tickertape_last_error_message` returns a per-thread string, valid until the next failing call on that thread.
  Never free them.

**Misuse.** NULL where a pointer is required returns `TICKERTAPE_E_INVALID_ARGUMENT`, as do out-of-range values and
non-zero reserved fields. A call on a handle that is already inside a call on the same thread (for example from a
fill callback, see below) returns `TICKERTAPE_E_BUSY` and does nothing. **Using a handle after closing it, closing it
twice, and calling into one handle from two threads at the same time are undefined behaviour and are not detected.**
The C++ wrapper and language bindings should make these impossible by construction.

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
  inside the mapped segment; you write the payload and return 0. Returning non-zero aborts the append
  (`TICKERTAPE_E_ABORTED`): nothing is committed, but the writer zeroes the buffer and syncs that to storage first
  (the format requires it), so aborting is slow. The callback runs on the calling thread, must not keep the buffer,
  must not call into the same writer, and must not unwind or `longjmp` out of the call. In C++ catch every exception
  inside the callback (the wrapper's `append_with` does this and rethrows after the call).
- `tickertape_writer_append_batch` appends an array of `tickertape_new_record`s in order. It checks every element
  first, so an argument error appends nothing. It then stops at the first failure and returns that error, with
  `*appended_out` records already appended (sequence numbers `*first_seq_out` onwards). Those records stay in the
  journal: retry from the failing one, not from the start.

## Reading

```c
tickertape_reader *reader;
tickertape_reader_open("/data/journal", NULL, &reader);      /* positioned at the oldest record */
tickertape_reader_seek(reader, 1000);                         /* next record returned is seq 1000 */
tickertape_record record;
for (;;) {
    tickertape_status s = tickertape_reader_next(reader, 100000000 /* 100 ms */, &record);
    if (s == TICKERTAPE_NONE) continue;                          /* nothing new yet */
    if (s == TICKERTAPE_E_DISCARDED) {                           /* recovery or retention replaced the segment */
        uint64_t at;
        tickertape_reader_position(reader, &at);
        tickertape_reader_seek(reader, at);
        continue;
    }
    if (s < 0) break;
    handle(&record);                                             /* record.payload is valid until the next call */
}
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

`tickertape_reader_read_batch` waits up to the timeout for the first record only, then takes what is already
committed, up to the array size or until the next payload does not fit in the buffer. A record that does not fit is
held back and returned first by the next call (`TICKERTAPE_E_BUFFER_TOO_SMALL` if even the first one does not fit,
with the size it needs in `records[0].payload_len`). If an error occurs after some records were read, the call
returns `TICKERTAPE_OK` with those records, and the next read call returns the error: records handed out are never
lost to an error.

## Durability

The C ABI uses the crash contract's two levels ([`crash-contract.md`](crash-contract.md)):

| Level | Reached when | Means |
|---|---|---|
| **Visible** | an append function returns `TICKERTAPE_OK` | Readers on this host can read the record, and it survives the writer process dying. It can still be lost in a power loss or OS crash, and the writer then reuses its sequence number |
| **Durable** | a later `tickertape_writer_checkpoint` returns `TICKERTAPE_OK` | The record is on stable storage, under the crash contract's storage assumptions |

Closing a writer does not checkpoint. A failed checkpoint poisons the writer: later checkpoints return
`TICKERTAPE_E_DURABILITY_LOST` while appends keep working; treat it as an incident.

Readers return visible records by default. A reader whose options have
`tickertape_reader_options_set_durable_only(options, 1)` returns only durable records: those below the checkpoint
marker's `next_seq` (crash contract C13). It treats records after that as not written yet (`TICKERTAPE_NONE`, or it
keeps waiting), returns nothing if there is no valid checkpoint marker, and never returns a record that a power loss
can remove.

## Errors

Each failing call records, for the calling thread only, what went wrong:

| Function | Returns |
|---|---|
| `tickertape_last_error_code()` | The status code (`TICKERTAPE_OK` if no call on this thread has failed) |
| `tickertape_last_error_message()` | A description with the file and operation involved |
| `tickertape_last_error_os_code()` | `errno` for `TICKERTAPE_E_IO` and `TICKERTAPE_E_STORAGE_FULL`, else 0 |
| `tickertape_last_error_corrupt_reason()` | A `TICKERTAPE_CORRUPT_*` reason for `TICKERTAPE_E_CORRUPT`, else 0 |

They behave like `errno`: successful calls leave them unchanged, so read them right after the call that failed.
`tickertape_status_name` and `tickertape_corrupt_reason_name` turn codes into names for logs.

| Code | Meaning |
|---|---|
| `TICKERTAPE_E_INVALID_ARGUMENT` | NULL required pointer, value out of range, non-zero reserved field |
| `TICKERTAPE_E_IO` | A system call failed (see `tickertape_last_error_os_code`) |
| `TICKERTAPE_E_STORAGE_FULL` | A system call failed with `ENOSPC` or `EDQUOT`; a failed append can be retried later |
| `TICKERTAPE_E_LOCKED` | Another writer has the journal open |
| `TICKERTAPE_E_CORRUPT` | Journal bytes failed validation; a reader's cursor does not move |
| `TICKERTAPE_E_UNSUPPORTED_VERSION` | A segment has an unsupported format version |
| `TICKERTAPE_E_NOT_FOUND` | The directory has no segments yet |
| `TICKERTAPE_E_SEQ_UNAVAILABLE` | The sequence number is older than the oldest segment |
| `TICKERTAPE_E_RECORD_TOO_LARGE` | The payload does not fit in a segment |
| `TICKERTAPE_E_DURABILITY_LOST` | An earlier checkpoint failed |
| `TICKERTAPE_E_SEQUENCE_EXHAUSTED` | Sequence numbers are exhausted |
| `TICKERTAPE_E_DISCARDED` | The reader's segment was replaced or removed by recovery or retention; call `tickertape_reader_seek` |
| `TICKERTAPE_E_INVALID_CONFIG` | A configuration value is out of range |
| `TICKERTAPE_E_BUFFER_TOO_SMALL` | A caller buffer or array is too small (the function says what size it needs) |
| `TICKERTAPE_E_ABORTED` | A fill callback returned non-zero |
| `TICKERTAPE_E_BUSY` | Re-entrant call on a handle that is already inside a call |
| `TICKERTAPE_E_PANIC` | An internal error (a bug in tickertape) was caught; the handle is broken, close it |
| `TICKERTAPE_E_UNKNOWN` | An error this ABI version has no code for |

Later versions may add codes; treat any unknown negative value as an error. No Rust panic ever unwinds into C: every
function catches panics and returns `TICKERTAPE_E_PANIC` (the library must be built with Rust's default
`panic = "unwind"`, which the crate enforces). The Rust panic message is also printed to standard error.

## Threading

- A writer or reader handle may be moved to another thread between calls, but must not be used by two threads at
  the same time. Synchronise yourself if you share one; a writer is single-threaded in this sense.
- Different readers are independent and can be used from different threads, in any number of processes.
- Option builders follow the same rule as handles. The free-standing functions (`tickertape_verify`,
  `tickertape_list_segments`, `tickertape_read_checkpoint`, `tickertape_retention_apply`, and the version and error
  functions) can be called from any thread.
- With the default options, a writer starts a background helper thread (prefaulting, and on macOS preparing the next
  segment); `tickertape_writer_options_set_prefault(options, 0)` gives a writer that starts no threads.
- **`fork()`:** do not use or close handles created before a fork in the child; the child should open its own
  handles or call `exec`. Two processes appending through one writer would corrupt the journal, and a writer's
  helper threads do not exist in the child. The library opens files with close-on-exec, so `exec` releases an
  inherited writer lock, but a child that neither execs nor exits keeps the journal locked.

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
`sizeof` the struct before the call.

## Versioning

- `TICKERTAPE_ABI_VERSION` in the header must equal `tickertape_abi_version()` from the loaded library; check it at
  start-up. `tickertape_version()` returns the library's version string.
- **Before 1.0** the ABI version is 0, and a minor release may change the ABI; the changelog says so when it does.
- **From 1.0** the ABI version is 1 and the shared library's soname is `libtickertape.so.1` (macOS:
  `libtickertape.1.dylib`). No function, constant or struct field is removed or changes meaning or layout. New
  functions, constants, option setters and status codes may be added.
- Configuration that may grow is behind opaque builders. The record structs (`tickertape_record`,
  `tickertape_new_record`), `tickertape_segment_info` and `tickertape_checkpoint` describe format version 1 and are
  frozen; a new format version would come with new types. Report structs grow at the end, guarded by `struct_size`.
  Reserved fields must be zero.

## Testing

`cargo test -p tickertape-ffi` runs Rust tests of the exported functions and compiles and runs C99 and C++11 test
programs and the examples against the committed header, including cross-language fixtures with the Rust reader and
the Python decoder ([`python/`](../python/README.md)). Set `TICKERTAPE_FFI_SANITIZE=1` to build the C programs with
AddressSanitizer and UBSan, or `TICKERTAPE_FFI_VALGRIND=1` to run them under Valgrind. After changing the C ABI,
regenerate the header with cbindgen 0.29.4 (the command is in `crates/tickertape-ffi/cbindgen.toml`); CI fails if it
differs.
