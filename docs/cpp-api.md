# C++ API

tickertape has a header-only C++ binding over its C ABI: one header,
[`crates/tickertape-ffi/include/tickertape.hpp`](../crates/tickertape-ffi/include/tickertape.hpp), next to the C
header `tickertape.h`, linked against the same `libtickertape`. It needs C++17 and nothing beyond the standard
library, and has the same API with and without exceptions; with GCC and Clang, code built with and without
exceptions (or debug checks) can be linked into one program, though binding types cannot be passed between code built
in different exception modes ([Mixing modes](#mixing-modes)). This page explains how to use it; the header documents
every function, and [ADR 0036](decisions/0036-cpp-binding.md) gives the design and its reasons. The rules of the C
ABI underneath ([`c-api.md`](c-api.md)) still hold; this page repeats the ones you need.

> **Status:** like the C ABI, the binding is not stable before tickertape 1.0. Use the header, the C header and the
> library from one release (the header checks this), and one release of the header in every part of a program (see
> [Versioning](#versioning)).

Example: [`examples/c/example.cpp`](../examples/c/example.cpp) records, follows a journal live from another process,
and replays it.

```cpp
#include "tickertape.hpp"
#include <cstdio>

namespace tt = tickertape;
using namespace std::chrono_literals;

int main() {
    tt::Writer writer = tt::Writer::open("/tmp/journal").value();   // throws tt::Exception on error
    std::uint64_t seq = writer.append(1, 0, tt::realtime_ns(), std::string_view("hello")).value();
    writer.checkpoint().value();                                    // durable, not only visible

    tt::Reader reader = tt::Reader::open("/tmp/journal").value();
    reader.seek(seq).value();
    if (std::optional<tt::RecordView> record = reader.next(100ms).value()) {
        // The payload is borrowed: valid until the next call on `reader`.
        std::printf("%llu %.*s\n", (unsigned long long)record->seq(), (int)record->size(),
                    record->as_string_view().data());
    }
}
```

Without exceptions, test the result instead of calling `value()`:

```cpp
tt::result<tt::Writer> opened = tt::Writer::open("/tmp/journal");
if (!opened) {
    std::fprintf(stderr, "%s\n", opened.error().to_string().c_str());   // "TICKERTAPE_E_LOCKED: ..."
    return 1;
}
tt::Writer writer = std::move(opened).value();
```

## Building and linking

Build the library with Cargo (Rust 1.85 or newer), then compile as C++17 or later against the include directory:

```bash
cargo build --release -p tickertape-ffi
c++ -std=c++17 app.cpp -I crates/tickertape-ffi/include -L target/release -ltickertape \
    -Wl,-rpath,"$PWD/target/release" -o app
```

Static linking, installation with `cargo cinstall` (which installs `tickertape.hpp` next to `tickertape.h`) and
pkg-config work as for C: see [`c-api.md` → Building and linking](c-api.md#building-and-linking).

> **macOS and the static library:** the GCC driver (Homebrew `g++`) passes `-no_compact_unwind` to the linker, which
> drops the static library's unwind information. An `append_with` whose fill returns `false` or throws does not need
> it: the binding catches the exception in its callback, and the library aborts the append without unwinding
> ([ADR 0039](decisions/0039-aborted-fill-without-unwinding.md)). A Rust panic inside the library (a bug, otherwise
> `Status::panic`) does unwind, so with that link it ends the process instead. Shared linkage, or Apple Clang's
> `c++`, keeps the unwind information.

**CMake.** `crates/tickertape-ffi/cmake/tickertapeConfig.cmake` defines `tickertape::tickertape` (the shared library
and `tickertape.h`) and `tickertape::cpp` (plus `tickertape.hpp` and the C++17 requirement):

```cmake
find_package(tickertape CONFIG REQUIRED PATHS /path/to/tickertape/crates/tickertape-ffi/cmake)
# or: add_subdirectory(/path/to/tickertape/crates/tickertape-ffi tickertape)
target_link_libraries(app PRIVATE tickertape::cpp)
```

It finds the library in the repository's `target/release` (set `TICKERTAPE_PROFILE` for another profile), then in
the usual system places, such as an installed library. Set `TICKERTAPE_LIBRARY` and `TICKERTAPE_INCLUDE_DIR` to
choose the files yourself. `examples/c/CMakeLists.txt` uses it.

**Compilers.** Tested with GCC 15 and Clang 21 on Linux and Apple Clang on macOS, as C++17, C++20 and C++23, with
and without `-fno-exceptions`, with and without `NDEBUG`, under `-Wall -Wextra -Wpedantic -Wconversion -Wshadow
-Werror`. CI adds the GCC and Clang of its Ubuntu runner. The header needs `<filesystem>` (GCC 9, Clang 9, or a
macOS 10.15 deployment target).

### Mixing modes

A program may link translation units built with and without exceptions, and with and without debug checks (for
example a component built with `-fno-exceptions` inside an application that catches `tickertape::Exception`). With
GCC and Clang each unit keeps its own behaviour:

- **Exceptions on or off.** Everything in the header lives in an inline namespace named after the mode:
  `tickertape::v1` with exceptions, `tickertape::v1_noexcept` without. You still write `tickertape::Writer`, but
  every definition the header provides has a different symbol in each mode (including the clean-up code the compiler
  generates, which an allocation failure passes through), and the header instantiates no standard-library function
  that an exception can pass through over only its C types: the C structs it owns are held in a buffer type of its
  own. What the two modes still share is standard-library code over built-in and standard types (`std::string`,
  `std::optional<std::uint64_t>`, ...), which any program that mixes exception modes shares with or without this
  header: mixing modes is only as safe as your toolchain's standard library makes it. The same goes for containers
  your own code instantiates over the binding's C types: `tickertape::NewRecord` is the C struct, outside the mode
  namespace, so a `std::vector<tickertape::NewRecord>` used in both modes is one shared instantiation. CI runs the
  mixed-mode test with GCC and Clang on Linux and Apple Clang on macOS; Homebrew GCC on macOS was run by hand.
- **Binding types do not cross that boundary.** A `tickertape::Writer` from a unit built with exceptions is a
  different type from one built without. If a header of yours declares `void feed(tickertape::Writer &)`, defining
  it in a `-fno-exceptions` component and calling it from an application built with exceptions fails to link
  (undefined reference to `feed(tickertape::v1::Writer&)`). The link error only helps with the commonest case, a
  binding type in the **parameter list** of an ordinary C++ function that is called directly by its name. Do not
  rely on it anywhere else: a binding type is often not part of the symbol's name, for example when it appears only
  in a non-template function's **return type** (`tickertape::Writer open_feed();`), in a data member or a global,
  or in an `extern "C"` function (whose name carries no types); what a function template's symbol encodes depends
  on how the type appears in it; and a call through a virtual function or a function pointer is not resolved by
  name at all (an
  interface such as `struct Sink { virtual bool feed(tickertape::Writer &) = 0; };` implemented in one mode and
  called from the other links). Such a crossing links, and is undefined behaviour. Do not let binding types cross the
  boundary at all; pass C
  handles (`writer.native_handle()`, used through the C API in `tickertape.h` on the other side) or plain data
  (sequence numbers, timestamps, payload bytes; `tickertape::NewRecord`, which is the C struct).
- **Debug checks on or off.** Debug and release units share the types and can pass them freely; the few inline
  functions whose behaviour depends on the check (`operator*`, `error()`, `RecordView::data()`,
  `RecordBatch::operator[]`, ...) get a different symbol in each mode (`abi_tag`).
- Units that exchange binding types must use the same C++ standard library with the same layout-affecting settings
  (such as `_GLIBCXX_USE_CXX11_ABI` or a standard-library debug mode), as for any C++ library.

What it does not change:

- With debug checks, where the types are shared, behaviour follows the code that acts on a value: a view made in a
  release unit is checked when a debug unit reads its payload, and not the other way round.
- A fill function passed to `append_with` from code built without exceptions must not throw.
- Your own inline functions that call debug-checked ones are yours to keep consistent across modes, as for any
  header.
- MSVC is not covered (no `abi_tag`): there, build the whole program in one mode.

## Conventions

- **Names:** everything is in `namespace tickertape`. Classes are `PascalCase`, functions and enumerators
  `snake_case`. The C names stay available through `tickertape.h`.
- **Results:** every fallible function returns `tickertape::result<T>`: a `T` or a `tickertape::Error`. Test it with
  `if (r)` or `r.has_value()`; take the value with `value()` (which throws `tickertape::Exception` for an error, or
  without exceptions prints the error and aborts), or with `*r` and `r->` (unchecked, except in debug builds); read
  the error with `r.error()`. `result<void>` has no value. Results are `[[nodiscard]]`: ignoring one is a compiler
  warning, so errors are never silently lost. On a temporary result, `value()`, `*` and `error()` return by value
  (copies from a `const` temporary, and not at all for a `const` temporary of a type that cannot be copied, such as
  `result<Writer>`), so `for (auto &s : list_segments(dir).value())` is safe. Likewise `error().message()` on a
  temporary returns the string by value. A `result<T>` needs a `T` whose move cannot throw.
- **Nothing else throws**, apart from `std::bad_alloc` (an error copies its message; `list_segments` returns a
  vector). The only other exceptions are from `value()`, and from `Writer::append_with`, which rethrows what your
  fill function threw.
- **Paths** can be a `const char *`, `std::string`, `std::string_view` or `std::filesystem::path`, passed directly
  as the argument; temporaries such as `tt::Writer::open(root / "journal")` are fine. A path with a NUL byte inside
  fails with `invalid_argument` (the C ABI would cut it short). The parameter type behind this is internal
  (`tickertape::detail::PathArg`): it can point into the temporary you pass, which is safe in these call forms only.
  Do not name it; code that does can store one and let it dangle. A function of yours that forwards a path should
  take a path type such as `const std::filesystem::path &`, or a template parameter.
- **Payloads** can be any contiguous range of `char`, `unsigned char`, `signed char` or `std::byte`
  (`std::string_view`, `std::string`, `std::vector<std::uint8_t>`, `std::array`, `std::span`, `char8_t` ranges in
  C++20), or a pointer and a size. String literals and other C arrays are refused, because they would include the
  terminating NUL: write `std::string_view("text")`.
- **Timeouts** are a `tickertape::Timeout`: `tickertape::no_wait` (the default), any `std::chrono` duration, or
  `tickertape::wait_forever`. **A zero or negative duration does not wait** (unlike the C ABI, where a negative
  timeout waits forever): a deadline that has passed returns at once. Durations are rounded up to whole nanoseconds;
  integral counts are converted exactly (integer arithmetic only, so a positive duration never becomes 0 or comes
  out below its exact value) for every period that converts at all (a period so extreme that
  `std::ratio_divide<Period, std::nano>` overflows does not compile), and anything longer than `INT64_MAX`
  nanoseconds, about 292 years,
  waits that long.
  A timeout bounds only the wait for a record; catching up after a seek and file I/O can take longer.
- **Timestamps** are `std::uint64_t` nanoseconds that you choose; `tickertape::realtime_ns()` reads the wall clock.

## Ownership and lifetimes

| Object | Create | Default-constructed | Owns |
|---|---|---|---|
| `Writer` | `Writer::open(dir[, options])` | empty | a writer handle (and the journal's lock) |
| `Reader` | `Reader::open(dir[, options])`, `Reader::open_mapped_unsafe(dir[, options])` | empty | a reader handle |
| `WriterOptions`, `ReaderOptions`, `Retention` | their constructors | the defaults | an option handle |
| `RecordBatch` | `RecordBatch(max_records, buffer_bytes)` | (no default constructor) | C++ buffers for batch reads |

- All of them are **move-only** and free what they own in their destructor. A moved-from object, and a
  default-constructed `Writer` or `Reader`, is **empty**: you can destroy it or assign to it, and calls on it fail
  with `invalid_argument`. `is_open()` tells whether a writer or reader holds a handle. A moved-from `RecordBatch`
  has capacity 0.
- The destructor of a `Writer` closes it **without checkpointing**: records stay visible, but only checkpointed
  records are durable. `close()` does the same now and reports the status, including the failure of a background
  checkpoint (see [Durability](#durability)).
- Options and retention settings are copied by the functions that take them. A moved-from options object passed to
  `open` fails with `invalid_argument` rather than meaning "the defaults".
- **`RecordView` borrows.** `Reader::next` returns a view whose payload points into the reader's buffer (safe reader)
  or into the memory-mapped segment (mapped reader). **It is valid until the next call on that reader other than
  `position()` and `metrics()`, or until the reader is moved or destroyed.** The view's `seq()`, `schema_id()`,
  `venue_ts_ns()`, `local_ts_ns()`, `size()` and `empty()` are copies and stay valid; `data()`, `as_string_view()` and `as_span()`
  read the payload. Copy what you need to keep: `std::string(view.as_string_view())`.
- **Debug check:** in builds without `NDEBUG` (or with `TICKERTAPE_DEBUG_CHECKS` defined to 1), fetching the payload
  through a view (`data()`, `as_string_view()`, `as_span()`) after its reader moved on aborts with a message, also
  for an empty payload. The check runs at that call: a pointer, `string_view` or `span` you took from the view earlier
  is a plain pointer with no continuing check. A view whose reader was destroyed is not detected. Code built with and
  without the check can be linked together ([Mixing modes](#mixing-modes)).
- **Copy-out instead:** `Reader::next_copy(buf, size)` (any memory, as `void *`) or `next_copy(buffer)` (a writable
  `std::array`, `std::vector`, C array, ... of bytes or chars) copies one payload into your buffer, and
  `Reader::read_batch(batch)` copies many into a `RecordBatch`. Views from a batch are valid until the next read into
  that batch, or until it is moved or destroyed; views from `next_copy` as long as your buffer.
- `data()` is null when the payload is empty. `as_string_view()` and `as_span()` handle that; guard it before
  passing the pointer to `memcpy` or `printf("%.*s")`.
- `NewRecord` (for `append_batch`) does not copy its payload: keep the bytes alive until the append returns.
  `new_record()` refuses temporaries that own their bytes at compile time (see [Writing](#writing)).
- **No hidden allocation on the hot path.** `append`, `append_with`, `append_batch`, `next_seq`, `next`, `next_copy`,
  `read_batch` and `position` allocate nothing in the binding when they succeed (`tests/cpp/test_no_alloc.cpp`
  checks it by counting `operator new`). A failure allocates, to copy the error message. `RecordBatch` allocates its
  storage once, in its constructor; `list_segments` returns a `std::vector`.

## Writing

```cpp
tt::WriterOptions options;
options.segment_size(64 * 1024 * 1024).prefault(false);
tt::Writer writer = tt::Writer::open("/data/journal", options).value();
```

- `append(schema_id, venue_ts_ns, local_ts_ns, payload)` copies the payload into the journal and returns the
  record's sequence number.
- `append_with(schema_id, venue_ts_ns, local_ts_ns, size, fill)` avoids that copy: it calls
  `fill(std::uint8_t *buf, std::size_t len)` once with `size` zeroed bytes inside the mapped segment (`buf` is null
  when `size` is 0), on your thread, before it returns. `fill` returns `void` to commit, or `bool` (`false` aborts,
  for code without exceptions). It must not keep `buf`.
  - Aborting, by returning `false` or throwing, commits nothing, but the writer zeroes the bytes and syncs that to
    storage first, so it is slow: keep it for exceptional cases. Returning `false` gives `Status::aborted`; an
    exception is rethrown unchanged after the call.
  - If that sync fails, the writer stops accepting appends and checkpoints (a storage incident: see
    [`c-api.md` → A failed clean-up](c-api.md#a-failed-clean-up-after-an-aborted-fill)). The call then returns
    `Status::io` (with the OS error) instead; if `fill` threw, it throws `tickertape::Exception` with that error, and
    your exception nested inside (`std::rethrow_if_nested`), so you learn both.
  - Calling the same writer from inside `fill` (including `close()`) fails with `Status::busy`. Destroying the
    writer, moving it or assigning to it from inside `fill` is allowed: the append still completes (or aborts), and
    a writer destroyed or replaced that way is closed when `append_with` returns.
- `append_batch(records)` appends a `std::vector`, `std::array` or other range of `NewRecord` (or a pointer and a
  count) in order and returns a `BatchAppend`: `first_seq`, `appended` (how many were committed) and `outcome` (the
  error that stopped it, if any). Every record is checked first, so an argument error appends nothing; after that the
  records before the failing one stay appended: retry from `records[appended]`, not from the start.

  ```cpp
  std::vector<tt::NewRecord> batch;
  for (const std::string &message : messages) batch.push_back(tt::new_record(7, venue_ts, tt::realtime_ns(), message));
  tt::BatchAppend appended = writer.append_batch(batch);
  appended.outcome.value();   // throws if it stopped early; appended.appended says how far it got
  ```

  `tt::NewRecord` is the C struct `tickertape_new_record` itself (its fields `payload`, `payload_len`, `venue_ts_ns`,
  `local_ts_ns` and `schema_id` are public), so a vector or array of records goes to the library as it is, without a
  copy or an allocation. Build records with `tt::new_record(schema_id, venue_ts_ns, local_ts_ns, bytes)` or
  `tt::new_record(schema_id, venue_ts_ns, local_ts_ns, data, size)`. A record keeps a pointer to its payload, not a
  copy, so **the payload must be a named object** (an lvalue) that outlives the append, or a view: an rvalue is
  accepted only if `tt::enable_borrowed_bytes<T>` is true for its type, which it is for `std::basic_string_view` and
  `std::span`. For a view type of your own, specialise it (at any language level) at namespace scope in the header
  that defines the type, so that every translation unit sees it before anything that asks the trait about the type
  (a program in which one unit passes an rvalue of the type to `new_record` with the specialisation in sight and
  another does so without it is ill-formed, and a compiler need not say so):

  ```cpp
  template <> inline constexpr bool tickertape::enable_borrowed_bytes<MyView> = true;
  ```

  `std::ranges::enable_borrowed_range` is not consulted, so other C++20 views (`std::ranges::subrange`, ...) must be
  named or opted in the same way. `tt::new_record(7, 0, 0, std::to_string(i) + "...")` does not compile (the string
  would be destroyed at the end of the statement and the record would point into freed memory), and neither does a
  temporary `std::string`, `std::vector` or `std::array`. Keep the strings in a container that lives until the
  append returns.

## Reading

**The read loop.** `next()` returns a `result<std::optional<RecordView>>`, which has two different "bools": the
result's (did the call fail?) and the optional's (did a record arrive?). Test them in that order:

```cpp
tt::result<std::optional<tt::RecordView>> r = reader.next(100ms);
if (!r) { /* the call failed: r.error() */ }
else if (!*r) { /* no record within the timeout */ }
else { const tt::RecordView &record = **r; /* a record */ }
```

> **Warning:** `if (auto r = reader.next())` is true when the call *succeeded*, including when no record arrived,
> and `(*r)->data()` on an empty optional is undefined behaviour. With exceptions, `if (auto record =
> reader.next().value())` tests the optional and is the shortest correct form.

A complete loop:

```cpp
tt::Reader reader = tt::Reader::open("/data/journal").value();   // at the oldest record
reader.seek(1000).value();                                       // the next record is seq 1000
while (!stop_requested()) {
    tt::result<std::optional<tt::RecordView>> next = reader.next(100ms);
    if (!next) {
        if (next.error().status() == tt::Status::discarded) {    // recovery or retention replaced the segment
            if (reader.seek(reader.position().value())) continue;
        }
        throw tt::Exception(next.error());                      // or handle it without exceptions
    }
    if (*next) handle(**next);                                  // the payload is valid until the next call
}
```

`seek_to_end()` (from 1.1) places the reader at the end of the journal instead, reading at most the newest segment
(`tickertape_reader_seek_to_end` in [`c-api.md`](c-api.md#reading)). `ReaderOptions::seek_index(true)` (from 1.1) lets
`seek` start at a record named by a seek index (`tickertape_reader_options_set_seek_index` there has what that gives up).

There are two readers, as in C:

- `Reader::open`, the **safe reader**, reads the files with `pread` into a buffer it owns and validates the copy. It
  is memory-safe whatever other processes do to the files.
- `Reader::open_mapped_unsafe`, the **mapped reader**, returns payloads that point straight into the memory-mapped
  segment, without a copy. **Precondition: while it exists, no process other than a tickertape writer may modify the
  journal's segment files.** A tickertape writer never changes committed bytes, so this holds in normal operation. If
  something else writes into or truncates a segment, payloads can change under you, and truncation kills the process
  with `SIGBUS`. The `_unsafe` suffix is there so that it is never chosen by accident.

**Starting before the writer.** Opening a journal whose writer has not started fails with `Status::io` and
`os_code() == ENOENT`; while the writer creates the first segment it fails with `Status::not_found`. A loop that waits
for the writer retries on exactly these two (`examples/c/example.cpp` does).

**Cancelling a wait.** `next(tickertape::wait_forever)` cannot be interrupted, and destroying the reader from another
thread while it waits is undefined behaviour. Loop with a finite timeout and check for a stop request between calls.

**Batches.** `read_batch(batch, timeout)` waits up to the timeout for the first record only, then takes what is
already committed, up to `batch.capacity()` records or until the next payload does not fit in the batch's buffer (it
is then returned first by the next read). If even the first does not fit, it fails with `Status::buffer_too_small`
and `error().needed_size()`. If an error happens after some records were read, the call returns those records, and
the next read returns the error: records handed out are never lost. A successful `seek` or `seek_to_end`
discards a held-back record and a deferred error.

```cpp
tt::RecordBatch batch(256, 1 << 20);   // allocated once
while (std::size_t n = reader.read_batch(batch, 10ms).value()) {
    for (const tt::RecordView &record : batch) handle(record);
}
```

## Durability

| Level | Reached when | Means |
|---|---|---|
| **Visible** | an append returns a value | Readers on this host can read the record, and it survives the writer process dying. It can still be lost in a power loss or OS crash |
| **Durable** | a later `Writer::checkpoint()` succeeds, or a later `Writer::request_checkpoint()` has finished | The record is on stable storage, under the crash contract's storage assumptions |

**Background checkpoints** (ADR 0038; [`c-api.md` → Background checkpoints](c-api.md#background-checkpoints) has the
rules): `Writer::request_checkpoint()` starts a checkpoint on a thread of the library and returns at once with the
sequence number it covers up to, so appends go on while it syncs; `Writer::checkpoint_progress()` returns a
`CheckpointProgress` (`durable`: the last durable `Checkpoint`, if any; `pending`: the frontier still in flight, if
any). A failure is returned once, by the next `request_checkpoint`, `checkpoint_progress`, `checkpoint` or `close`,
then `Status::durability_lost`. `close()` waits for a checkpoint in flight and returns that failure (or
`durability_lost` for a poisoned writer); the destructor waits too but cannot report, so call `close()` when you need
to know. `WriterOptions::early_writeback(bool)` and `WriterOptions::helper_cpus(HelperCpus)` (or a list of CPUs) set
the Linux-only options of ADR 0038.

Closing or destroying a writer does not checkpoint. A failed checkpoint poisons the writer: later checkpoints fail
with `Status::durability_lost` while appends keep working; treat it as an incident. `ReaderOptions::durable_only(true)`
gives a reader that returns only durable records (crash contract C13): it treats later records as not written yet
and returns nothing without a valid checkpoint marker. See [`c-api.md` → Durability](c-api.md#durability) and
[`crash-contract.md`](crash-contract.md) for the conditions.

## Errors

`tickertape::Error` describes a failed call:

| Member | Returns |
|---|---|
| `status()` | A `tickertape::Status` (`io`, `locked`, `corrupt`, ...) |
| `message()` | The library's description, with the file and operation involved (UTF-8) |
| `os_code()` | `errno` for `Status::io` and `Status::storage_full`, else 0 |
| `corrupt_reason()` | A `tickertape::CorruptReason` for `Status::corrupt`, else `CorruptReason::none` |
| `needed_size()` | For `Status::buffer_too_small` from `next_copy` and `read_batch`: the bytes the record needs |
| `name()`, `to_string()` | `"TICKERTAPE_E_IO"`, and `"TICKERTAPE_E_IO: <message>"` |

`tickertape::Exception` (a `std::runtime_error`) holds an `Error`, shared between copies so that copying an exception
never throws; `what()` is `to_string()`. Moving an exception copies it, so a moved-from exception still has its
`Error`. `message()` on a temporary `Error` returns the string by value.

`Status` and `CorruptReason` have one enumerator per C code: see the table in
[`c-api.md` → Errors](c-api.md#errors) (`TICKERTAPE_E_SEQ_UNAVAILABLE` is `Status::seq_unavailable`, and so on). A
newer library may return a code this header does not name; it keeps its numeric value, and any negative value is an
error. The binding adds three cases of its own: `invalid_argument` for a path with a NUL byte and for moved-from
options, and `unsupported_version` when the loaded library does not match the header.

The option builders record the first error a setter reports (for example `backoff` with a minimum above the
maximum), so you can chain setters; the open function that takes them returns it, and `options.error()` shows it.
Each builder checks the loaded library before it creates its C handle: with a library that does not match the header
it records `unsupported_version` instead, and calls no C function other than the version queries.
Their `native_handle()` gives the C handle for a C setter the header does not wrap yet; errors from such calls are
not recorded.

## Threading

- A `Writer` or `Reader` may be moved to another thread between calls, but must not be used by two threads at the
  same time, **including through const member functions** (`Writer::next_seq()`, `last_retention_error()`,
  `Reader::position()` call into the same C handle). The same holds for options and batches. Different readers are
  independent.
- The fill function runs on the thread that called `append_with`. The writer counts as in use until `append_with`
  returns, even if the fill hands it to another thread: that thread must not call it before then.
- The binding reads the C ABI's per-thread error state inside the call that failed, so `Error` is always the right
  one, whatever your threading. The header adds no global state of its own.
- With the default options a writer starts background helper threads; `WriterOptions::prefault(false)` gives one
  that starts none.
- **`fork()`:** the C rules apply unchanged ([`c-api.md` → Threading](c-api.md#threading)): never use or destroy, in
  the child, objects created before the fork. Destroying a `Writer` closes its handle, so a child that does not
  `exec` must not run their destructors either (leave with `_exit`).

## Inspection and retention

- `tickertape::verify(dir)` checks every segment and returns a `VerifyReport` (records, first and last sequence
  numbers, number of problems and the first one). Corruption is reported there, not as an error.
- `tickertape::list_segments(dir)` returns the segments, oldest first.
- `tickertape::read_checkpoint(dir)` returns the checkpoint marker, or `std::nullopt` if there is no valid one.
- Retention (ADR 0016) removes a sealed, checkpoint-covered prefix of old segments. Build a `Retention` with limits
  (`max_segments`, `max_bytes`, `max_age`) and an action, which must be set explicitly: `delete_segments()` or
  `move_to(dir)`. **A negative or NaN `max_age` is an error** (`invalid_argument`, returned by whatever uses the
  settings), never zero, which would make every sealed segment old enough to remove; ages round up to whole
  nanoseconds. Pass it to `WriterOptions::retention` to run after every checkpoint (errors go to
  `Writer::last_retention_error()`), or to `tickertape::apply_retention(dir, settings)` to run once.
  `tickertape::plan_retention(dir, settings)` says what would be removed, without an action and without changing
  anything.

## Operational metrics

The values `tickertape status` prints (ADR [0032](decisions/0032-operational-metrics.md); their meaning and accuracy:
[`operations.md`](operations.md#monitoring)), over the C calls in [`c-api.md` → Operational
metrics](c-api.md#operational-metrics), which also lists what each costs:

```cpp
tickertape::JournalStatus status = tickertape::JournalStatus::sample(dir).value();  // no writer needed
const tickertape::StatusReport &r = status.report();
if (r.exposure.records) std::printf("not durable yet: %llu records\n", (unsigned long long)*r.exposure.records);
if (auto lag = reader.metrics(status).value().lag) std::printf("behind: %llu\n", (unsigned long long)lag->records);
tickertape::WriterMetrics w = writer.metrics().value();  // in the writer's process: no system calls
```

- `JournalStatus::sample(dir)` (or `sample(dir, readers_dir)`) reads the values once into a `StatusReport` and the
  reader beacons into `beacons()` (a `std::vector<BeaconStatus>`, sorted by name), and keeps the C sample for
  `Reader::metrics`. It is move-only and keeps no file open; its const member functions may be called from several
  threads at once.
- Unknown values are empty `std::optional`s, never guesses; ages are `std::chrono::nanoseconds`. Enumerations
  (`HeadEnd`, `CheckpointProblem`, `AgeUnknown`, `LagUnknown`, `BeaconState`, `ReadersDirState`) name every C value.
- Corruption is `report().head.end == HeadEnd::invalid`, a `checkpoint->problem` other than `usable`, or
  `head.chain_broken`. `head.verified == false` means the segments between the checkpoint and the newest were not
  read: no corruption reported then does not mean that a writer would open the journal.
- `Reader::metrics(status)` measures against the sample: take it after the reads you want counted. Its `lag` is empty,
  with the reason in `lag_unknown`, when the reader's segment is not the one the sample listed (removed by retention,
  another file, or newer than the sample). It does not invalidate a `RecordView`.
- `Writer::metrics()` makes no system call. Like every value type of the binding, these structs gain no members
  within a major version (ADR 0036 → Versioning); later values come as new types or functions.

The latency histograms and record ages of the Rust crate (`tickertape::latency`, from 1.1, ADR
[0047](decisions/0047-latency-histograms.md)) are not available from C++ yet; `tickertape age` (see [`cli.md`](cli.md))
reports ages from the command line.

## Versioning

The header defines `TICKERTAPE_HPP_VERSION_MAJOR`, `_MINOR`, `_PATCH`, `TICKERTAPE_HPP_VERSION_NUMBER` and
`TICKERTAPE_HPP_ABI_VERSION`: the release it ships in. It has the same version as the `tickertape.h` and library of
that release, and follows the C ABI's rule ([`c-api.md` → Versioning](c-api.md#versioning)):

- **Before 1.0** (releases 0.x) it needed exactly its own release's `tickertape.h` and library.
- **From 1.0** it needs the same ABI version and a `tickertape.h` and library at least as new as itself, so a newer
  library under an older header keeps working. Within a major version no public declaration is removed or changes
  meaning; new functions, member functions, types and enumerators may be added, and **no class gains a data member**,
  so rebuilding against a newer minor release never changes the size of your own types that contain the binding's.
- **One release of the header per program.** Every translation unit of a program that includes `tickertape.hpp` must
  use the same release of it. Mixing releases breaks the one-definition rule, as with any header-only library: the
  linker keeps one copy of each inline function, and those copies differ between releases (version checks, the sizes
  of C structs), so a part built against 1.1 could end up running 1.0's code. Nothing detects this at build time.
  Rebuild every part when you upgrade.
- The inline namespace carries the major version (`v1`; a major version that changes a layout moves to `v2`) and the
  exception mode ([Mixing modes](#mixing-modes)). You write `tickertape::Writer`; do not forward-declare the header's
  classes or name the inline namespace.

It checks `tickertape.h` at compile time (`#error`), and the loaded library at run time in every function that takes
a journal path and in the option builders' constructors (`Status::unsupported_version`).
`tickertape::check_library()` and `tickertape::library_compatible()` run the same check, and `library_version()`
returns the library's version string. The header's own helper macros are undefined at its end.

## Testing

`cargo test -p tickertape-ffi` compiles and runs the binding's test programs (`crates/tickertape-ffi/tests/cpp/`)
as C++17, C++20 and C++17 without exceptions, plus a test against stubbed C functions (a failed clean-up after a
throwing fill, every status and corruption reason), the allocation check, a debug-check death test, the
cross-language fixtures with Rust, Python and C, the example, and a compile-only matrix of C++17, C++20 and C++23
with and without exceptions and `NDEBUG` for every C++ compiler it finds (`TICKERTAPE_FFI_CXX_COMPILERS` sets the
list), which also pins every class's size and alignment (`tests/cpp/layout.cpp`). It checks that the header's
definitions are the same at every language level, links units built in different modes into one program with every
compiler (failing each allocation inside a binding function in turn, and checking with `nm` that the two exception
modes define no common binding symbol), checks that binding types cannot be passed between exception modes, that
`new_record` refuses owning temporaries and other unsafe code at compile time, and fails if a C function, struct
field, status code or corruption reason is added without a decision for the binding, or if a debug-check-dependent
function lacks its mode tag (ADR 0036).
`TICKERTAPE_FFI_SANITIZE=1` and `TICKERTAPE_FFI_VALGRIND=1` work as for the C tests; under Valgrind the tests that
replace `operator new` run with `--soname-synonyms=somalloc=nouserintercepts`. With the GCC driver on macOS, the
statically linked test programs run too ([Building and linking](#building-and-linking)).
