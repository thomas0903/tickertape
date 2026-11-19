# 0036: Header-only C++ binding over the C ABI

- **Status:** accepted (the surface becomes stable at v1.0)
- **Date:** 2026-11-19

## Context

ADR [0031](0031-bindings-and-install-channels.md) makes a header-only C++ wrapper over the C ABI (ADR
[0028](0028-c-abi.md)) a v1.0 binding: move-only handles, views and RAII, tested in the same job as the C ABI. A
binding counts as delivered when it has the full record and replay surface with errors mapped to the language's own,
documented ownership and lifetimes, the shared cross-language fixture tests, a runnable example, an install test, and
a version rule tied to the C ABI's.

Until now the repository had only `examples/c/tickertape.hpp`, a small C++11 wrapper meant to be copied. It covered
part of the surface, returned the C structs, needed exceptions, and leaked the writer handle if a fill callback
destroyed its own writer (the C ABI refuses that close with `TICKERTAPE_E_BUSY`). Its public surface was never meant
to be kept.

The audience is mostly low-latency C++ code: many such code bases build with `-fno-exceptions`, care about
allocations on the hot path, and mix compiler versions and language levels in one program.

## Options considered

**Language level.**

1. **C++11 or C++14.** The widest reach, but without `std::string_view`, `std::optional`, `std::filesystem::path`,
   `if constexpr` or class-level `[[nodiscard]]`, the header would need its own versions of each, and they would then
   be part of the stable surface.
2. **C++17.** Has all of the above. GCC 9, Clang 9 and Apple Clang with a macOS 10.15 target have the library parts
   used here (`<filesystem>` included).
3. **C++20.** `std::span` and concepts, but it would shut out code bases still on C++17 toolchains, which are common
   in the trading systems this is aimed at.

**Errors.**

1. **Exceptions only**, as in the example wrapper. Unusable with `-fno-exceptions`.
2. **Two surfaces**: throwing functions plus non-throwing overloads (as `std::filesystem` does with
   `std::error_code&`). Twice the surface to keep stable and test, and `std::error_code` loses the library's message.
3. **One surface returning a result type** whose `value()` throws. Works the same with and without exceptions;
   exception users write `.value()`.
4. **`std::expected`** (C++23 only) or **`std::error_code`** with a `std::error_category`: the first is not
   available at C++17; the second needs a process-wide category object (global state) and keeps only a number.

**Where the header lives.** In `examples/c/` (as now; but an example is not a shipped file), in a new
`bindings/cpp/` directory (a second place to version and install), or next to `tickertape.h` in
`crates/tickertape-ffi/include/`.

**The mapped reader.** A separate `MappedReader` type, or a factory function on `Reader` whose name carries the
precondition.

## Decision

**Location and packaging.** One header, `crates/tickertape-ffi/include/tickertape.hpp`, next to `tickertape.h`
and installed with it by `cargo cinstall` (`[package.metadata.capi.install.include]`). It is hand-written (cbindgen
only generates the C header) and depends on nothing but the standard library and `tickertape.h`. `examples/c/`
uses the shipped header; the example copy is removed. A minimal CMake package, `crates/tickertape-ffi/cmake/
tickertapeConfig.cmake`, defines `tickertape::tickertape` (the shared library and include directory) and
`tickertape::cpp` (plus C++17) for `find_package(tickertape CONFIG PATHS ...)`, and
`crates/tickertape-ffi/CMakeLists.txt` defines the same targets globally for `add_subdirectory`. It finds the
library in the repository's `target/release` or the usual system places; installing the CMake files with the
library comes with the C library archive (ADR 0031).

