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
tested as C++17, C++20 and C++23. Every class has the same definition at every language level: the C++20-only extra
(`tickertape::as_span(const RecordView&)`, a `std::span`) is a free function, not a member, and byte payloads are
taken by a template that accepts any contiguous range of `char`/`unsigned char`/`signed char`/`std::byte` (and
`char8_t` in C++20), so `std::span` works without a C++20 overload. Exception and debug-check modes are below
(**Mixing modes**).

**Mixing modes.** One program may contain translation units built with and without exceptions, and with and without
debug checks: a low-latency component built with `-fno-exceptions` inside an application built with exceptions is
exactly this binding's audience, so mixing is made safe rather than forbidden. The first version claimed this
without doing it: the layouts matched, but inline functions whose bodies depend on the mode (`value()` throws or
aborts) exist once per program, chosen by link order, so an application's `value()` aborted instead of throwing when
a no-exceptions component came first on the link line (reproduced with Apple Clang at -O0 and -O2 and GCC 15 at
-O0). What is guaranteed now, with GCC and Clang (the Itanium C++ ABI: Linux, macOS):

- **The types are shared.** Every class has the same layout in every mode (the debug check's fields exist in every
  build; `[[maybe_unused]]` keeps Clang quiet when `NDEBUG` leaves them unread), so objects, results and
  `tickertape::Exception` pass between units built in different modes.
- **Each unit keeps its own mode's behaviour.** Every inline function whose behaviour depends on the exception mode
  or the debug-check mode is declared `TICKERTAPE_HPP_MODE_DEPENDENT`, which expands to
  `__attribute__((abi_tag(E, D)))` with E `tickertape_exc` or `tickertape_noexc` and D `tickertape_dbg` or
  `tickertape_nodbg`. The tags are part of the mangled name, so each mode's copy is a different symbol and the
  linker cannot substitute one for another. This covers `result<T>::value()`, `operator*`, `operator->` and
  `error()`, `result<void>::value()` and `error()`, `RecordView::data()` and `as_string_view()`, `as_span`,
  `RecordBatch::operator[]` and its iterator's `operator*`, `Writer::append_with` and its trampoline, and the
  internal `detail::fail` and `detail::debug_check`. The header's other functions do not call them (they read a
  failed `result<void>` through an unchecked internal accessor), so their single copy behaves the same in every
  mode. One macro carries both tags, so a function cannot get one and miss the other.
- **Not covered:** behaviour follows the code that acts on a value, not the code that made it (`value()` in a
  no-exceptions unit aborts, whichever unit produced the result); a fill passed to `append_with` from a
  no-exceptions unit must not throw (nothing there catches it); your own inline functions that call mode-dependent
  ones are yours to keep consistent; and compilers without `abi_tag` (MSVC) get no tag, so there one mode per
  program is required and mixing is undefined behaviour.
- **Enforced** by a source check in `tests/c_api.rs` (it fails if a function whose body mentions a mode macro, or
  calls a tagged function by name, lacks the tag; calls through operators are not seen, so the operators are tagged
  by hand), and by a link test that compiles units in each mode and links them into one program in both orders, at
  -O0 and -O2, asserting each unit's behaviour (no-exceptions with exceptions; debug checks with none). Without the
  tags it fails in every pair.

**Errors: one surface returning `tickertape::result<T>`** (option 3). `result<T>` holds a `T` or a
`tickertape::Error`; it is `[[nodiscard]]`, so a discarded result is a compiler warning (an ignored error is never
silent). `value()` returns the value or throws `tickertape::Exception` (a `std::runtime_error` holding the `Error`);
with exceptions disabled it prints the error and calls `std::abort`. `operator*` and `->` are unchecked, except in
debug builds. On an rvalue result, `value()`, `*` and `error()` return by value (one move) rather than a
reference, unlike `std::optional` and `std::expected`: GCC's `-Wdangling-reference` caught
`for (auto &s : list_segments(dir).value())` referring into a destroyed temporary in our own compile test, and the
move costs nothing next to a call into the library. The header defines its own type rather than using
`std::expected`, so that its types are the same at every language level. Apart from `std::bad_alloc` (an error copies
its message; `list_segments` returns a vector), the binding never throws, except from `value()` and from
`Writer::append_with`, which rethrows what the user's fill threw (below).

