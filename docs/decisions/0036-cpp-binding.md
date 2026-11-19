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

**Nothing the header defines depends on the language level.** A program may link a C++17 unit and a C++20 unit
that both include the header, and each inline function, class, variable template and inline variable must then have
one definition, token for token. **Rule:** a block conditional on the language standard or on a library
feature-test macro may only add whole declarations (checked for the compilers and standard libraries the test
matrix runs, since a feature-test macro is a property of the library in use: `as_span`, the `char8_t` specialisation
of `detail::is_byte`, the
`std::span` specialisation of `enable_borrowed_bytes`), at namespace scope (a member added to a class changes that
class's definition); it never changes a definition that also exists at C++17.
Round 2 broke this: under C++20 `detail::is_byte_view_v` gained an `|| std::ranges::enable_borrowed_range<...>` term,
so the two units held two definitions of one variable template. It is replaced by the binding's own opt-in
(**Records**), the same at every level. `tests/c_api.rs` enforces the rule: it preprocesses the header as C++17,
C++20 and C++23, with and without exceptions, with every C++ compiler found, and fails if a line of the lower level
changes or disappears at the higher one, or if the lines only the higher level has are not whole declarations
(balanced brackets, ending in `;` or `}`, after a line that ends a declaration); run against round 2 it reports
exactly that term and nothing else. One condition in the header is a property of the target, not of the language
level: the exact duration conversion uses a 128-bit product where `__SIZEOF_INT128__` is defined and 64-bit long
multiplication otherwise (**Timeouts**); that is the same in every unit of a program built for one target with GCC
or Clang, and both bodies compute the same exact value.

**Mixing modes.** One program may contain translation units built with and without exceptions, and with and without
debug checks: a low-latency component built with `-fno-exceptions` inside an application built with exceptions is
exactly this binding's audience, so mixing is made safe rather than forbidden. The first version claimed this
without doing it: the layouts matched, but inline functions whose bodies depend on the mode (`value()` throws or
aborts) exist once per program, chosen by link order, so an application's `value()` aborted instead of throwing when
a no-exceptions component came first on the link line. The second version tagged the functions whose bodies mention a
mode macro (`abi_tag`). That was not enough for the exception mode: any function can contain exception handling the
compiler generates (`list_segments` calls `reserve()` while another vector is live, so it has a clean-up path), and
when the linker kept the copy built with `-fno-exceptions`, a `std::bad_alloc` passing through it terminated the
process (Clang, which marks such code as unable to unwind) or skipped its clean-up (GCC). What is guaranteed now, with
GCC and Clang (the Itanium C++ ABI: Linux, macOS):

- **Every definition the header provides has a mode-specific symbol.** Everything the header declares lives in an
  inline namespace whose name carries the mode: `tickertape::v1` when exceptions are enabled (`__cpp_exceptions`),
  `tickertape::v1_noexcept` when they are not. Every function, type and inline variable of the binding therefore has
  a symbol per mode, whatever the compiler generates inside it. Code in one translation unit still writes
  `tickertape::Writer`.
- **No standard-library instantiation over only the binding's C types is shared.** The namespace does not reach a
  standard-library template whose arguments are only C types: round 2's `list_segments` used
  `std::vector<tickertape_segment_info>`, whose `resize` is one weak symbol for both modes; when the
  `-fno-exceptions` object came first on the link line the linker kept its copy (compiled without unwind
  information), and a `std::bad_alloc` from `operator new` inside it ended the process (Apple Clang 21, `-O0` and
  `-O2`; Homebrew GCC 15 was not affected). The C structs the header owns (and the byte buffer of `RecordBatch`) are
  now held in `detail::Array<T>`, a move-only, pointer-sized owner of a `new[]` array declared in the mode-specific
  namespace, so its functions are mode-specific; moving C handles uses `detail::take_handle` rather than
  `std::exchange` over a C pointer. Neither changes a public signature or a class's size or alignment (`RecordBatch`
  stays two pointers and four integers). What is left is `std::data` and `std::size` on a range of `NewRecord` given
  to `append_batch`, which throw only what the range's own `data()` or `size()` throws (nothing, for the standard
  containers and arrays).