**Language level: C++17**, for the reasons in option 2. The header refuses older standards with `#error`, and is
tested as C++17, C++20 and C++23. Every class has the same definition at every language level and in both
exception modes, so translation units built differently can be linked into one program without violating the
one-definition rule: the C++20-only extra (`tickertape::as_span(const RecordView&)`, a `std::span`) is a free
function, not a member, and byte payloads are taken by a template that accepts any contiguous range of
`char`/`unsigned char`/`signed char`/`std::byte` (and `char8_t` in C++20), so `std::span` works without a C++20
overload. The debug checks (below) change no layout either. Only function bodies differ between exception modes
(`value()` throws or aborts), as with `assert`; the docs ask for one exception mode per program.

**Errors: one surface returning `tickertape::result<T>`** (option 3). `result<T>` holds a `T` or a
`tickertape::Error`; it is `[[nodiscard]]`, so a discarded result is a compiler warning (an ignored error is never
silent). `value()` returns the value or throws `tickertape::Exception` (a `std::runtime_error` holding the `Error`);
with exceptions disabled it prints the error and calls `std::abort`. `operator*` and `->` are unchecked, except in
debug builds. On an rvalue result, `value()`, `*` and `error()` return by value (one move) rather than a
reference, unlike `std::optional` and `std::expected`: GCC's `-Wdangling-reference` caught
`for (auto &s : list_segments(dir).value())` referring into a destroyed temporary in our own compile test, and the
move costs nothing next to a call into the library. The header defines its own type rather than using
`std::expected`, so that its types are the same at every language level. The library never throws, except from `value()` and from `Writer::append_with`, which rethrows
what the user's fill threw (below).

- `Error` carries a `Status`, the library's message, the OS error number (`os_code()`, for `io` and `storage_full`)
  and the corruption reason (`corrupt_reason()`, for `corrupt`), all read from the C ABI's per-thread last-error
  state right after the failing call, and `needed_size()` for `buffer_too_small` from the copy-out reads. The message
  is copied, so an error allocates; a success does not.
- `enum class Status` names every `tickertape_status` (including `ok` and `none`, which never appear in an `Error`),
  and `enum class CorruptReason` every reason. A code from a newer library that the header does not name is kept as
  its numeric value (a fixed-underlying-type enum can hold it), never squashed into `unknown`. One exception type
  rather than a class per status: statuses can be added in minor releases, and `e.status()` distinguishes them.
- The option builders (`WriterOptions`, `ReaderOptions`, `Retention`) record the first error a setter reports, so
  setters can be chained; the open (or retention) function that takes them returns it.
- Errors the binding itself detects use the same type: a path with an embedded NUL byte (the C ABI would silently
  truncate it) and moved-from options (which the C ABI would take as "defaults") are `invalid_argument`; a library
  that does not match the header is `unsupported_version`.

**Surface.** Everything in the C ABI, in C++ terms:

| C++ | C |
|---|---|
| `Writer::open(dir[, options])`, `append` (pointer and size, or any byte range), `append_with`, `append_batch` (array or range of `NewRecord`, returns `BatchAppend`), `checkpoint`, `next_seq`, `last_retention_error`, `close`, destructor | `tickertape_writer_*` |
| `Reader::open`, `Reader::open_mapped_unsafe`, `seek`, `next(Timeout)`, `next_copy`, `read_batch(RecordBatch&)`, `position`, `close`, destructor | `tickertape_reader_*` |
| `WriterOptions`, `ReaderOptions` (including `durable_only`), `Retention` | the option builders |
| `verify`, `list_segments`, `read_checkpoint`, `apply_retention`, `plan_retention` | inspection and `tickertape_retention_apply` (`dry_run`) |
| `library_abi_version`, `library_version_number`, `library_version`, `library_compatible`, `check_library`, `realtime_ns`, `status_name`, `corrupt_reason_name` | version and helper functions |
| `Error` | `tickertape_last_error_message`, `_os_code`, `_corrupt_reason` |

Paths are a `PathArg`: a `const char *`, `std::string`, `std::string_view` (copied for the NUL terminator; opening is
not a hot path) or `std::filesystem::path`, without overload ambiguity. The inspection results are C++ structs with
`std::optional` where the C struct has "valid only if" fields; `list_segments` returns a `std::vector` (it allocates,
and retries if the list grows between its two calls). `apply_retention` and `plan_retention` replace the `dry_run`
flag with two names.