- `Error` carries a `Status`, the library's message, the OS error number (`os_code()`, for `io` and `storage_full`)
  and the corruption reason (`corrupt_reason()`, for `corrupt`), all read from the C ABI's per-thread last-error
  state right after the failing call, and `needed_size()` for `buffer_too_small` from the copy-out reads. The message
  is copied, so an error allocates; a success does not.
- `enum class Status` names every `tickertape_status` (including `ok` and `none`, which never appear in an `Error`),
  and `enum class CorruptReason` every reason. A code from a newer library that the header does not name is kept as
  its numeric value (a fixed-underlying-type enum can hold it), never squashed into `unknown`. One exception type
  rather than a class per status: statuses can be added in minor releases, and `e.status()` distinguishes them.
- The option builders (`WriterOptions`, `ReaderOptions`, `Retention`) record the first error a setter reports, so
  setters can be chained; the open (or retention) function that takes them returns it. `native_handle()` has a
  non-const overload, so a C setter the header does not wrap yet can be called without `const_cast` (its errors are
  not recorded).
- **Durations are never clamped towards deleting more.** `Retention::max_age` with a negative or NaN duration
  records `invalid_argument` instead of clamping to 0 ns, which would make every sealed segment old enough to
  remove; ages round up to whole nanoseconds, so a fraction never shortens one. `max_segments` and `max_bytes` are
  unsigned and taken as given. The reader back-off durations still clamp negative values to 0, which only makes a
  waiting reader poll sooner.
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
function is in neither list, if a "wrapped" function is never called by the header's code (comments are stripped
first, so a name in a comment does not count), or if a "not wrapped" one is called or missing from this ADR.
Another lists every field of every C struct with a decision (used, with where, or unused, with why) and fails if
a struct gains or loses a field, or if a "used" field never appears in the header's code. Another test parses both headers and fails if a status code, corruption reason or
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
  layout does not depend on the setting (see **Mixing modes**); the comparison runs when `TICKERTAPE_DEBUG_CHECKS` is 1 (the default
  unless `NDEBUG`). A view whose reader was destroyed is not detected (the counter is gone).
- **Records.** `NewRecord` (for `append_batch`) does not copy its payload. It is standard-layout with the size and
  alignment of `tickertape_new_record` (asserted), so an array of them is passed to the C ABI without copying.
  Because it keeps a pointer, **a byte range that is an rvalue is accepted only if its type is a specialisation of
  `std::basic_string_view` or `std::span`**; for any other range type the rvalue constructor is deleted, so
  `batch.emplace_back(1, 0, 0, std::to_string(i) + "...")` (which used to store a pointer into a string destroyed at
  the end of the statement, a heap use-after-free with no warning) does not compile. Lvalues of any byte range are
  accepted, as are pointer-and-size pairs. An allowlist of view types rather than a rule such as "trivially
  copyable": a trivially copyable type can own its bytes inline (`std::array`, a user's fixed buffer), and a rule
  that lets one of those through would bring the dangling pointer back. A user's own view type is passed by name
  (an lvalue) or as pointer and size. `Writer::append` copies the payload before it returns, so it takes
  temporaries.
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

**Threads and fork** are the C ABI's rules: an object may move between threads but must not be used by two at once,
including through const member functions (`Writer::next_seq`, `last_retention_error`, `Reader::position` call
into the C handle); a writer counts as in use until `append_with` returns, even if the fill hands it to another
thread; separate readers are independent; the per-thread last-error state is read inside the call that failed, so the
binding is safe with any threading. The fork rules of `docs/c-api.md` apply unchanged. The header adds no global
state: no static or thread-local variables, only constants (`no_wait`, `wait_forever`).