- **What remains shared between modes** is standard-library code over built-in and standard types (`std::string`,
  `std::optional<std::uint64_t>` and the like), which any program that mixes exception modes shares with or without
  this header, and a container that the user's code instantiates over the binding's C types
  (`std::vector<tickertape::NewRecord>`: `NewRecord` is the C struct, outside the mode namespace), which is one
  instantiation for both modes like round 2's `std::vector<tickertape_segment_info>`, only outside the header. Mixing
  modes is therefore only as safe as the toolchain's standard library makes it. CI runs the mixed-mode test with GCC
  and Clang on Linux (libstdc++) and Apple Clang on macOS (libc++); Homebrew GCC on macOS (libstdc++) was run by
  hand, by setting `TICKERTAPE_FFI_CXX_COMPILERS` (on the macOS runner `g++` is Apple Clang).
- **Units built in different exception modes can be linked into one program**, and each keeps its own behaviour for
  the binding: `value()` throws in the one and aborts in the other, a fill's exception is rethrown, and an allocation
  failure inside a binding function reaches the caller's `catch`.
- **Binding types do not cross a boundary between exception modes.** `tickertape::Writer` in a unit built with
  exceptions and `tickertape::Writer` in one built without are different types (`v1::Writer` and
  `v1_noexcept::Writer`). Where a binding type is in a function's **parameter list**, it is part of the symbol's
  name, so a function declared as `bool is_open(const tickertape::Writer &)` in a shared header is a different
  function in each mode: defining it in a `-fno-exceptions` unit and calling it from an exceptions unit is a link
  error (undefined reference), when the call is a direct call by name of an ordinary C++ function. Nothing more is
  claimed: a binding type is not part of the symbol's name where it appears only in a non-template function's
  **return type**, in a data member or a global, or in an `extern "C"` function, whose name carries no types; what
  a function template's symbol encodes depends on how the type appears in it (a deduced return type or a trailing
  `requires` clause can leave it out); and a call through a virtual function or a function pointer is not resolved
  by name at all (a round-3 reviewer linked an interface with a
  virtual `feed(tickertape::Writer &)` implemented without exceptions and called with them). Such a crossing links,
  and is undefined behaviour. The rule is that binding types do not cross the boundary; the link error is a help for
  the commonest
  case, not a guarantee. Across such a boundary pass C handles (`native_handle()`, used through the C API on the
  other side) or plain data: `tickertape::NewRecord` is the C struct and is shared; sequence numbers, timestamps and
  payload bytes are plain values.
- **The debug-check mode is shared.** Debug and release units of one program commonly exchange the binding's types,
  and only the check differs, so the types are the same in both modes (the debug check's fields exist in every
  build; `[[maybe_unused]]` keeps Clang quiet when `NDEBUG` leaves them unread). Every inline function whose
  behaviour depends on the debug-check mode is declared `TICKERTAPE_HPP_MODE_DEPENDENT`, which expands to
  `__attribute__((abi_tag("tickertape_dbg")))` or `abi_tag("tickertape_nodbg")`, so each mode's copy is a different
  symbol. This covers `result<T>::operator*`, `operator->`, `error()` and its private checked access,
  `result<void>::error()`, `RecordView::data()` and `as_string_view()` (and the private check they call), `as_span`,
  `RecordBatch::operator[]` and its iterator's `operator*`, and the internal `detail::debug_check`. The header's
  other functions do not call them (they read a failed `result<void>` through an unchecked internal accessor), so
  their single copy behaves the same in both debug-check modes.