**Functions not wrapped**, deliberately:

- `tickertape_last_error_code`: every call returns its status, and `Error` takes the code from that return value.

Every other C function is called by the header. A test in `tests/c_api.rs` parses `tickertape.h` and fails if a
function is in neither list, if a "wrapped" function is never called by the header, or if a "not wrapped" one is
called or missing from this ADR. Another test parses both headers and fails if a status code, corruption reason or
recovery policy has no C++ enumerator; the C++ test program checks the reverse at run time (every code the library
names is a named `Status`), and `-Wswitch` makes a new enumerator break the test's exhaustive switch.

**Ownership and lifetimes.**

- `Writer`, `Reader`, the three builders and `RecordBatch` are move-only owners of one C handle (or buffer) each. A
  moved-from or default-constructed object is empty: destroying it or assigning to it is safe, and calls on it fail
  with `invalid_argument` (the C ABI's answer to a NULL handle). `close()` reports the close's status; the
  destructor ignores it, as it must. `native_handle()` gives the C handle for code that calls C directly.
- **`RecordView`** is what `Reader::next` returns. Its payload borrows the reader's memory and is valid until the next
  call on that reader other than `position()`, or until the reader is moved or destroyed (the C rule, plus moves,
  which the binding treats as invalidating). Views from a `RecordBatch` are valid until the next read into that batch;
  views from `next_copy` point into the caller's buffer. The sequence number, schema id and timestamps are copies and
  stay valid. **Debug check:** each reader and batch keeps a counter that every invalidating call and every move
  bumps; a view remembers the counter's address and value, and reading its payload after the counter moved calls
  `std::abort` with a message. This costs one increment per call and 16 bytes per view, in every build, so that the
  layout does not depend on the setting; the comparison runs when `TICKERTAPE_DEBUG_CHECKS` is 1 (the default
  unless `NDEBUG`). A view whose reader was destroyed is not detected (the counter is gone).
- `NewRecord` (for `append_batch`) does not copy its payload. It is standard-layout with the size and alignment of
  `tickertape_new_record` (asserted), so an array of them is passed to the C ABI without copying.
- **No hidden allocation on the append and read path**: `append`, `append_with`, `append_batch`, `next_seq`, `next`,
  `next_copy`, `read_batch` and `position` allocate nothing in the binding when they succeed. `RecordBatch` allocates
  its storage once, in its constructor. `tests/cpp/test_no_alloc.cpp` checks this by replacing `operator new` and
  counting during a loop of every one of those calls; a failure copies the error message and does allocate.

**The fill callback.** `append_with(schema, venue_ts, local_ts, size, fill)` takes any callable
`fill(std::uint8_t *buf, std::size_t len)` returning `void` (commit) or `bool` (`false` aborts; for code without
exceptions). The C callback is a `noexcept` trampoline that catches every exception, stores it and returns non-zero,
so no exception ever reaches the C ABI. After the call:

- a clean abort after a throw rethrows the user's exception unchanged; after `false` it returns `aborted`;
- if the clean-up after the abort failed (`Writer::abandoned_write_error` in the core; the C call returns
  `TICKERTAPE_E_IO` instead of `TICKERTAPE_E_ABORTED`), a throw becomes `std::throw_with_nested(Exception(io
  error))`, so the caller gets both the library error (the writer has stopped) and its own exception
  (`std::rethrow_if_nested`); after `false` it returns the `io` error.

**Calling into the writer from its own fill** returns `busy` (the C ABI detects re-entry). **Destroying,
moving or move-assigning the writer from its own fill** is defined: while the fill runs, the writer points to a frame
on `append_with`'s stack. The destructor and move-assignment, seeing the frame, hand the old handle to it instead of
closing it (the C ABI would refuse the close with `busy`, and the handle would leak); a move carries the frame to
the new owner. After the C call returns, `append_with` touches only the frame: it closes the handle if its owner was
destroyed or reassigned, or clears the current owner's pointer otherwise. So the append completes (or aborts), and the
handle is closed when `append_with` returns. An explicit `close()` from inside the fill returns `busy` and closes
nothing. Each case is tested with the real library (lock released, record present, no leak under AddressSanitizer
and Valgrind).

**Timeouts** are a `Timeout`: `no_wait` (the default), any `std::chrono::duration`, or `wait_forever`. Durations are
rounded up to whole nanoseconds and saturate at `INT64_MAX` nanoseconds. **A zero or negative duration does not
wait.** This deliberately differs from the C ABI, where a negative timeout waits forever: in C++ a negative duration
is usually a deadline that has passed, and waiting forever for it would be a bug that is hard to see. Waiting forever
needs the explicit `wait_forever`.

**Threads and fork** are the C ABI's rules: an object may move between threads but must not be used by two at once;
separate readers are independent; the per-thread last-error state is read inside the call that failed, so the
binding is safe with any threading. The fork rules of `docs/c-api.md` apply unchanged. The header adds no global
state: no static or thread-local variables, only constants (`no_wait`, `wait_forever`).

**Versioning.** The header defines `TICKERTAPE_HPP_VERSION_MAJOR`, `_MINOR`, `_PATCH`, `TICKERTAPE_HPP_VERSION_NUMBER`
and `TICKERTAPE_HPP_ABI_VERSION`, the version of the release it ships in. **Rule:** the C++ header has the same
version as the C header and library it ships with, and is compatible with them by the C ABI's rule (ADR 0028): before
1.0 (ABI version 0) exactly that version; from 1.0, the same ABI version and a C header and library at least as new.
It checks this at compile time against `tickertape.h` (`#error`) and at run time, in every open function, through
`tickertape_abi_version()` and `tickertape_version_number()` (`unsupported_version` with both versions in the
message); `check_library()` runs the same check. The C++ surface follows the release's semantic version: from 1.0,
no public declaration is removed or changes meaning within a major version, and additions are new functions, new
members, new enumerators and new optional parameters. A test checks that the header's version macros match the
crate version, so a release cannot ship a stale header; the release checklist says to bump them.

## Consequences

- C++ users get one supported header, installed with the library, usable with and without exceptions.
- Tests, all in the `c abi` CI job (`cargo test -p tickertape-ffi`, Linux x86-64 and macOS, AddressSanitizer and
  UBSan, then Valgrind on Linux): `tests/cpp/test_binding.cpp` (the surface, errors, fill-callback cases, move
  semantics; built as C++17, C++20 and C++17 without exceptions), `test_binding_stub.cpp` (a failed clean-up after a
  throwing fill, every status and reason, a mismatched library; stubbed C functions, since the real library cannot be
  made to fail a clean-up on demand from C), `test_no_alloc.cpp`, a death test for a stale view, the cross-language
  fixtures (C++ writes, Rust, Python and C read; Rust and C write, C++ reads), the C++ example (record, follow live,
  replay), a compile-only matrix (`compile_all.cpp` as C++17, C++20 and C++23, with and without exceptions, with every
  C++ compiler found: GCC and Clang on Linux, Apple Clang on macOS, under `-Wall -Wextra -Wpedantic -Wconversion
  -Wshadow -Werror`), and the coverage, enumerator and version checks above. CI also builds the examples with make,
  with CMake through `find_package`, and through `add_subdirectory`.
- The C header's C++11 promise (ADR 0028) is now tested by a small program of its own, since the example wrapper
  that used to cover it is gone.
- Not covered yet: the install test from a built package on every platform that ADR 0031 requires; it needs the C
  library archive, which also brings installed CMake files. The binding is otherwise delivered.
- New docs: `docs/cpp-api.md`; `docs/c-api.md` and `examples/c/` point to the binding.