**Versioning.** The header defines `TICKERTAPE_HPP_VERSION_MAJOR`, `_MINOR`, `_PATCH`, `TICKERTAPE_HPP_VERSION_NUMBER`
and `TICKERTAPE_HPP_ABI_VERSION`, the version of the release it ships in. **Rule:** the C++ header has the same
version as the C header and library it ships with, and is compatible with them by the C ABI's rule (ADR 0028): before
1.0 (ABI version 0) exactly that version; from 1.0, the same ABI version and a C header and library at least as new.
It checks this at compile time against `tickertape.h` (`#error`) and at run time, in every function that takes a
journal path (the opens, `verify`, `list_segments`, `read_checkpoint`, `apply_retention`, `plan_retention`), through
`tickertape_abi_version()` and `tickertape_version_number()` (`unsupported_version` with both versions in the
message); `check_library()` runs the same check. The C++ surface follows the release's semantic version: from 1.0,
no public declaration is removed or changes meaning within a major version, and additions are new functions, new
member functions, new types, new enumerators and new optional parameters.

**No data member is added to an existing class within a major version** (`Error`, `Writer`, `RecordView`,
`VerifyReport` and every other class), and none is removed or reordered: two components built against different
minor versions of the header would otherwise have different definitions of one type in one program. A report that
needs more information gets a new type or a new function. The classes live in an inline namespace named after
their layout version, `tickertape::v1` (written as `tickertape::Writer`); a later major version that changes a
layout moves to `tickertape::v2`, so components built against both can be linked into one program. (Before 1.0 the
layout can still change within `v1`; the exact-version check forbids mixing those releases.)
`tests/cpp/layout.cpp` pins the size and alignment of every public class against a mirror of its members, in every
compiler, language level and mode of the compile matrix, and a test fails if a public class is missing from it. A test checks that the header's version macros match the
crate version, so a release cannot ship a stale header; the release checklist says to bump them.

## Consequences

- C++ users get one supported header, installed with the library, usable with and without exceptions, and in one
  program whose parts are built in different modes (GCC and Clang).
- Tests, all in the `c abi` CI job (`cargo test -p tickertape-ffi`, Linux x86-64 and macOS, AddressSanitizer and
  UBSan, then Valgrind on Linux): `tests/cpp/test_binding.cpp` (the surface, errors, fill-callback cases, move
  semantics; built as C++17, C++20 and C++17 without exceptions), `test_binding_stub.cpp` (a failed clean-up after a
  throwing fill, every status and reason, a mismatched library; stubbed C functions, since the real library cannot be
  made to fail a clean-up on demand from C), `test_no_alloc.cpp`, a death test for a stale view, the cross-language
  fixtures (C++ writes, Rust, Python and C read; Rust and C write, C++ reads), the C++ example (record, follow live,
  replay), a compile-only matrix (`compile_all.cpp` and `layout.cpp` as C++17, C++20 and C++23, with and without
  exceptions, with and without `NDEBUG`, with every C++ compiler found: GCC and Clang on Linux, Apple Clang on
  macOS, under `-Wall -Wextra -Wpedantic -Wconversion -Wshadow -Werror`), the mixed-mode link test and the mode-tag
  source check (**Mixing modes**), compile-fail and compile-pass programs for `NewRecord` (**Records**), and the
  coverage, struct-field, layout, enumerator and version checks above. CI also builds the examples with make,
  with CMake through `find_package`, and through `add_subdirectory`.
- The C header's C++11 promise (ADR 0028) is now tested by a small program of its own, since the example wrapper
  that used to cover it is gone.
- Not covered yet: the install test from a built package on every platform that ADR 0031 requires; it needs the C
  library archive, which also brings installed CMake files. The binding is otherwise delivered.
- New docs: `docs/cpp-api.md`; `docs/c-api.md` and `examples/c/` point to the binding.