- **The C++ standard library must agree.** Units that exchange binding types (in the same exception mode) must use
  the same standard library with the same settings that affect its layout (for example `_GLIBCXX_USE_CXX11_ABI`, or
  libstdc++'s and libc++'s debug modes), as for any C++ library whose types contain `std::string`, `std::vector` or
  `std::optional`.
- **Not covered:** in the debug-check mode, where the types are shared, behaviour follows the code that acts on a
  value, not the code that made it (a view made in a release unit is checked when a debug unit reads its payload);
  a fill passed to `append_with` from a no-exceptions unit must not throw (nothing there catches it); your own inline
  functions that call debug-checked ones are yours to keep consistent; and MSVC is out of scope (it has no
  `abi_tag`): there, build the whole program in one mode.
- **Enforced** by a source check in `tests/c_api.rs` (it fails if anything in `namespace tickertape` is outside the
  mode-specific inline namespace, if a function whose body mentions a debug-check macro or calls a tagged function by
  name lacks the tag, or if a tagged function has no such reason; calls through operators are not seen, so those
  operators are tagged by hand), by `tests/cpp/layout.cpp` (the namespace name in each mode of the compile matrix),
  and by link tests: units built in each mode are linked into one program in both orders, at -O0 and -O2, asserting
  each unit's behaviour (no-exceptions with exceptions; debug checks with none); and passing a `tickertape::Writer`
  from a no-exceptions unit to an exceptions unit must fail to link, while the same two units built in one mode link.
  The allocation case fails every allocation of a `list_segments` call in turn (n = 1, 2, ... until the call
  succeeds), with the failure armed only after the replaced `operator new` and `operator new[]` have been probed
  (`detail::Array` allocates with `new[]`, which AddressSanitizer and Valgrind do not route through a replaced
  `operator new`), and asserts that each `std::bad_alloc` reaches the exceptions unit's `catch` and that at least
  the two known allocations were exercised.
  (Round 2's version armed it before its own probe, so n = 1 failed the probe and the first allocation inside the
  call was never failed, yet the test passed.) Where `nm` is available, the test also fails if any symbol defined in
  both the exceptions object and the no-exceptions object mentions `tickertape`, which catches C struct names
  inside standard-library instantiations; without `nm` it says so and skips that part. Against round 2's header both
  parts fail with Apple Clang: the symbol check lists `std::vector<tickertape_segment_info>` functions, and the
  allocation walk terminates with "uncaught exception of type std::bad_alloc" when the no-exceptions object comes
  first.

**Errors: one surface returning `tickertape::result<T>`** (option 3). `result<T>` holds a `T` or a
`tickertape::Error`; it is `[[nodiscard]]`, so a discarded result is a compiler warning (an ignored error is never
silent). `value()` returns the value or throws `tickertape::Exception` (a `std::runtime_error` holding the `Error`);
with exceptions disabled it prints the error and calls `std::abort`. `operator*` and `->` are unchecked, except in
debug builds. On an rvalue result, `value()`, `*` and `error()` return by value (one move) rather than a
reference, unlike `std::optional` and `std::expected`: GCC's `-Wdangling-reference` caught
`for (auto &s : list_segments(dir).value())` referring into a destroyed temporary in our own compile test, and the
move costs nothing next to a call into the library. These rules are fixed for 1.0:

- **`T` must be nothrow move-constructible** (`static_assert`): `std::variant` becomes valueless when a move throws
  during assignment, after which `value()` and `error()` would dereference null. Every `T` the binding returns
  qualifies, and so does `Error`.
- `result(const T &)` exists only for a copyable `T`, so `std::is_constructible_v<result<Writer>, const Writer &>`
  is false instead of true-and-then-a-hard-error. The converting constructors `result(T &&)`, `result(Error)` and
  `result<void>(Error)` are `noexcept` (`result(const T &)` is `noexcept` when `T`'s copy is).
- **A const rvalue result** (`std::move(const_result).value()`, a function returning `const result<T>`) hands out
  copies from `value()`, `*` and `error()`, and its `value()` and `*` are deleted when `T` cannot be copied
  (`result<Writer>`), so it never lends a reference into a temporary.
- **`Error::message()` is ref-qualified**: on an lvalue it returns `const std::string &`; on an rvalue it returns the
  string by value, so `const auto &m = writer.checkpoint().error().message();` binds to a string that lives as long
  as `m`. `name()` returns a `std::string_view` of static storage and the other accessors return values, so nothing
  else lends a reference.
- **`tickertape::Exception` is nothrow-copyable**, as every standard exception is (copying one during exception
  handling must not throw, or the program terminates): it holds its `Error` through a `std::shared_ptr<const Error>`
  created once in its constructor, so copies share it. This is a layout change, made before 1.0 for that reason.
  **It declares its copy operations and no move operations**, so moving one is a nothrow copy, as with
  `std::runtime_error`: an implicit move would leave the source's `shared_ptr` empty, and `status()`, `error()` or
  any other accessor on a moved-from exception (a handler that moves it into a queue, then logs from the original)
  would dereference null. A test calls every accessor on a moved-from and a move-assigned-from exception.

The header defines its own type rather than using `std::expected`, so that its types are the same at every language
level. Apart from `std::bad_alloc` (an error copies its message; `list_segments` returns a vector), the binding never
throws, except from `value()` and from `Writer::append_with`, which rethrows what the user's fill threw (below).

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
| `Reader::open`, `Reader::open_mapped_unsafe`, `seek`, `next(Timeout)`, `next_copy` (untyped memory or a writable byte range), `read_batch(RecordBatch&)`, `position`, `close`, destructor | `tickertape_reader_*` |
| `WriterOptions`, `ReaderOptions` (including `durable_only`), `Retention` | the option builders |
| `verify`, `list_segments`, `read_checkpoint`, `apply_retention`, `plan_retention` | inspection and `tickertape_retention_apply` (`dry_run`) |
| `library_abi_version`, `library_version_number`, `library_version`, `library_compatible`, `check_library`, `realtime_ns`, `status_name`, `corrupt_reason_name` | version and helper functions |
| `Error` | `tickertape_last_error_message`, `_os_code`, `_corrupt_reason` |

**Paths** are passed directly as the argument: a `const char *`, `std::string`, `std::string_view` (copied for the NUL
terminator; opening is not a hot path) or `std::filesystem::path`, without overload ambiguity. The parameter type
that accepts them, `detail::PathArg`, keeps a pointer into a `std::string` or `std::filesystem::path` argument,
which is safe because a temporary argument lives until the call returns, but would dangle in a variable
(`PathArg p = std::filesystem::path(root) / "journal";` points into a path destroyed at the end of the statement).
Rather than refuse rvalues (which would also refuse `Writer::open(dir / "journal")`, the commonest form) or copy every
path, the type is **an internal parameter type**: it lives in `tickertape::detail`, `tickertape::PathArg` does not
exist (a compile-fail test), and the documented way to pass a path is the path itself. Its protection is against
binding a temporary in the documented call forms, no more: code that names the `detail` type itself can still store
one and let it dangle (`tickertape::detail::PathArg p = std::string("x");` compiles, by guaranteed copy elision). A
function of yours that forwards a path takes a path type (`const std::filesystem::path &`, `const char *`) or a
template parameter, not the binding's parameter type. The
inspection results are C++ structs with
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

- `Writer`, `Reader`, the three builders and `RecordBatch` are move-only owners of one C handle (or, for
  `RecordBatch`, C++ buffers it allocates) each. A default-constructed `Writer` or `Reader` is empty; the builders
  (`WriterOptions`, `ReaderOptions`, `Retention`) start with the library's defaults; `RecordBatch` has no default
  constructor (it needs its capacities). A moved-from object is empty: destroying it or assigning to it is safe, and
  calls on it fail with `invalid_argument` (the C ABI's answer to a NULL handle).
- **The builders check the library before they allocate.** Each builder's constructor runs `check_library()` once,
  before it creates its C handle. If the loaded library cannot be used with the header, it creates no handle and
  records `unsupported_version` as its error, which the open (or retention) function that takes it returns; its
  setters then call nothing. So a mismatched library is never given a call other than the version queries, even
  through the builders (the stub test counts every other C call). `close()` reports the close's status; the
  destructor ignores it, as it must. `native_handle()` gives the C handle for code that calls C directly.
- **`RecordView`** is what `Reader::next` returns. Its payload borrows the reader's memory and is valid until the next
  call on that reader other than `position()`, or until the reader is moved or destroyed (the C rule, plus moves,
  which the binding treats as invalidating). Views from a `RecordBatch` are valid until the next read into that batch;
  views from `next_copy` point into the caller's buffer. The sequence number, schema id, timestamps, `size()` and
  `empty()` are copies and stay valid; `data()`, `as_string_view()` and `as_span()` read the payload and are
  checked. The check runs when the payload is fetched through the view: a pointer, `string_view` or `span` taken
  from it earlier has no continuing check (it is a plain pointer). `as_span` fetches the pointer before it tests for an
  empty payload, so a stale view of an empty record is caught too. **Debug check:** each reader and batch keeps a counter that every invalidating call and every move
  bumps; a view remembers the counter's address and value, and reading its payload after the counter moved calls
  `std::abort` with a message. This costs one increment per call and 16 bytes per view, in every build, so that the
  layout does not depend on the setting (see **Mixing modes**); the comparison runs when `TICKERTAPE_DEBUG_CHECKS` is 1 (the default
  unless `NDEBUG`). A view whose reader was destroyed is not detected (the counter is gone).
- **Records.** `NewRecord` (for `append_batch`) **is `tickertape_new_record`**, an alias rather than a wrapper.
  `append_batch` passes a pointer to its records to the C ABI, which indexes it as an array of `tickertape_new_record`;
  an array of wrapper objects is not an array of their members, whatever their size and alignment (it works with
  today's compilers, but the language does not promise it), so the element type must be the C type itself. The
  fields are public (`payload`, `payload_len`, `venue_ts_ns`, `local_ts_ns`, `schema_id`; `reserved` must be 0), a
  value-initialised record is empty, and it is plain data shared by both exception modes. The checked construction
  moves into factory functions: `tickertape::new_record(schema_id, venue_ts_ns, local_ts_ns, bytes)` and
  `new_record(schema_id, venue_ts_ns, local_ts_ns, data, size)`, so `batch.push_back(new_record(...))` and a
  `std::array` or `std::vector` of records stay allocation-free and go to the C ABI as they are. A record keeps a
  pointer, not a copy, so **`new_record` accepts an rvalue byte range only if its type is a view**: one for which
  the binding's own variable template `tickertape::enable_borrowed_bytes<T>` is true. It is `false` by default and
  `true` for `std::basic_string_view` and, where `std::span` exists, `std::span`; a user's own view type opts in by
  specialising it (`template <> inline constexpr bool tickertape::enable_borrowed_bytes<MyView> = true;`), at C++17
  as well, at namespace scope in the header that defines the type, before any use that instantiates the primary
  template. Within one translation unit a missing specialisation selects the deleted overload and a late one is
  "specialization after instantiation", both compile errors; but a program in which one unit instantiates the
  trait for the type with the specialisation in sight and another instantiates it without (by passing an rvalue of
  the type to `new_record`) is ill-formed with no diagnostic required, which is why it
  belongs in the type's own header. Round 2 picked up `std::ranges::enable_borrowed_range` automatically in C++20, which made the header's
  definition depend on the standard (**Language level**); the binding no longer consults it, so a C++20 type that
  opts only into it (or `std::ranges::subrange`) is refused unless it also specialises `enable_borrowed_bytes`. For
  any other range type the rvalue overload is deleted,
  so `new_record(1, 0, 0, std::to_string(i) + "...")` (a pointer into a string destroyed at the end of the statement)
  and rvalues of `std::array`, `std::vector` and `std::string` do not compile. Lvalues of any byte range are
  accepted, as are pointer-and-size pairs. A list of view types rather than a rule such as "trivially copyable": a
  trivially copyable type can own its bytes inline (`std::array`, a user's fixed buffer), and a rule that let one of
  those through would bring the dangling pointer back. `Writer::append` copies the payload before it returns, so it
  takes temporaries. A record built by hand (its fields set directly) is not checked: it is the C struct.
- **No hidden allocation on the append and read path**: `append`, `append_with`, `append_batch`, `next_seq`, `next`,
  `next_copy`, `read_batch` and `position` allocate nothing in the binding when they succeed. `RecordBatch` allocates
  its storage once, in its constructor. `tests/cpp/test_no_alloc.cpp` checks this by replacing `operator new` and
  counting during a loop of every one of those calls; a failure copies the error message and does allocate.

**The fill callback.** `append_with(schema, venue_ts, local_ts, size, fill)` takes any callable
`fill(std::uint8_t *buf, std::size_t len)` returning `void` (commit) or `bool` (`false` aborts; for code without
exceptions). The C callback is a `noexcept` trampoline that catches every exception, stores it and returns non-zero,
so no exception ever reaches the C ABI. After the call:

- a clean abort after a throw rethrows the user's exception unchanged; after `false` it returns `aborted`. The
  rethrow happens before anything that allocates (the binding tests the C status, not an `Error` built from it), so a
  failed allocation cannot replace the user's exception with `std::bad_alloc`;
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
rounded up to whole nanoseconds and saturate at `INT64_MAX` nanoseconds; the unsigned conversion for retention ages
and back-off sleeps saturates at `UINT64_MAX`. **Integral counts are converted exactly, for every period that
converts at all** (a period for which `std::ratio_divide<Period, std::nano>` overflows, such as
`std::ratio<INTMAX_MAX, INTMAX_MAX - 1>`, is rejected at compile time by the standard library), in
integer arithmetic (so `std::chrono::nanoseconds{9'200'000'000'000'000'000}` stays that value): a positive duration
never becomes 0 and never comes out below its exact value. The count is split into whole periods and a remainder;
where the period's `num * den` does not fit 64 bits, the remainder's share is computed with a 128-bit product where
the compiler has one (`__SIZEOF_INT128__`) and otherwise by long multiplication in 64-bit arithmetic that keeps the
quotient and remainder of the partial product (`detail::mul_div_ceil_portable`, tested against the 128-bit result).
Round 2 used `long double` there, which is a 53-bit `double` on Apple arm64: one tick of
`duration<int, ratio<1000000000, 999999999999999999>>` (just over 1 ns) gave 1 ns, and 986 of 1999 counts just
below 1000000007 of `duration<int64_t, ratio<999999937, 1000000007>>` came out low by up to about 70 ns.
Floating-point counts are compared with the first value the target cannot hold (2^63 or 2^64) before the cast. **A
zero or negative duration does not wait.** This deliberately differs from the C ABI, where a negative timeout waits
forever: in C++ a negative duration is usually a deadline that has passed, and waiting forever for it would be a bug
that is hard to see. Waiting forever needs the explicit `wait_forever`.

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

**One release of the C++ header per program.** Every translation unit of a program that includes `tickertape.hpp`
must use the same release of it. Mixing releases is a one-definition-rule violation, as with any header-only
library: the inline functions are emitted in every unit and the linker keeps one copy, and their behaviour depends on
the release (`library_compatible()` and `check_library()` carry the header's version constants; `verify()` and
retention pass the `sizeof` of C structs that grow in minor releases), so with mixed releases a 1.1 caller could run
a 1.0 copy and accept a 1.0 library. The round-1 text promised that components built against different minor
releases could be linked together because layouts are frozen; that cannot hold for inline functions and is
withdrawn. A newer C library with an older C++ header remains supported, by the rule above. The mismatch is not
detected at link time: an inline variable whose name carries the release would let two releases coexist silently,
and a deliberately conflicting strong symbol would detect it but break legitimate uses (a header-only library
included in several shared objects), so it is documented, not enforced.

**No data member is added to an existing class within a major version** (`Error`, `Writer`, `RecordView`,
`VerifyReport` and every other class), and none is removed or reordered. This no longer serves mixing releases; it
keeps rebuilds safe: rebuilding against a newer minor release never changes the size or alignment of your own types
that contain the binding's, so your own ABI does not change with a minor upgrade. A report that needs more
information gets a new type or a new function. The inline namespace's name carries the major version (and the
exception mode, above); a later major version that changes a layout moves to `tickertape::v2`. (Before 1.0 the
layout can still change within `v1`; round 2 changed `Exception` and `NewRecord`.) `tests/cpp/layout.cpp` pins the
size and alignment of every public class against a mirror of its members, in every compiler, language level and
mode of the compile matrix, and a test fails if a public class is missing from it. A test checks that the header's
version macros match the crate version, so a release cannot ship a stale header; the release checklist says to bump
them.

**The header's internal macros are undefined at its end** (`TICKERTAPE_HPP_MODE_DEPENDENT`, the tag and namespace
helpers, `TICKERTAPE_HPP_EXCEPTIONS`), so they do not leak into the includer's code; the public macros stay
(`TICKERTAPE_HPP_VERSION_*`, `TICKERTAPE_HPP_ABI_VERSION`, `TICKERTAPE_DEBUG_CHECKS`). The compile matrix checks both.

**Static linking with the GCC driver on macOS** does not work for aborted fills: the GCC driver (Homebrew GCC) passes
`-no_compact_unwind` to the linker, and the library aborts an append whose fill returns non-zero by unwinding
internally, so with `libtickertape.a` linked that way the process ends inside the library. Apple's toolchain
defaults and the shared library work. `docs/c-api.md` and `docs/cpp-api.md` say so, and the C++20 test program
(the statically linked one) is skipped in that configuration with that reason. A change to the C library so that an
ordinary abort does not unwind is a separate follow-up.

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
  macOS, under `-Wall -Wextra -Wpedantic -Wconversion -Wshadow -Werror`, with `static_assert`s for the API's
  compile-time properties), the language-level check (**Language level**), the mixed-mode link tests with their
  shared-symbol check and the mode-tag source check (**Mixing modes**),
  compile-fail and compile-pass programs as C++17 and C++20 (`new_record` and temporaries, `enable_borrowed_bytes`
  for a user's view, a payload with a throwing move, a const rvalue `result<Writer>`, `tickertape::PathArg`), and the
  coverage, struct-field, layout, enumerator and version
  checks above. `test_no_alloc.cpp` replaces every form of `operator new` (plain, array, aligned, `nothrow`) and
  first checks that a deliberate allocation of each kind is counted; under Valgrind it runs with
  `--soname-synonyms=somalloc=nouserintercepts`, without which Valgrind replaces the test's operators and nothing is
  counted (the positive control then fails), and the stub and link tests that inject allocation failures run the
  same way. CI also builds the examples with make,
  with CMake through `find_package`, and through `add_subdirectory`.
- The C header's C++11 promise (ADR 0028) is now tested by a small program of its own, since the example wrapper
  that used to cover it is gone.
- Not covered yet: the install test from a built package on every platform that ADR 0031 requires; it needs the C
  library archive, which also brings installed CMake files. The binding is otherwise delivered.
- New docs: `docs/cpp-api.md`; `docs/c-api.md` and `examples/c/` point to the binding.
