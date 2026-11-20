// tickertape C++ binding: a header-only C++17 wrapper over the C ABI in tickertape.h (ADR 0036).
// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Read docs/cpp-api.md first. In short:
//
// - Every fallible function returns `tickertape::result<T>`: a value or a `tickertape::Error`. `.value()` returns
//   the value or throws `tickertape::Exception` (with exceptions disabled it prints the error and aborts). The
//   header has the same API with and without exceptions. With GCC and Clang, translation units built with and
//   without exceptions (or debug checks) can be linked into one program, but binding types cannot be passed between
//   units built in different exception modes (ADR 0036 -> Mixing modes). Apart from std::bad_alloc, it throws only
//   from `.value()` and from `Writer::append_with`, which rethrows what your fill function threw.
// - `Writer`, `Reader`, `WriterOptions`, `ReaderOptions`, `Retention` and `RecordBatch` are move-only owners of one
//   C handle (or buffer) each. A default-constructed `Writer` or `Reader` is empty; the option builders start with
//   the defaults; `RecordBatch` has no default constructor. A moved-from object is empty: destroying or assigning to
//   it is safe, and calls on it fail with `Status::invalid_argument`.
// - Every translation unit of a program that includes this header must use the same release of it (ADR 0036 ->
//   Versioning).
// - `Reader::next` returns a `RecordView` that borrows the reader's memory: its payload is valid until the next call
//   on the same reader (other than `position` and `metrics`), or until the reader is moved or destroyed. Debug
//   builds check this.
// - Visible is not durable: a record is visible to readers when an append returns, and durable only after a later
//   successful `Writer::checkpoint()` (docs/crash-contract.md).
// - A writer or reader may move between threads, but must not be used by two threads at once, including through
//   const member functions. The header adds no global state. See docs/c-api.md for fork() and the per-thread error
//   state.
#ifndef TICKERTAPE_HPP
#define TICKERTAPE_HPP

#if !defined(__cplusplus) || __cplusplus < 201703L
#error "tickertape.hpp needs C++17 or later; from C, include tickertape.h"
#endif

#include "tickertape.h"

#include <chrono>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <exception>
#include <filesystem>
#include <iterator>
#include <limits>
#include <memory>
#include <optional>
#include <ratio>
#include <stdexcept>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <variant>
#include <vector>

#if __cplusplus >= 202002L && __has_include(<span>)
#include <span>
#endif

// ---- Version ----
//
// This header belongs to the tickertape release below and is shipped next to that release's tickertape.h. Before
// 1.0 (ABI version 0) it needs exactly that release's tickertape.h and library; from 1.0 the same ABI version and a
// tickertape.h and library at least as new as this header (ADR 0036 -> Versioning).

#define TICKERTAPE_HPP_VERSION_MAJOR 0
#define TICKERTAPE_HPP_VERSION_MINOR 4
#define TICKERTAPE_HPP_VERSION_PATCH 0
#define TICKERTAPE_HPP_VERSION_NUMBER \
    ((TICKERTAPE_HPP_VERSION_MAJOR * 1000000) + (TICKERTAPE_HPP_VERSION_MINOR * 1000) + TICKERTAPE_HPP_VERSION_PATCH)
#define TICKERTAPE_HPP_ABI_VERSION 0

#if TICKERTAPE_ABI_VERSION != TICKERTAPE_HPP_ABI_VERSION
#error "tickertape.hpp and tickertape.h have different ABI versions; use the files from one release"
#endif
#if TICKERTAPE_HPP_ABI_VERSION == 0
#if TICKERTAPE_VERSION_NUMBER != TICKERTAPE_HPP_VERSION_NUMBER
#error "tickertape.hpp and tickertape.h come from different releases; before 1.0 they must match exactly"
#endif
#else
#if TICKERTAPE_VERSION_NUMBER < TICKERTAPE_HPP_VERSION_NUMBER
#error "tickertape.h is older than tickertape.hpp"
#endif
#endif

// Debug checks (record views used after their reader moved on, unchecked access to an empty result): on unless
// NDEBUG is defined. Define TICKERTAPE_DEBUG_CHECKS to 0 or 1 to choose. See below for linking translation units
// built with and without them.
#ifndef TICKERTAPE_DEBUG_CHECKS
#ifdef NDEBUG
#define TICKERTAPE_DEBUG_CHECKS 0
#else
#define TICKERTAPE_DEBUG_CHECKS 1
#endif
#endif

#if defined(__cpp_exceptions)
#define TICKERTAPE_HPP_EXCEPTIONS 1
#else
#define TICKERTAPE_HPP_EXCEPTIONS 0
#endif

// Mixing modes (ADR 0036 -> Mixing modes).
//
// - Exception mode: everything below lives in the inline namespace tickertape::v1 when exceptions are enabled and
//   tickertape::v1_noexcept when they are not, so every definition the header provides has a symbol of its own in
//   each mode, including the exception handling a compiler generates inside it, and the header instantiates no
//   standard-library function that an exception can pass through over only its C types (detail::Array holds those;
//   what is left, std::data and std::size on a range given to append_batch, throws only what that range's own
//   data() or size() throws). Code still writes
//   tickertape::Writer. What remains shared between modes is standard-library code over built-in and standard types
//   (std::string, std::optional<std::uint64_t>, ...), as in any program that mixes modes: that is as safe as the
//   toolchain's standard library makes it. Binding types must not cross a boundary between exception modes (pass C
//   handles or plain data). A binding type in the parameter list of an ordinary C++ function called directly by
//   name is part of the symbol's name and makes such a crossing a link error. That is a help in the commonest case
//   and nothing to rely on: elsewhere a binding type is often not part of the name (for example a non-template
//   function's return type, a data member or global, an extern "C" function, whose name carries no types), and a
//   call through a virtual function or a function pointer is not resolved by name at all. Those crossings link and
//   are undefined behaviour.
// - Debug-check mode: the types are shared, so that debug and release units can exchange them. The inline functions
//   whose behaviour depends on the mode are declared TICKERTAPE_HPP_MODE_DEPENDENT, which gives them a different
//   symbol in each mode (GCC's and Clang's abi_tag). A test fails if a function that mentions a debug-check macro,
//   or calls such a function, lacks it. Other compilers (MSVC) get no tag: there, use one mode per program.
#if defined(__GNUC__) && !defined(_MSC_VER)
#if TICKERTAPE_DEBUG_CHECKS
#define TICKERTAPE_HPP_DEBUG_CHECKS_TAG_ "tickertape_dbg"
#else
#define TICKERTAPE_HPP_DEBUG_CHECKS_TAG_ "tickertape_nodbg"
#endif
#define TICKERTAPE_HPP_MODE_DEPENDENT \
    __attribute__((abi_tag(TICKERTAPE_HPP_DEBUG_CHECKS_TAG_)))
#else
#define TICKERTAPE_HPP_MODE_DEPENDENT
#endif

#if TICKERTAPE_HPP_EXCEPTIONS
#define TICKERTAPE_HPP_INLINE_NAMESPACE_ v1
#else
#define TICKERTAPE_HPP_INLINE_NAMESPACE_ v1_noexcept
#endif

namespace tickertape {
// The inline namespace carries the exception mode (above) and the major version. Do not forward-declare the
// header's classes or name the inline namespace; include the header and write tickertape::X.
inline namespace TICKERTAPE_HPP_INLINE_NAMESPACE_ {
#undef TICKERTAPE_HPP_INLINE_NAMESPACE_

// ---- Status codes ----

// Every tickertape_status. `ok` and `none` are successes and never appear in an Error. A newer library may return
// a code this header does not name: it is kept as is (the value is still a Status), and is an error if negative.
enum class Status : std::int32_t {
    ok = TICKERTAPE_OK,
    none = TICKERTAPE_NONE,
    invalid_argument = TICKERTAPE_E_INVALID_ARGUMENT,
    io = TICKERTAPE_E_IO,
    storage_full = TICKERTAPE_E_STORAGE_FULL,
    locked = TICKERTAPE_E_LOCKED,
    corrupt = TICKERTAPE_E_CORRUPT,
    unsupported_version = TICKERTAPE_E_UNSUPPORTED_VERSION,
    not_found = TICKERTAPE_E_NOT_FOUND,
    seq_unavailable = TICKERTAPE_E_SEQ_UNAVAILABLE,
    record_too_large = TICKERTAPE_E_RECORD_TOO_LARGE,
    durability_lost = TICKERTAPE_E_DURABILITY_LOST,
    sequence_exhausted = TICKERTAPE_E_SEQUENCE_EXHAUSTED,
    discarded = TICKERTAPE_E_DISCARDED,
    invalid_config = TICKERTAPE_E_INVALID_CONFIG,
    buffer_too_small = TICKERTAPE_E_BUFFER_TOO_SMALL,
    aborted = TICKERTAPE_E_ABORTED,
    busy = TICKERTAPE_E_BUSY,
    panic = TICKERTAPE_E_PANIC,
    // A reader beacon with this name is held by a running reader (ADR 0032). The C ABI does not create beacons yet,
    // so no function of this header returns it today; it is named so that every C status has a name here.
    beacon_in_use = TICKERTAPE_E_BEACON_IN_USE,
    unknown = TICKERTAPE_E_UNKNOWN,
};

// Why journal bytes failed validation (Error::corrupt_reason() for Status::corrupt, and verify reports).
enum class CorruptReason : std::int32_t {
    none = TICKERTAPE_CORRUPT_NONE,
    bad_magic = TICKERTAPE_CORRUPT_BAD_MAGIC,
    bad_segment_header = TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER,
    size_mismatch = TICKERTAPE_CORRUPT_SIZE_MISMATCH,
    base_seq_mismatch = TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH,
    bad_offset = TICKERTAPE_CORRUPT_BAD_OFFSET,
    sequence_exhausted = TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED,
    bad_length = TICKERTAPE_CORRUPT_BAD_LENGTH,
    bad_checksum = TICKERTAPE_CORRUPT_BAD_CHECKSUM,
    bad_sequence = TICKERTAPE_CORRUPT_BAD_SEQUENCE,
    bad_flags = TICKERTAPE_CORRUPT_BAD_FLAGS,
    bad_seal = TICKERTAPE_CORRUPT_BAD_SEAL,
    broken_chain = TICKERTAPE_CORRUPT_BROKEN_CHAIN,
    dangling_checkpoint = TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT,
    // A commit word reads 0 and evidence of committed records follows it (docs/crash-contract.md C11, ADR 0035).
    empty_commit_word = TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
    // A commit word reads 0 and the search of the bytes after it stopped at its work bound: inconclusive, and
    // treated like empty_commit_word.
    unchecked_tail = TICKERTAPE_CORRUPT_UNCHECKED_TAIL,
    unknown = TICKERTAPE_CORRUPT_UNKNOWN,
};

// What a writer does on open when the newest segment ends in a corrupt committed record (docs/format.md -> Recovery).
enum class RecoveryPolicy : std::int32_t {
    quarantine = TICKERTAPE_RECOVERY_QUARANTINE,  // copy the bad bytes to quarantine/ and continue (the default)
    fail = TICKERTAPE_RECOVERY_FAIL,              // fail the open with Status::corrupt
};

// The library's name for a status, such as "TICKERTAPE_E_IO" (static storage; never empty).
inline std::string_view status_name(Status status) noexcept {
    return tickertape_status_name(static_cast<tickertape_status>(status));
}

// The library's name for a corruption reason, such as "TICKERTAPE_CORRUPT_BAD_CHECKSUM" (static storage).
inline std::string_view corrupt_reason_name(CorruptReason reason) noexcept {
    return tickertape_corrupt_reason_name(static_cast<std::int32_t>(reason));
}

// ---- Errors ----

// A failed call: the status, the library's message, and the OS error and corruption reason where they apply. It
// is a plain value; copying it is cheap except for the message.
class Error {
public:
    Error(Status status, std::string message, std::int32_t os_code = 0,
          CorruptReason corrupt_reason = CorruptReason::none, std::size_t needed_size = 0)
        : status_(status),
          os_code_(os_code),
          corrupt_reason_(corrupt_reason),
          needed_size_(needed_size),
          message_(std::move(message)) {}

    // The details of the failing call that returned `status`, read from the calling thread's last-error state. Call
    // it right after that call, on the same thread.
    static Error from_last_call(tickertape_status status, std::size_t needed_size = 0) {
        return Error(static_cast<Status>(status), tickertape_last_error_message(), tickertape_last_error_os_code(),
                     static_cast<CorruptReason>(tickertape_last_error_corrupt_reason()), needed_size);
    }

    Status status() const noexcept { return status_; }
    // errno for Status::io and Status::storage_full (also after a failed clean-up of an aborted fill), else 0.
    std::int32_t os_code() const noexcept { return os_code_; }
    // The reason for Status::corrupt, else CorruptReason::none.
    CorruptReason corrupt_reason() const noexcept { return corrupt_reason_; }
    // For Status::buffer_too_small from Reader::next_copy and Reader::read_batch: the bytes the record needs.
    // Otherwise 0.
    std::size_t needed_size() const noexcept { return needed_size_; }
    // The library's message. On a temporary Error it returns the string by value, so that
    // `const auto &m = writer.checkpoint().error().message();` never refers into a destroyed Error.
    const std::string &message() const & noexcept { return message_; }
    std::string message() && noexcept { return std::move(message_); }
    std::string message() const && { return message_; }
    std::string_view name() const noexcept { return status_name(status_); }
    // "TICKERTAPE_E_IO: <message>".
    std::string to_string() const {
        std::string text(name());
        if (!message_.empty()) {
            text += ": ";
            text += message_;
        }
        return text;
    }

private:
    Status status_;
    std::int32_t os_code_;
    CorruptReason corrupt_reason_;
    std::size_t needed_size_;
    std::string message_;
};

// What result<T>::value() throws for an error. what() is Error::to_string(). Copying it never throws (as for every
// standard exception): the Error is shared between copies, not copied. It has no move operations: moving one copies
// it, so that a moved-from Exception keeps its Error (as std::runtime_error keeps its message) and every accessor
// still works on it.
class Exception : public std::runtime_error {
public:
    explicit Exception(Error error)
        : std::runtime_error(error.to_string()),
          error_(std::shared_ptr<const Error>(std::make_shared<Error>(std::move(error)))) {}
    // Declared so that no move operations are generated (a move would leave error_ null).
    Exception(const Exception &) = default;
    Exception &operator=(const Exception &) = default;
    ~Exception() override = default;

    const Error &error() const noexcept { return *error_; }
    Status status() const noexcept { return error_->status(); }

private:
    std::shared_ptr<const Error> error_;
};

namespace detail {
[[noreturn]] inline void fatal(const char *what) noexcept {
    std::fprintf(stderr, "tickertape: %s\n", what);
    std::fflush(stderr);
    std::abort();
}

[[noreturn]] inline void fail(const Error &error) {
#if TICKERTAPE_HPP_EXCEPTIONS
    throw Exception(error);
#else
    std::fprintf(stderr, "tickertape: result<T>::value() on an error: %s\n", error.to_string().c_str());
    std::fflush(stderr);
    std::abort();
#endif
}

TICKERTAPE_HPP_MODE_DEPENDENT inline void debug_check(bool ok, const char *what) noexcept {
#if TICKERTAPE_DEBUG_CHECKS
    if (!ok) fatal(what);
#else
    (void)ok;
    (void)what;
#endif
}
}  // namespace detail

// A value or an Error, like C++23's std::expected (the header does not use std::expected so that its types are the
// same at every language level). Discarding one is a compiler warning ([[nodiscard]]). On an rvalue result, value(),
// operator* and error() return by value (moving out, or copying out of a const rvalue; deleted for a const rvalue
// whose T cannot be copied), so `for (auto &s : list_segments(dir).value())` and `auto &&w =
// Writer::open(dir).value()` never refer into a destroyed temporary. T must be nothrow move-constructible, so that a
// result always holds a value or an error (never neither).
template <class T>
class [[nodiscard]] result {
public:
    static_assert(!std::is_same_v<std::remove_cv_t<T>, Error>, "result<Error> is not allowed");
    static_assert(!std::is_reference_v<T>, "result<T&> is not allowed");
    static_assert(std::is_nothrow_move_constructible_v<T>,
                  "result<T> needs a nothrow move-constructible T (a throwing move could leave it holding nothing)");

    template <class U = T, std::enable_if_t<std::is_copy_constructible_v<U>, int> = 0>
    result(const T &value) noexcept(std::is_nothrow_copy_constructible_v<T>) : v_(std::in_place_index<0>, value) {}
    result(T &&value) noexcept : v_(std::in_place_index<0>, std::move(value)) {}
    result(Error error) noexcept : v_(std::in_place_index<1>, std::move(error)) {}

    bool has_value() const noexcept { return v_.index() == 0; }
    explicit operator bool() const noexcept { return has_value(); }

    // The value; for an error, throws Exception (or, without exceptions, prints the error and aborts).
    T &value() & {
        if (!has_value()) detail::fail(*std::get_if<1>(&v_));
        return *std::get_if<0>(&v_);
    }
    const T &value() const & {
        if (!has_value()) detail::fail(*std::get_if<1>(&v_));
        return *std::get_if<0>(&v_);
    }
    T value() && {
        if (!has_value()) detail::fail(*std::get_if<1>(&v_));
        return std::move(*std::get_if<0>(&v_));
    }
    template <class U = T, std::enable_if_t<std::is_copy_constructible_v<U>, int> = 0>
    T value() const && {
        if (!has_value()) detail::fail(*std::get_if<1>(&v_));
        return *std::get_if<0>(&v_);
    }
    template <class U = T, std::enable_if_t<!std::is_copy_constructible_v<U>, int> = 0>
    T value() const && = delete;
    template <class U>
    T value_or(U &&fallback) const & {
        return has_value() ? *std::get_if<0>(&v_) : static_cast<T>(std::forward<U>(fallback));
    }
    template <class U>
    T value_or(U &&fallback) && {
        return has_value() ? std::move(*std::get_if<0>(&v_)) : static_cast<T>(std::forward<U>(fallback));
    }

    // Unchecked access (checked in debug builds).
    TICKERTAPE_HPP_MODE_DEPENDENT T &operator*() & noexcept { return *checked_value(); }
    TICKERTAPE_HPP_MODE_DEPENDENT const T &operator*() const & noexcept { return *checked_value(); }
    TICKERTAPE_HPP_MODE_DEPENDENT T operator*() && noexcept { return std::move(*checked_value()); }
    template <class U = T, std::enable_if_t<std::is_copy_constructible_v<U>, int> = 0>
    TICKERTAPE_HPP_MODE_DEPENDENT T operator*() const && noexcept(std::is_nothrow_copy_constructible_v<T>) {
        return *checked_value();
    }
    template <class U = T, std::enable_if_t<!std::is_copy_constructible_v<U>, int> = 0>
    T operator*() const && = delete;
    TICKERTAPE_HPP_MODE_DEPENDENT T *operator->() noexcept { return checked_value(); }
    TICKERTAPE_HPP_MODE_DEPENDENT const T *operator->() const noexcept { return checked_value(); }

    // The error. Only call it when has_value() is false (checked in debug builds).
    TICKERTAPE_HPP_MODE_DEPENDENT const Error &error() const & noexcept {
        detail::debug_check(!has_value(), "result<T>::error() on a value");
        return *std::get_if<1>(&v_);
    }
    TICKERTAPE_HPP_MODE_DEPENDENT Error error() && noexcept {
        detail::debug_check(!has_value(), "result<T>::error() on a value");
        return std::move(*std::get_if<1>(&v_));
    }
    TICKERTAPE_HPP_MODE_DEPENDENT Error error() const && {
        detail::debug_check(!has_value(), "result<T>::error() on a value");
        return *std::get_if<1>(&v_);
    }

private:
    TICKERTAPE_HPP_MODE_DEPENDENT T *checked_value() noexcept {
        detail::debug_check(has_value(), "result<T>: unchecked access to an error");
        return std::get_if<0>(&v_);
    }
    TICKERTAPE_HPP_MODE_DEPENDENT const T *checked_value() const noexcept {
        detail::debug_check(has_value(), "result<T>: unchecked access to an error");
        return std::get_if<0>(&v_);
    }
    std::variant<T, Error> v_;
};

namespace detail {
struct ResultAccess;
}  // namespace detail

// Success with no value, or an Error.
template <>
class [[nodiscard]] result<void> {
public:
    result() noexcept = default;
    result(Error error) noexcept : error_(std::move(error)) {}

    bool has_value() const noexcept { return !error_.has_value(); }
    explicit operator bool() const noexcept { return has_value(); }
    // Throws Exception for an error (without exceptions: prints it and aborts).
    void value() const {
        if (error_) detail::fail(*error_);
    }
    TICKERTAPE_HPP_MODE_DEPENDENT const Error &error() const & noexcept {
        detail::debug_check(error_.has_value(), "result<void>::error() on a success");
        return *error_;
    }
    TICKERTAPE_HPP_MODE_DEPENDENT Error error() && noexcept {
        detail::debug_check(error_.has_value(), "result<void>::error() on a success");
        return std::move(*error_);
    }
    TICKERTAPE_HPP_MODE_DEPENDENT Error error() const && {
        detail::debug_check(error_.has_value(), "result<void>::error() on a success");
        return *error_;
    }

private:
    friend struct detail::ResultAccess;
    std::optional<Error> error_;
};

namespace detail {
// The error of a failed result<void>, without error()'s debug check, so that the header's own functions do not
// depend on the debug-check mode. Only call it after testing the result.
struct ResultAccess {
    static Error take_error(result<void> &&failed) noexcept { return std::move(*failed.error_); }
};
inline Error take_error(result<void> &&failed) noexcept { return ResultAccess::take_error(std::move(failed)); }

inline result<void> status_to_result(tickertape_status status) {
    if (status < 0) return Error::from_last_call(status);
    return {};
}
}  // namespace detail

// ---- Library version ----

// The C ABI version of the loaded library (0 before 1.0: unstable).
inline std::uint32_t library_abi_version() noexcept { return tickertape_abi_version(); }
// The loaded library's version as MAJOR * 1000000 + MINOR * 1000 + PATCH.
inline std::uint32_t library_version_number() noexcept { return tickertape_version_number(); }
// The loaded library's version, such as "0.4.0".
inline std::string_view library_version() noexcept { return tickertape_version(); }

// Whether the loaded library can be used with this header: before 1.0 exactly this header's version, from 1.0 the
// same ABI version and a library at least as new as this header. Every function that takes a journal path checks
// it.
inline bool library_compatible() noexcept {
    const std::uint32_t lib = tickertape_version_number();
    const std::uint32_t mine = static_cast<std::uint32_t>(TICKERTAPE_HPP_VERSION_NUMBER);
    if (tickertape_abi_version() != static_cast<std::uint32_t>(TICKERTAPE_HPP_ABI_VERSION)) return false;
#if TICKERTAPE_HPP_ABI_VERSION == 0
    return lib == mine;
#else
    return lib >= mine;
#endif
}

// Succeeds if library_compatible(); otherwise Status::unsupported_version with both versions in the message.
inline result<void> check_library() {
    if (library_compatible()) return {};
    std::string message = "the loaded libtickertape ";
    message += tickertape_version();
    message += " (ABI ";
    message += std::to_string(tickertape_abi_version());
    message += ") cannot be used with tickertape.hpp ";
    message += std::to_string(TICKERTAPE_HPP_VERSION_MAJOR) + "." + std::to_string(TICKERTAPE_HPP_VERSION_MINOR) +
               "." + std::to_string(TICKERTAPE_HPP_VERSION_PATCH);
    message += " (ABI " + std::to_string(TICKERTAPE_HPP_ABI_VERSION) + ")";
    return Error(Status::unsupported_version, std::move(message));
}

// The system wall-clock time in nanoseconds since the Unix epoch (CLOCK_REALTIME: it can jump), or 0 before the
// epoch. A convenience for record timestamps, which the library never interprets.
inline std::uint64_t realtime_ns() noexcept { return tickertape_realtime_ns(); }

// ---- Arguments ----

namespace detail {
// How the functions that take a journal path receive it: a `const char *` (NUL-terminated), `std::string`,
// `std::string_view` (copied, since the C ABI needs a NUL terminator) or `std::filesystem::path`, passed directly as
// the argument. Paths go to the operating system unchanged; a path with a NUL byte inside fails with
// Status::invalid_argument.
//
// Not part of the API (ADR 0036 -> Paths): it can point into a temporary `std::string` or `std::filesystem::path`,
// which is safe because a temporary argument lives until the call returns. That protects the documented call forms
// (the path passed directly as the argument) only: code that names this detail type itself can still store one and
// let it dangle (`detail::PathArg p = std::string("x");` compiles, by guaranteed copy elision). Pass the path
// itself; a function that forwards a path takes a path type or a template parameter.
class PathArg {
public:
    PathArg(const char *path) noexcept : ptr_(path) {}
    PathArg(const std::string &path) noexcept : ptr_(path.c_str()), has_nul_(path.find('\0') != std::string::npos) {}
    PathArg(std::string_view path)
        : owned_(path), ptr_(owned_.c_str()), has_nul_(path.find('\0') != std::string_view::npos) {}
    PathArg(const std::filesystem::path &path) noexcept
        : ptr_(path.c_str()), has_nul_(path.native().find('\0') != std::filesystem::path::string_type::npos) {}
    PathArg(const PathArg &) = delete;
    PathArg &operator=(const PathArg &) = delete;

    const char *c_str() const noexcept { return ptr_; }

    // Status::invalid_argument for a path with an embedded NUL byte; otherwise success.
    result<void> check() const {
        if (has_nul_) return Error(Status::invalid_argument, "the path contains a NUL byte");
        return {};
    }

private:
    std::string owned_;
    const char *ptr_;
    bool has_nul_ = false;
};

// ceil(a * b / d), exactly, for a < d <= INTMAX_MAX (so the result is at most b), in std::uintmax_t arithmetic
// only: long multiplication over the bits of b, keeping the quotient and the remainder (below d) of the partial
// product, so nothing overflows. Used where there is no 128-bit integer type, and tested against it.
constexpr std::uintmax_t mul_div_ceil_portable(std::uintmax_t a, std::uintmax_t b, std::uintmax_t d) noexcept {
    std::uintmax_t quotient = 0;
    std::uintmax_t remainder = 0;  // quotient * d + remainder == a * (the bits of b seen so far), remainder < d
    for (int bit = std::numeric_limits<std::uintmax_t>::digits - 1; bit >= 0; bit--) {
        quotient <<= 1;
        remainder <<= 1;  // below 2 * d, which fits
        if (remainder >= d) {
            remainder -= d;
            quotient++;
        }
        if (((b >> bit) & 1u) != 0) {
            remainder += a;  // below 2 * d
            if (remainder >= d) {
                remainder -= d;
                quotient++;
            }
        }
    }
    return quotient + (remainder != 0 ? 1 : 0);
}

// The same, with a 128-bit product where the compiler has one (GCC and Clang on 64-bit targets). The condition is a
// property of the target, not of the language level: it is the same in every unit of a program built for one
// target, and both bodies compute the same exact value.
#if defined(__SIZEOF_INT128__)
constexpr std::uintmax_t mul_div_ceil(std::uintmax_t a, std::uintmax_t b, std::uintmax_t d) noexcept {
    __extension__ typedef unsigned __int128 wide;
    const wide product = static_cast<wide>(a) * static_cast<wide>(b);
    return static_cast<std::uintmax_t>(product / d + (product % d != 0 ? 1 : 0));
}
#else
constexpr std::uintmax_t mul_div_ceil(std::uintmax_t a, std::uintmax_t b, std::uintmax_t d) noexcept {
    return mul_div_ceil_portable(a, b, d);
}
#endif

// `d` in whole nanoseconds, rounded up, as an Int (std::int64_t or std::uint64_t): 0 for a zero, negative or NaN
// duration (callers for which that is wrong reject those first), and the largest Int for a duration that does not
// fit. Integral counts are converted exactly, in integer arithmetic, for every period that converts at all (a
// period for which std::ratio_divide<Period, std::nano> overflows does not compile). Floating-point ones are
// compared, before the cast, with 2^63 or 2^64, the first value Int cannot hold.
template <class Int, class Rep, class Period>
constexpr Int ceil_ns(std::chrono::duration<Rep, Period> d) noexcept {
    static_assert(std::is_same_v<Int, std::int64_t> || std::is_same_v<Int, std::uint64_t>, "an int64 or uint64");
    constexpr Int cap = std::numeric_limits<Int>::max();
    if (!(d > std::chrono::duration<Rep, Period>::zero())) return 0;  // also NaN
    using Ratio = std::ratio_divide<Period, std::nano>;               // nanoseconds per tick: num / den, reduced
    if constexpr (std::is_integral_v<Rep>) {
        static_assert(sizeof(Rep) <= sizeof(std::uintmax_t), "duration counts wider than std::uintmax_t");
        using U = std::uintmax_t;
        constexpr U num = static_cast<U>(Ratio::num);
        constexpr U den = static_cast<U>(Ratio::den);
        constexpr U ucap = static_cast<U>(cap);
        const U count = static_cast<U>(d.count());  // positive
        const U whole = count / den;
        const U rest = count % den;
        if (whole > ucap / num) return cap;
        const U ns = whole * num;
        U extra = 0;  // rest * num / den, rounded up, exactly; at most num, and at least 1 (rest > 0)
        if (rest != 0) {
            if constexpr (num <= std::numeric_limits<U>::max() / den) {
                extra = rest * num / den + (rest * num % den != 0 ? 1 : 0);
            } else {  // rest * num can exceed 64 bits
                extra = mul_div_ceil(rest, num, den);
            }
        }
        if (ns > ucap - extra) return cap;
        return static_cast<Int>(ns + extra);
    } else {
        constexpr long double limit = std::is_signed_v<Int> ? 0x1p63L : 0x1p64L;
        const long double ticks = static_cast<long double>(d.count());
        // Far beyond the limit (with a factor 2 of margin for rounding): saturate before the product can overflow.
        if (!(ticks < 2 * limit * static_cast<long double>(Ratio::den) / static_cast<long double>(Ratio::num))) {
            return cap;
        }
        const long double ns = ticks * static_cast<long double>(Ratio::num) / static_cast<long double>(Ratio::den);
        if (!(ns < limit)) return cap;
        const Int whole = static_cast<Int>(ns);
        return static_cast<long double>(whole) < ns && whole != cap ? whole + 1 : whole;
    }
}
}  // namespace detail

// A timeout for the read functions. Default-constructed (or `no_wait`) does not wait; a positive duration waits up
// to that long (rounded up to whole nanoseconds; integral counts are converted exactly; a duration longer than
// INT64_MAX nanoseconds, about 292 years, waits INT64_MAX nanoseconds); `wait_forever` waits until a record arrives
// and cannot be interrupted. A zero or negative duration (a deadline that has passed) does not wait: unlike the C
// ABI, where a negative timeout waits forever. A timeout bounds only the wait for a record: catching up after a seek
// and file I/O can take longer.
class Timeout {
public:
    struct Forever {};

    constexpr Timeout() noexcept = default;
    constexpr Timeout(Forever) noexcept : ns_(TICKERTAPE_WAIT_FOREVER) {}
    template <class Rep, class Period>
    constexpr Timeout(std::chrono::duration<Rep, Period> timeout) noexcept
        : ns_(detail::ceil_ns<std::int64_t>(timeout)) {}

    // The value passed to the C ABI: 0, a positive number of nanoseconds, or -1 for forever.
    constexpr std::int64_t c_value() const noexcept { return ns_; }
    constexpr bool is_forever() const noexcept { return ns_ < 0; }

private:
    std::int64_t ns_ = TICKERTAPE_NO_WAIT;
};

inline constexpr Timeout no_wait{};
inline constexpr Timeout::Forever wait_forever{};

// Whether new_record() may keep a pointer into an rvalue of the byte range T: true only for views, whose bytes live
// elsewhere and outlive the view (ADR 0036 -> Records). true for std::basic_string_view and, where the standard
// library has it, std::span; false for everything else, including std::string, std::vector and std::array, which own
// their bytes. Specialise it for a view type of your own (cv-unqualified, at namespace scope, in the header that
// defines the type, so that every translation unit sees it before its first use):
//
//     template <> inline constexpr bool tickertape::enable_borrowed_bytes<MyView> = true;
//
// It is the binding's own opt-in, the same at every language level; std::ranges::enable_borrowed_range is not
// consulted (that would make the header's definitions depend on the standard).
template <class T>
inline constexpr bool enable_borrowed_bytes = false;
template <class Char, class Traits>
inline constexpr bool enable_borrowed_bytes<std::basic_string_view<Char, Traits>> = true;
#if __cplusplus >= 202002L && defined(__cpp_lib_span) && __cpp_lib_span >= 202002L
template <class T, std::size_t Extent>
inline constexpr bool enable_borrowed_bytes<std::span<T, Extent>> = true;
#endif

namespace detail {
template <class T>
struct is_byte : std::false_type {};
template <>
struct is_byte<char> : std::true_type {};
template <>
struct is_byte<signed char> : std::true_type {};
template <>
struct is_byte<unsigned char> : std::true_type {};
template <>
struct is_byte<std::byte> : std::true_type {};
#if defined(__cpp_char8_t)
template <>
struct is_byte<char8_t> : std::true_type {};
#endif

template <class R, class = void>
struct byte_range : std::false_type {};
template <class R>
struct byte_range<R, std::void_t<decltype(std::data(std::declval<const R &>())),
                                 decltype(std::size(std::declval<const R &>()))>>
    : std::bool_constant<!std::is_array_v<R> &&
                         std::is_pointer_v<decltype(std::data(std::declval<const R &>()))> &&
                         is_byte<std::remove_cv_t<
                             std::remove_pointer_t<decltype(std::data(std::declval<const R &>()))>>>::value> {};

// Contiguous ranges of bytes or chars: std::string, std::string_view, std::vector<std::uint8_t>, std::array,
// std::span, ... C arrays are refused, because a string literal would include its NUL terminator.
template <class R>
inline constexpr bool is_byte_range_v = byte_range<std::remove_cv_t<std::remove_reference_t<R>>>::value;

// Contiguous ranges of writable bytes or chars, for Reader::next_copy: the above with a non-const data(), plus C
// arrays (a buffer, not a payload, so a NUL terminator is not an issue).
template <class R, class = void>
struct writable_byte_range : std::false_type {};
template <class R>
struct writable_byte_range<R, std::void_t<decltype(std::data(std::declval<R &>())),
                                          decltype(std::size(std::declval<R &>()))>>
    : std::bool_constant<std::is_pointer_v<decltype(std::data(std::declval<R &>()))> &&
                         !std::is_const_v<std::remove_pointer_t<decltype(std::data(std::declval<R &>()))>> &&
                         is_byte<std::remove_cv_t<
                             std::remove_pointer_t<decltype(std::data(std::declval<R &>()))>>>::value> {};
template <class R>
inline constexpr bool is_writable_byte_range_v = writable_byte_range<std::remove_reference_t<R>>::value;

// Whether new_record() accepts an rvalue of R: true for views, whose bytes outlive them (see enable_borrowed_bytes).
template <class R>
inline constexpr bool is_byte_view_v = enable_borrowed_bytes<std::remove_cv_t<std::remove_reference_t<R>>>;

// An owned array of `size` value-initialised T (none for 0), allocated with new[] (which can throw std::bad_alloc),
// freed by the destructor; move-only and pointer-sized, like std::unique_ptr<T[]>. The header holds the C structs
// (and the bytes) it owns in this rather than in std::vector or std::unique_ptr: those, instantiated with a C struct,
// would be one standard-library symbol shared by both exception modes, so the linker could keep the copy built
// without exceptions for the whole program, and a std::bad_alloc passing through it would end the process. Declared
// here, in the mode-specific namespace, each mode has its own copy (ADR 0036 -> Mixing modes).
template <class T>
class Array {
public:
    Array() noexcept = default;
    explicit Array(std::size_t size) : p_(size != 0 ? new T[size]() : nullptr) {}
    Array(Array &&other) noexcept : p_(other.p_) { other.p_ = nullptr; }
    Array &operator=(Array &&other) noexcept {
        if (this != &other) {
            delete[] p_;
            p_ = other.p_;
            other.p_ = nullptr;
        }
        return *this;
    }
    Array(const Array &) = delete;
    Array &operator=(const Array &) = delete;
    ~Array() { delete[] p_; }

    T *get() const noexcept { return p_; }
    T &operator[](std::size_t index) const noexcept { return p_[index]; }

private:
    T *p_ = nullptr;
};

// Empties a C handle and returns its old value (std::exchange, without a standard-library instantiation over a C
// type that both exception modes would share).
template <class T>
T *take_handle(T *&handle) noexcept {
    T *old = handle;
    handle = nullptr;
    return old;
}

// A duration in whole nanoseconds for the C ABI, rounded up: negative and NaN durations are 0 (callers for which
// that is wrong reject them first); integral counts are exact; a duration longer than UINT64_MAX nanoseconds is
// UINT64_MAX.
template <class Rep, class Period>
constexpr std::uint64_t duration_ns(std::chrono::duration<Rep, Period> d) noexcept {
    return ceil_ns<std::uint64_t>(d);
}

// For the option builders: the error that check_options() returns, in this order: a setter's first error (also a
// library that does not match the header), then a missing handle (moved from).
template <class Options>
inline result<void> check_options(const Options &options) {
    if (options.error()) return *options.error();
    if (options.native_handle() == nullptr) return Error(Status::invalid_argument, "the options were moved from");
    return {};
}
}  // namespace detail

// ---- Options ----
//
// The option builders start with the defaults and record the first error a setter reports, so setters can be
// chained without checking each one; the open function that takes the options returns that error. Each builder owns
// a C handle and is move-only. The constructor checks the loaded library first (check_library()): if it cannot be
// used with this header, it creates no C handle and records Status::unsupported_version as its error, and neither
// it nor a setter calls a C function other than the version queries. Without a handle (moved from, or that
// mismatch), a setter records Status::invalid_argument (unless an error is recorded already) and calls nothing.

// Retention settings (ADR 0016): limits, plus an action that must be chosen explicitly (there is no default that
// deletes). Only a sealed prefix strictly before the checkpoint segment is ever removed. Starts with no limits and
// no action.
class Retention {
public:
    Retention() {
        if (auto compatible = check_library(); !compatible) {
            error_ = detail::take_error(std::move(compatible));
        } else {
            h_ = tickertape_retention_new();
        }
    }
    Retention(Retention &&other) noexcept
        : h_(detail::take_handle(other.h_)), error_(std::exchange(other.error_, std::nullopt)) {}
    Retention &operator=(Retention &&other) noexcept {
        if (this != &other) {
            if (h_ != nullptr) tickertape_retention_free(h_);
            h_ = detail::take_handle(other.h_);
            error_ = std::exchange(other.error_, std::nullopt);
        }
        return *this;
    }
    Retention(const Retention &) = delete;
    Retention &operator=(const Retention &) = delete;
    ~Retention() {
        if (h_ != nullptr) tickertape_retention_free(h_);
    }

    // Keep at most `count` segment files in total (the checkpoint segment and later ones are always kept).
    Retention &max_segments(std::uint64_t count) {
        return usable() ? record(tickertape_retention_set_max_segments(h_, count)) : *this;
    }
    // Keep the total size of segment files at or below `bytes`.
    Retention &max_bytes(std::uint64_t bytes) {
        return usable() ? record(tickertape_retention_set_max_bytes(h_, bytes)) : *this;
    }
    // Remove segments sealed longer ago than `age` (by the writer's wall clock), rounded up to whole nanoseconds.
    // A negative or NaN age is an error (Status::invalid_argument), not zero: zero makes every sealed segment old
    // enough to remove.
    template <class Rep, class Period>
    Retention &max_age(std::chrono::duration<Rep, Period> age) {
        // Not `age >= zero()`: std::chrono defines >= as !(age < zero()), which is true for NaN.
        using Duration = std::chrono::duration<Rep, Period>;
        if (!(age > Duration::zero()) && !(age == Duration::zero())) {
            if (!error_) error_ = Error(Status::invalid_argument, "the retention age is negative or NaN");
            return *this;
        }
        return usable() ? record(tickertape_retention_set_max_age_ns(h_, detail::duration_ns(age))) : *this;
    }
    // Permanently delete eligible segments.
    Retention &delete_segments() { return usable() ? record(tickertape_retention_set_delete(h_)) : *this; }
    // Move eligible segments into the existing directory `dir`, keeping their names (never replacing a file).
    Retention &move_to(const detail::PathArg &dir) {
        if (auto checked = dir.check(); !checked) {
            if (!error_) error_ = detail::take_error(std::move(checked));
            return *this;
        }
        return usable() ? record(tickertape_retention_set_move_to(h_, dir.c_str())) : *this;
    }

    // The first error a setter reported, if any.
    const std::optional<Error> &error() const noexcept { return error_; }
    // The C handle, for C setters the binding does not wrap. Errors those report are not recorded in error().
    tickertape_retention *native_handle() noexcept { return h_; }
    const tickertape_retention *native_handle() const noexcept { return h_; }

private:
    // Whether there is a C handle to call a setter on; if not, records why (unless an error is recorded already).
    bool usable() {
        if (h_ != nullptr) return true;
        if (!error_) error_ = Error(Status::invalid_argument, "the options were moved from");
        return false;
    }
    Retention &record(tickertape_status status) {
        if (status < 0 && !error_) error_ = Error::from_last_call(status);
        return *this;
    }
    tickertape_retention *h_ = nullptr;
    std::optional<Error> error_;
};

// Writer options: segment size, recovery policy, wake-ups, background prefaulting, retention. Starts with the
// defaults.
class WriterOptions {
public:
    WriterOptions() {
        if (auto compatible = check_library(); !compatible) {
            error_ = detail::take_error(std::move(compatible));
        } else {
            h_ = tickertape_writer_options_new();
        }
    }
    WriterOptions(WriterOptions &&other) noexcept
        : h_(detail::take_handle(other.h_)), error_(std::exchange(other.error_, std::nullopt)) {}
    WriterOptions &operator=(WriterOptions &&other) noexcept {
        if (this != &other) {
            if (h_ != nullptr) tickertape_writer_options_free(h_);
            h_ = detail::take_handle(other.h_);
            error_ = std::exchange(other.error_, std::nullopt);
        }
        return *this;
    }
    WriterOptions(const WriterOptions &) = delete;
    WriterOptions &operator=(const WriterOptions &) = delete;
    ~WriterOptions() {
        if (h_ != nullptr) tickertape_writer_options_free(h_);
    }

    // Size of new segment files: a multiple of 65,536 from 65,536 to 2^40 (default 128 MiB). Checked on open
    // (Status::invalid_config).
    WriterOptions &segment_size(std::uint64_t bytes) {
        return usable() ? record(tickertape_writer_options_set_segment_size(h_, bytes)) : *this;
    }
    WriterOptions &recovery_policy(RecoveryPolicy policy) {
        return usable() ? record(tickertape_writer_options_set_recovery_policy(h_, static_cast<std::int32_t>(policy)))
                        : *this;
    }
    // Wake blocked readers through the shared-memory wake-up region (default on).
    WriterOptions &notify(bool enabled) {
        return usable() ? record(tickertape_writer_options_set_notify(h_, enabled ? 1 : 0)) : *this;
    }
    // Prefault pages (and on macOS prepare the next segment) in background threads (default on). Off gives a writer
    // that starts no threads.
    WriterOptions &prefault(bool enabled) {
        return usable() ? record(tickertape_writer_options_set_prefault(h_, enabled ? 1 : 0)) : *this;
    }
    // Apply `retention` (copied) after open and after every successful checkpoint. Its failures never fail the
    // writer; see Writer::last_retention_error(). Settings with an error (or moved from) record that error here.
    WriterOptions &retention(const Retention &settings) {
        if (auto checked = detail::check_options(settings); !checked) {
            // A moved-from Retention has no handle, which the C ABI would read as "no retention".
            if (!error_) error_ = detail::take_error(std::move(checked));
            return *this;
        }
        return usable() ? record(tickertape_writer_options_set_retention(h_, settings.native_handle())) : *this;
    }
    WriterOptions &no_retention() {
        return usable() ? record(tickertape_writer_options_set_retention(h_, nullptr)) : *this;
    }

    const std::optional<Error> &error() const noexcept { return error_; }
    // The C handle, for C setters the binding does not wrap. Errors those report are not recorded in error().
    tickertape_writer_options *native_handle() noexcept { return h_; }
    const tickertape_writer_options *native_handle() const noexcept { return h_; }

private:
    // Whether there is a C handle to call a setter on; if not, records why (unless an error is recorded already).
    bool usable() {
        if (h_ != nullptr) return true;
        if (!error_) error_ = Error(Status::invalid_argument, "the options were moved from");
        return false;
    }
    WriterOptions &record(tickertape_status status) {
        if (status < 0 && !error_) error_ = Error::from_last_call(status);
        return *this;
    }
    tickertape_writer_options *h_ = nullptr;
    std::optional<Error> error_;
};

// Reader options: durable-only reads, wake-ups and how a waiting reader backs off. Starts with the defaults.
class ReaderOptions {
public:
    ReaderOptions() {
        if (auto compatible = check_library(); !compatible) {
            error_ = detail::take_error(std::move(compatible));
        } else {
            h_ = tickertape_reader_options_new();
        }
    }
    ReaderOptions(ReaderOptions &&other) noexcept
        : h_(detail::take_handle(other.h_)), error_(std::exchange(other.error_, std::nullopt)) {}
    ReaderOptions &operator=(ReaderOptions &&other) noexcept {
        if (this != &other) {
            if (h_ != nullptr) tickertape_reader_options_free(h_);
            h_ = detail::take_handle(other.h_);
            error_ = std::exchange(other.error_, std::nullopt);
        }
        return *this;
    }
    ReaderOptions(const ReaderOptions &) = delete;
    ReaderOptions &operator=(const ReaderOptions &) = delete;
    ~ReaderOptions() {
        if (h_ != nullptr) tickertape_reader_options_free(h_);
    }

    // Return only durable records: those below the checkpoint marker's next_seq (crash contract C13). A durable-only
    // reader treats later records as not written yet, and returns nothing without a valid checkpoint marker.
    ReaderOptions &durable_only(bool enabled) {
        return usable() ? record(tickertape_reader_options_set_durable_only(h_, enabled ? 1 : 0)) : *this;
    }
    // Block on the writer's wake-up region instead of sleeping (default on).
    ReaderOptions &notify(bool enabled) {
        return usable() ? record(tickertape_reader_options_set_notify(h_, enabled ? 1 : 0)) : *this;
    }
    // Wait with `spins` busy polls, then `yields` polls that yield, then sleeps from `min_sleep` doubling to
    // `max_sleep` (min_sleep must not exceed max_sleep). Negative sleeps count as zero.
    template <class Rep1, class Period1, class Rep2, class Period2>
    ReaderOptions &backoff(std::uint32_t spins, std::uint32_t yields, std::chrono::duration<Rep1, Period1> min_sleep,
                           std::chrono::duration<Rep2, Period2> max_sleep) {
        if (!usable()) return *this;
        return record(tickertape_reader_options_set_backoff(h_, spins, yields, detail::duration_ns(min_sleep),
                                                            detail::duration_ns(max_sleep)));
    }
    // Spin without ever yielding or sleeping: lowest latency, one core at 100 %.
    ReaderOptions &busy_spin() { return usable() ? record(tickertape_reader_options_set_busy_spin(h_)) : *this; }

    const std::optional<Error> &error() const noexcept { return error_; }
    // The C handle, for C setters the binding does not wrap. Errors those report are not recorded in error().
    tickertape_reader_options *native_handle() noexcept { return h_; }
    const tickertape_reader_options *native_handle() const noexcept { return h_; }

private:
    // Whether there is a C handle to call a setter on; if not, records why (unless an error is recorded already).
    bool usable() {
        if (h_ != nullptr) return true;
        if (!error_) error_ = Error(Status::invalid_argument, "the options were moved from");
        return false;
    }
    ReaderOptions &record(tickertape_status status) {
        if (status < 0 && !error_) error_ = Error::from_last_call(status);
        return *this;
    }
    tickertape_reader_options *h_ = nullptr;
    std::optional<Error> error_;
};

// ---- Records ----

// A record to append with Writer::append_batch: the C struct tickertape_new_record itself (an alias, not a wrapper),
// so that a C array, std::array or std::vector of them is an array of the C type and goes to the C ABI as it is.
// Its fields are public: payload, payload_len, venue_ts_ns, local_ts_ns, schema_id (and reserved, which must be 0).
// Build one with new_record(), which checks the payload's type; a value-initialised one (`NewRecord{}`) is an empty
// payload with zero fields. It does not copy the payload: the bytes must stay valid and unmodified until the append
// returns.
using NewRecord = ::tickertape_new_record;

// A NewRecord for the `size` bytes at `data` (not copied).
inline NewRecord new_record(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                            const void *data, std::size_t size) noexcept {
    NewRecord record{};
    record.payload = static_cast<const std::uint8_t *>(data);
    record.payload_len = size;
    record.venue_ts_ns = venue_ts_ns;
    record.local_ts_ns = local_ts_ns;
    record.schema_id = schema_id;
    return record;
}

// A NewRecord for a contiguous range of bytes or chars, which it points into (ADR 0036 -> Records). Because the
// record keeps a pointer, a temporary that owns its bytes (`std::to_string(i) + "..."`, a std::vector or std::array
// returned by a function) would leave it pointing into freed memory at the end of the statement: for those the
// overload below is deleted. Accepted: an lvalue (a named object) of any byte range, and an rvalue of a view type,
// whose bytes live elsewhere: a type for which enable_borrowed_bytes is true (std::basic_string_view, std::span, and
// your own view types if you specialise it).
template <class Bytes, std::enable_if_t<detail::is_byte_range_v<Bytes> &&
                                            (std::is_lvalue_reference_v<Bytes> || detail::is_byte_view_v<Bytes>),
                                        int> = 0>
NewRecord new_record(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                     Bytes &&payload) noexcept {
    return new_record(schema_id, venue_ts_ns, local_ts_ns, std::data(payload), std::size(payload));
}
// Deleted: a temporary that owns its bytes (std::string, std::vector, std::array, ...) is destroyed at the end of the
// statement, leaving the record dangling. Name it, so that it outlives the append.
template <class Bytes, std::enable_if_t<detail::is_byte_range_v<Bytes> && !std::is_lvalue_reference_v<Bytes> &&
                                            !detail::is_byte_view_v<Bytes>,
                                        int> = 0>
NewRecord new_record(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                     Bytes &&payload) = delete;

// The checkpoint marker (docs/format.md -> Checkpoint marker).
struct Checkpoint {
    std::uint64_t generation = 0;
    std::uint64_t next_seq = 0;  // every record below this was durable when the marker was written
    std::uint64_t segment_base_seq = 0;
    std::uint64_t offset = 0;
};

// ---- Operational metrics (ADR 0032) ----

// How the newest segment ends, as a status sample saw it.
enum class HeadEnd : std::int32_t {
    open = TICKERTAPE_HEAD_OPEN,        // at a slot with no record yet: the normal state
    sealed = TICKERTAPE_HEAD_SEALED,    // at a seal without a successor: a writer died during a roll
    invalid = TICKERTAPE_HEAD_INVALID,  // at invalid bytes (StatusReport::Head::corrupt_reason): corruption
};

// Why the checkpoint marker cannot be used; anything but `usable` means a writer refuses the journal (corruption).
enum class CheckpointProblem : std::int32_t {
    usable = TICKERTAPE_CHECKPOINT_USABLE,
    missing_segment = TICKERTAPE_CHECKPOINT_MISSING_SEGMENT,
    invalid_segment_header = TICKERTAPE_CHECKPOINT_INVALID_SEGMENT_HEADER,
    invalid_offset = TICKERTAPE_CHECKPOINT_INVALID_OFFSET,
    other = TICKERTAPE_CHECKPOINT_OTHER_PROBLEM,
};

// Why the checkpoint's age could not be estimated from the marker's modification time.
enum class AgeUnknown : std::int32_t {
    known = TICKERTAPE_AGE_KNOWN,
    no_modification_time = TICKERTAPE_AGE_NO_MODIFICATION_TIME,
    in_future = TICKERTAPE_AGE_IN_FUTURE,
    before_segment = TICKERTAPE_AGE_BEFORE_SEGMENT,
    marker_resized = TICKERTAPE_AGE_MARKER_RESIZED,
    changing = TICKERTAPE_AGE_CHANGING,  // a writer checkpointing very often: not a stale checkpoint
    older_slot_lost = TICKERTAPE_AGE_OLDER_SLOT_LOST,
    other = TICKERTAPE_AGE_OTHER_REASON,
};

// Why a reader's lag is unknown.
enum class LagUnknown : std::int32_t {
    known = TICKERTAPE_LAG_KNOWN,
    segment_gone = TICKERTAPE_LAG_SEGMENT_GONE,              // retention or recovery removed the reader's segment
    other_history = TICKERTAPE_LAG_OTHER_HISTORY,            // the segment is another file of that name
    segment_unreadable = TICKERTAPE_LAG_SEGMENT_UNREADABLE,  // its header could not be read
    checkpoint_unusable = TICKERTAPE_LAG_CHECKPOINT_UNUSABLE,  // durable-only reader, unusable marker
    ahead_of_sample = TICKERTAPE_LAG_AHEAD_OF_SAMPLE,        // the reader is past the sample: take a new one
    other = TICKERTAPE_LAG_OTHER_REASON,
};

// Whether a reader beacon's owner is running, and whether the beacon could be read.
enum class BeaconState : std::int32_t {
    live = TICKERTAPE_BEACON_LIVE,
    stale = TICKERTAPE_BEACON_STALE,  // the owner died: its position is history, not lag
    uncertain = TICKERTAPE_BEACON_UNCERTAIN,
    invalid = TICKERTAPE_BEACON_INVALID,
    unreadable = TICKERTAPE_BEACON_UNREADABLE,
    changing = TICKERTAPE_BEACON_CHANGING,
};

// Whether the beacon directory could be listed.
enum class ReadersDirState : std::int32_t {
    listed = TICKERTAPE_READERS_DIR_LISTED,
    missing = TICKERTAPE_READERS_DIR_MISSING,
    unusable = TICKERTAPE_READERS_DIR_UNUSABLE,
};

// A position in the journal: the sequence number expected there, and where that is.
struct Position {
    std::uint64_t next_seq = 0;
    std::uint64_t segment_base_seq = 0;
    std::uint64_t offset = 0;
};

// How far a reader is behind what it could read now.
struct Lag {
    std::uint64_t records = 0;           // records it could return now but has not
    std::optional<std::uint64_t> bytes;  // segment-file footprint up to the target, if known
};

namespace detail {
inline std::optional<std::uint64_t> known(std::uint64_t value) noexcept {
    if (value == TICKERTAPE_UNKNOWN) return std::nullopt;
    return value;
}
inline std::optional<std::chrono::nanoseconds> known_ns(std::uint64_t value) noexcept {
    if (value == TICKERTAPE_UNKNOWN) return std::nullopt;
    constexpr std::uint64_t max = static_cast<std::uint64_t>(std::numeric_limits<std::int64_t>::max());
    return std::chrono::nanoseconds(static_cast<std::int64_t>(value < max ? value : max));
}
}  // namespace detail

// The values of one status sample (docs/operations.md; ADR 0032). Unknown values are empty optionals, never guesses.
struct StatusReport {
    struct Head {
        Position position;  // where the next record goes, as far as the newest segment shows
        std::uint64_t segment_size = 0;
        HeadEnd end = HeadEnd::open;
        CorruptReason corrupt_reason = CorruptReason::none;  // for HeadEnd::invalid
        std::optional<std::uint64_t> last_local_ts_ns;      // of the last record the scan passed
        bool verified = false;      // every record from the checkpoint to the head was validated
        bool chain_broken = false;  // the listing proves a segment missing: corruption
    };
    struct Segments {
        std::uint64_t count = 0;
        std::uint64_t first_seq = 0;
        std::uint64_t total_bytes = 0;
        std::optional<std::uint64_t> oldest_created_unix_ns;
        std::uint64_t before_checkpoint = 0;
    };
    struct CheckpointState {
        Checkpoint marker;
        CheckpointProblem problem = CheckpointProblem::usable;
        std::optional<std::uint64_t> modified_unix_ns;
        std::optional<std::chrono::nanoseconds> estimated_age;
        AgeUnknown age_unknown = AgeUnknown::known;
    };
    struct Exposure {
        std::optional<std::uint64_t> records;  // what a power loss now could remove
        std::optional<std::uint64_t> bytes;
        std::optional<std::chrono::nanoseconds> estimated_age;  // an upper bound
        std::optional<std::uint64_t> oldest_local_ts_ns;
        bool verified = false;
    };
    struct Storage {
        std::uint64_t available_bytes = 0;
        std::uint64_t total_bytes = 0;
        std::uint64_t next_segment_size = 0;
        std::uint64_t estimated_rolls_left = 0;  // optimistic on btrfs
        bool spare_segment = false;
    };
    std::uint64_t sampled_unix_ns = 0;
    Head head;
    Segments segments;
    std::optional<CheckpointState> checkpoint;  // none if there is no valid marker
    Exposure exposure;
    Storage storage;
    std::optional<std::uint64_t> writer_pid;  // the last writer's PID from writer.lock (informational)
    std::optional<bool> writer_alive;
    ReadersDirState readers_dir_state = ReadersDirState::missing;
};

// A reader in another process, from its beacon.
struct BeaconStatus {
    struct Report {
        std::uint64_t pid = 0;
        std::uint64_t next_seq = 0;  // the next record it returns
        Position slot;               // the slot it reads next
        bool seek_pending = false;
        bool durable_only = false;
        std::uint64_t updated_unix_ns = 0;
    };
    std::string name;
    BeaconState state = BeaconState::invalid;
    std::optional<Report> report;  // none for invalid, unreadable or changing beacons
    std::optional<std::chrono::nanoseconds> update_age;
    std::optional<Lag> lag;  // as of the beacon's last write
    LagUnknown lag_unknown = LagUnknown::known;
    std::optional<std::uint64_t> oldest_unread_local_ts_ns;
    std::optional<std::chrono::nanoseconds> unread_age;
};

// What only the writer knows (Writer::metrics).
struct WriterMetrics {
    Position head;
    std::uint64_t segment_size = 0;
    std::optional<Checkpoint> checkpoint;  // as this writer last wrote or read it
    std::optional<std::chrono::nanoseconds> since_checkpoint;  // monotonic; none before this writer's first
    std::uint64_t uncheckpointed_records = 0;
    std::uint64_t uncheckpointed_bytes = 0;
    bool poisoned = false;  // a checkpoint failed: later ones fail with Status::durability_lost
    std::uint64_t retention_failures = 0;
};

// A reader's cursor and its lag behind a status sample (Reader::metrics).
struct ReaderMetrics {
    std::uint64_t next_seq = 0;  // as Reader::position()
    Position slot;               // the slot it reads next
    bool seek_pending = false;
    bool durable_only = false;  // lag is measured against the checkpoint
    bool held_record = false;   // a record that did not fit a buffer is held for the next read
    std::optional<Lag> lag;
    LagUnknown lag_unknown = LagUnknown::known;
};

// One read-only sample of a journal's operational state: what `tickertape status` prints. Taking it needs no writer
// and disturbs none; its cost grows with what was appended since the last checkpoint (docs/c-api.md lists the files
// it reads). The values are read once, at sample(); the sample keeps no file open. Move-only; a moved-from sample is
// empty (Reader::metrics on it fails with Status::invalid_argument).
class JournalStatus {
public:
    JournalStatus() noexcept = default;
    JournalStatus(JournalStatus &&other) noexcept
        : h_(detail::take_handle(other.h_)), report_(std::move(other.report_)), beacons_(std::move(other.beacons_)) {}
    JournalStatus &operator=(JournalStatus &&other) noexcept {
        if (this != &other) {
            tickertape_journal_status_free(h_);
            h_ = detail::take_handle(other.h_);
            report_ = std::move(other.report_);
            beacons_ = std::move(other.beacons_);
        }
        return *this;
    }
    JournalStatus(const JournalStatus &) = delete;
    JournalStatus &operator=(const JournalStatus &) = delete;
    ~JournalStatus() { tickertape_journal_status_free(h_); }

    // Samples the journal in `dir`, reading reader beacons from `<dir>/readers` (or from `readers_dir`). Errors:
    // Status::not_found if it has no segments, corrupt, unsupported_version, io.
    static result<JournalStatus> sample(const detail::PathArg &dir) { return sample_impl(dir, nullptr); }
    static result<JournalStatus> sample(const detail::PathArg &dir, const detail::PathArg &readers_dir) {
        if (auto checked = readers_dir.check(); !checked) return detail::take_error(std::move(checked));
        return sample_impl(dir, &readers_dir);
    }

    bool is_open() const noexcept { return h_ != nullptr; }
    explicit operator bool() const noexcept { return is_open(); }

    const StatusReport &report() const noexcept { return report_; }
    // Reader beacons, sorted by name.
    const std::vector<BeaconStatus> &beacons() const noexcept { return beacons_; }

    // The C handle, for calling C functions the binding does not cover. Do not free it.
    const tickertape_journal_status *native_handle() const noexcept { return h_; }

private:
    static result<JournalStatus> sample_impl(const detail::PathArg &dir, const detail::PathArg *readers_dir) {
        if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
        if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
        JournalStatus out;
        const tickertape_status status = tickertape_journal_status_sample(
            dir.c_str(), readers_dir != nullptr ? readers_dir->c_str() : nullptr, &out.h_);
        if (status < 0) return Error::from_last_call(status);
        tickertape_journal_status_report raw{};
        raw.struct_size = sizeof raw;
        if (const tickertape_status s = tickertape_journal_status_get_report(out.h_, &raw); s < 0) {
            return Error::from_last_call(s);
        }
        StatusReport &r = out.report_;
        r.sampled_unix_ns = raw.sampled_unix_ns;
        r.head.position = Position{raw.head_next_seq, raw.head_segment_base_seq, raw.head_offset};
        r.head.segment_size = raw.head_segment_size;
        r.head.end = static_cast<HeadEnd>(raw.head_end);
        r.head.corrupt_reason = static_cast<CorruptReason>(raw.head_corrupt_reason);
        r.head.last_local_ts_ns = detail::known(raw.head_last_local_ts_ns);
        r.head.verified = raw.head_verified != 0;
        r.head.chain_broken = raw.head_chain_broken != 0;
        r.segments.count = raw.segment_count;
        r.segments.first_seq = raw.first_seq;
        r.segments.total_bytes = raw.segments_total_bytes;
        r.segments.oldest_created_unix_ns = detail::known(raw.oldest_segment_created_unix_ns);
        r.segments.before_checkpoint = raw.segments_before_checkpoint;
        if (raw.has_checkpoint != 0) {
            StatusReport::CheckpointState c;
            c.marker = Checkpoint{raw.checkpoint_generation, raw.checkpoint_next_seq, raw.checkpoint_segment_base_seq,
                                  raw.checkpoint_offset};
            c.problem = static_cast<CheckpointProblem>(raw.checkpoint_problem);
            c.modified_unix_ns = detail::known(raw.checkpoint_modified_unix_ns);
            c.estimated_age = detail::known_ns(raw.checkpoint_estimated_age_ns);
            c.age_unknown = static_cast<AgeUnknown>(raw.checkpoint_age_unknown);
            r.checkpoint = c;
        }
        r.exposure.records = detail::known(raw.exposure_records);
        r.exposure.bytes = detail::known(raw.exposure_bytes);
        r.exposure.estimated_age = detail::known_ns(raw.exposure_estimated_age_ns);
        r.exposure.oldest_local_ts_ns = detail::known(raw.exposure_oldest_local_ts_ns);
        r.exposure.verified = raw.exposure_verified != 0;
        r.storage.available_bytes = raw.storage_available_bytes;
        r.storage.total_bytes = raw.storage_total_bytes;
        r.storage.next_segment_size = raw.storage_next_segment_size;
        r.storage.estimated_rolls_left = raw.storage_estimated_rolls_left;
        r.storage.spare_segment = raw.storage_spare_segment != 0;
        r.writer_pid = detail::known(raw.writer_pid);
        if (raw.writer_alive != TICKERTAPE_UNKNOWN) r.writer_alive = raw.writer_alive != 0;
        r.readers_dir_state = static_cast<ReadersDirState>(raw.readers_dir_state);
        out.beacons_.reserve(static_cast<std::size_t>(raw.beacon_count));
        for (std::size_t i = 0; i < raw.beacon_count; i++) {
            tickertape_beacon_status b{};
            b.struct_size = sizeof b;
            if (const tickertape_status s = tickertape_journal_status_get_beacon(out.h_, i, &b); s < 0) {
                return Error::from_last_call(s);
            }
            BeaconStatus beacon;
            beacon.name = tickertape_journal_status_beacon_name(out.h_, i);
            beacon.state = static_cast<BeaconState>(b.state);
            if (b.has_report != 0) {
                beacon.report = BeaconStatus::Report{
                    b.report_pid,
                    b.report_next_seq,
                    Position{b.report_slot_seq, b.report_segment_base_seq, b.report_offset},
                    b.report_seek_pending != 0,
                    b.report_durable_only != 0,
                    b.report_updated_unix_ns};
            }
            beacon.update_age = detail::known_ns(b.update_age_ns);
            if (b.lag_records != TICKERTAPE_UNKNOWN) beacon.lag = Lag{b.lag_records, detail::known(b.lag_bytes)};
            beacon.lag_unknown = static_cast<LagUnknown>(b.lag_unknown);
            beacon.oldest_unread_local_ts_ns = detail::known(b.oldest_unread_local_ts_ns);
            beacon.unread_age = detail::known_ns(b.unread_age_ns);
            out.beacons_.push_back(std::move(beacon));
        }
        return out;
    }

    tickertape_journal_status *h_ = nullptr;
    StatusReport report_;
    std::vector<BeaconStatus> beacons_;
};

class Reader;
class RecordBatch;

// A validated record. Its payload is **borrowed**:
// - from Reader::next: it points into the reader's buffer (or, for a mapped reader, into the mapped segment) and
//   is valid until the next call on that reader other than position() and metrics(), or until the reader is moved
//   or destroyed;
// - from a RecordBatch: it points into the batch's buffer and is valid until the next read into that batch, or
//   until the batch is moved or destroyed;
// - from Reader::next_copy: it points into your buffer and is valid as long as that is.
// Copy what you need to keep. seq(), schema_id(), the timestamps, size() and empty() are copies and stay valid;
// data(), as_string_view() and as_span() read the payload and are checked: in debug builds (TICKERTAPE_DEBUG_CHECKS)
// fetching the payload through a view after its reader or batch moved on aborts with a message. The check runs when
// the payload is fetched through the view: a pointer, string_view or span taken from it earlier has no continuing
// check. A view whose reader or batch was destroyed is not detected.
class RecordView {
public:
    std::uint64_t seq() const noexcept { return rec_.seq; }
    std::uint32_t schema_id() const noexcept { return rec_.schema_id; }
    std::uint64_t venue_ts_ns() const noexcept { return rec_.venue_ts_ns; }
    std::uint64_t local_ts_ns() const noexcept { return rec_.local_ts_ns; }
    // The payload bytes: no alignment is guaranteed, and the pointer is null when size() is 0.
    TICKERTAPE_HPP_MODE_DEPENDENT const std::uint8_t *data() const noexcept {
        check_live();
        return rec_.payload;
    }
    std::size_t size() const noexcept { return rec_.payload_len; }
    bool empty() const noexcept { return rec_.payload_len == 0; }
    // The payload as characters.
    TICKERTAPE_HPP_MODE_DEPENDENT std::string_view as_string_view() const noexcept {
        check_live();
        return rec_.payload_len == 0
                   ? std::string_view()
                   : std::string_view(reinterpret_cast<const char *>(rec_.payload), rec_.payload_len);
    }

private:
    friend class Reader;
    friend class RecordBatch;
    RecordView(const tickertape_record &rec, const std::uint64_t *generation) noexcept
        : rec_(rec), generation_(generation), snapshot_(generation ? *generation : 0) {}
    TICKERTAPE_HPP_MODE_DEPENDENT void check_live() const noexcept {
#if TICKERTAPE_DEBUG_CHECKS
        if (generation_ != nullptr && *generation_ != snapshot_) {
            detail::fatal("a RecordView's payload was used after the next call on its Reader or RecordBatch (or "
                          "after it was moved); copy the payload if you need it longer");
        }
#endif
    }
    tickertape_record rec_;
    // Read only by the debug check, but present in every build so that the layout is the same in every mode.
    [[maybe_unused]] const std::uint64_t *generation_;
    [[maybe_unused]] std::uint64_t snapshot_;
};

#if __cplusplus >= 202002L && defined(__cpp_lib_span) && __cpp_lib_span >= 202002L
// C++20: the payload as a span (same lifetime as the view).
TICKERTAPE_HPP_MODE_DEPENDENT inline std::span<const std::uint8_t> as_span(const RecordView &record) noexcept {
    const std::uint8_t *data = record.data();  // first, so that the debug check also covers an empty payload
    if (record.empty()) return std::span<const std::uint8_t>();
    return std::span<const std::uint8_t>(data, record.size());
}
#endif

// Storage for Reader::read_batch: room for `max_records` records whose payloads are copied one after another into
// a buffer of `buffer_bytes`. Both are allocated once, here (C++ allocations it owns, not a C handle); reading into a
// batch never allocates. There is no default constructor. Move-only; a moved-from batch has capacity 0.
class RecordBatch {
public:
    class iterator {
    public:
        using iterator_category = std::input_iterator_tag;
        using value_type = RecordView;
        using difference_type = std::ptrdiff_t;
        using pointer = void;
        using reference = RecordView;

        iterator() noexcept = default;
        TICKERTAPE_HPP_MODE_DEPENDENT RecordView operator*() const noexcept { return (*batch_)[index_]; }
        iterator &operator++() noexcept {
            ++index_;
            return *this;
        }
        iterator operator++(int) noexcept {
            iterator old = *this;
            ++index_;
            return old;
        }
        friend bool operator==(const iterator &a, const iterator &b) noexcept {
            return a.batch_ == b.batch_ && a.index_ == b.index_;
        }
        friend bool operator!=(const iterator &a, const iterator &b) noexcept { return !(a == b); }

    private:
        friend class RecordBatch;
        iterator(const RecordBatch *batch, std::size_t index) noexcept : batch_(batch), index_(index) {}
        const RecordBatch *batch_ = nullptr;
        std::size_t index_ = 0;
    };

    RecordBatch(std::size_t max_records, std::size_t buffer_bytes)
        : records_(max_records),
          buffer_(buffer_bytes),
          capacity_(max_records),
          buffer_size_(buffer_bytes) {}
    RecordBatch(RecordBatch &&other) noexcept
        : records_(std::move(other.records_)),
          buffer_(std::move(other.buffer_)),
          capacity_(std::exchange(other.capacity_, 0)),
          buffer_size_(std::exchange(other.buffer_size_, 0)),
          count_(std::exchange(other.count_, 0)) {
        other.generation_++;
    }
    RecordBatch &operator=(RecordBatch &&other) noexcept {
        if (this != &other) {
            records_ = std::move(other.records_);
            buffer_ = std::move(other.buffer_);
            capacity_ = std::exchange(other.capacity_, 0);
            buffer_size_ = std::exchange(other.buffer_size_, 0);
            count_ = std::exchange(other.count_, 0);
            generation_++;
            other.generation_++;
        }
        return *this;
    }
    RecordBatch(const RecordBatch &) = delete;
    RecordBatch &operator=(const RecordBatch &) = delete;
    ~RecordBatch() = default;

    // The records read by the last read_batch into this batch.
    std::size_t size() const noexcept { return count_; }
    bool empty() const noexcept { return count_ == 0; }
    std::size_t capacity() const noexcept { return capacity_; }
    std::size_t buffer_size() const noexcept { return buffer_size_; }
    // Record `index` (< size(); checked in debug builds).
    TICKERTAPE_HPP_MODE_DEPENDENT RecordView operator[](std::size_t index) const noexcept {
        detail::debug_check(index < count_, "RecordBatch index out of range");
        return RecordView(records_[index], &generation_);
    }
    iterator begin() const noexcept { return iterator(this, 0); }
    iterator end() const noexcept { return iterator(this, count_); }

private:
    friend class Reader;
    detail::Array<tickertape_record> records_;
    detail::Array<std::uint8_t> buffer_;
    std::size_t capacity_;
    std::size_t buffer_size_;
    std::size_t count_ = 0;
    std::uint64_t generation_ = 0;
};

// ---- Writer ----

// What Writer::append_batch did: `appended` records were committed, with sequence numbers first_seq,
// first_seq + 1, ...; `outcome` is the error that stopped it at records[appended], if any. The committed records
// stay in the journal: retry from the failing record, not from the start.
struct [[nodiscard]] BatchAppend {
    std::uint64_t first_seq = 0;
    std::size_t appended = 0;
    result<void> outcome;
};

class Writer;

namespace detail {
// The state of a Writer::append_with call, on its stack. The Writer that owns the handle points to it while the
// fill runs, so that destroying, moving or move-assigning that Writer from inside the fill is defined.
struct FillFrame {
    tickertape_writer *handle = nullptr;
    Writer *owner = nullptr;       // the Writer that owns `handle` now (updated when that Writer is moved)
    bool close_on_return = false;  // the owner was destroyed or reassigned: close `handle` after the call
    std::exception_ptr exception;  // what the fill threw
};

struct CloseOnExit {
    explicit CloseOnExit(tickertape_writer *writer) noexcept : handle(writer) {}
    CloseOnExit(const CloseOnExit &) = delete;
    CloseOnExit &operator=(const CloseOnExit &) = delete;
    ~CloseOnExit() {
        if (handle != nullptr) (void)tickertape_writer_close(handle);
    }
    tickertape_writer *handle;
};
}  // namespace detail

// The journal's single writer (one per journal, across processes). Move-only; the destructor closes the writer
// without checkpointing: appended records stay visible and survive the process, but only checkpointed ones are
// durable.
class Writer {
public:
    // An empty writer; calls on it fail with Status::invalid_argument.
    Writer() noexcept = default;
    Writer(Writer &&other) noexcept
        : h_(detail::take_handle(other.h_)), frame_(detail::take_handle(other.frame_)) {
        if (frame_ != nullptr) frame_->owner = this;
    }
    Writer &operator=(Writer &&other) noexcept {
        if (this != &other) {
            release();
            h_ = detail::take_handle(other.h_);
            frame_ = detail::take_handle(other.frame_);
            if (frame_ != nullptr) frame_->owner = this;
        }
        return *this;
    }
    Writer(const Writer &) = delete;
    Writer &operator=(const Writer &) = delete;
    ~Writer() { release(); }

    // Opens the journal in `dir` for writing, creating it if needed; takes the journal's lock and recovers it
    // (docs/format.md -> Recovery). Errors: Status::locked (another writer has it open), invalid_config, corrupt,
    // unsupported_version, io; unsupported_version also if the loaded library does not match this header.
    static result<Writer> open(const detail::PathArg &dir) { return open_impl(dir, nullptr); }
    static result<Writer> open(const detail::PathArg &dir, const WriterOptions &options) {
        if (auto checked = detail::check_options(options); !checked) {
            return detail::take_error(std::move(checked));
        }
        return open_impl(dir, options.native_handle());
    }

    bool is_open() const noexcept { return h_ != nullptr; }
    explicit operator bool() const noexcept { return is_open(); }

    // Appends one record and returns its sequence number. On success it is **visible** (readers can read it, and it
    // survives this process dying), not **durable** until a later successful checkpoint(). Errors:
    // Status::record_too_large, sequence_exhausted, storage_full or io (a failed roll: the journal is unchanged and
    // the call can be retried), io for every append after a failed clean-up of an aborted fill (see append_with).
    result<std::uint64_t> append(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                                 const void *data, std::size_t size) {
        std::uint64_t seq = 0;
        const tickertape_status status = tickertape_writer_append(
            h_, schema_id, venue_ts_ns, local_ts_ns, static_cast<const std::uint8_t *>(data), size, &seq);
        if (status < 0) return Error::from_last_call(status);
        return seq;
    }
    // The same, with a contiguous range of bytes or chars as the payload (std::string_view, std::vector, ...).
    template <class Bytes, std::enable_if_t<detail::is_byte_range_v<Bytes>, int> = 0>
    result<std::uint64_t> append(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                                 const Bytes &payload) {
        return append(schema_id, venue_ts_ns, local_ts_ns, std::data(payload), std::size(payload));
    }

    // Appends one record whose payload `fill` writes in place, without a copy: `fill(std::uint8_t *buf,
    // std::size_t len)` is called once with `size` zeroed bytes inside the mapped segment (buf is null when size is
    // 0), on this thread, before append_with returns. It returns void to commit, or bool (false aborts). It must not
    // keep `buf`. Durability as for append().
    //
    // Aborting (fill returns false or throws) commits nothing, but the writer zeroes the bytes and syncs that, so it
    // is slow. A clean abort gives Status::aborted, or rethrows what fill threw. If that sync fails, the writer
    // refuses every later append and checkpoint (a storage incident, docs/c-api.md -> A failed clean-up): the call
    // then returns Status::io instead, or, if fill threw, throws Exception with that error and fill's exception
    // nested inside it (std::rethrow_if_nested).
    //
    // Calling this writer from inside fill fails with Status::busy (the C ABI refuses re-entry). Destroying,
    // moving or move-assigning the writer from inside fill is allowed: the append still completes (or aborts), and
    // if the writer was destroyed or reassigned its handle is closed when append_with returns.
    template <class Fill>
    result<std::uint64_t> append_with(std::uint32_t schema_id, std::uint64_t venue_ts_ns, std::uint64_t local_ts_ns,
                                      std::size_t size, Fill &&fill) {
        using F = std::remove_reference_t<Fill>;
        static_assert(std::is_invocable_v<F &, std::uint8_t *, std::size_t>,
                      "fill must be callable as fill(std::uint8_t *buf, std::size_t len)");
        using R = std::invoke_result_t<F &, std::uint8_t *, std::size_t>;
        static_assert(std::is_void_v<R> || std::is_same_v<R, bool>, "fill must return void or bool");
        return append_with_impl(this, schema_id, venue_ts_ns, local_ts_ns, size, fill);
    }

    // Appends `count` records in order, stopping at the first failure; see BatchAppend. Every record is checked first,
    // so an argument error appends nothing. Durability as for append().
    BatchAppend append_batch(const NewRecord *records, std::size_t count) {
        BatchAppend out;
        const tickertape_status status =
            tickertape_writer_append_batch(h_, records, count, &out.appended, &out.first_seq);
        if (status < 0) out.outcome = Error::from_last_call(status);
        return out;
    }
    // The same for a contiguous range of NewRecord (std::vector, std::array, std::span, ...), passed as it is.
    template <class Records,
              std::enable_if_t<std::is_same_v<std::remove_cv_t<std::remove_pointer_t<decltype(std::data(
                                                  std::declval<const Records &>()))>>,
                                              NewRecord>,
                               int> = 0>
    BatchAppend append_batch(const Records &records) {
        return append_batch(std::data(records), std::size(records));
    }

    // Makes every record appended so far **durable** (blocks until the syncs finish), then runs configured
    // retention. Errors: Status::io or storage_full if a sync fails; the writer is then poisoned and every later
    // checkpoint returns durability_lost (appends keep working). Treat a checkpoint error as an incident.
    result<void> checkpoint() { return detail::status_to_result(tickertape_writer_checkpoint(h_)); }

    // The sequence number the next append will get.
    result<std::uint64_t> next_seq() const {
        std::uint64_t seq = 0;
        const tickertape_status status = tickertape_writer_next_seq(h_, &seq);
        if (status < 0) return Error::from_last_call(status);
        return seq;
    }

    // What only the writer knows: its head, the last checkpoint by the monotonic clock, uncheckpointed records and
    // bytes, whether it is poisoned, failed retention runs. No system calls.
    result<WriterMetrics> metrics() const {
        tickertape_writer_metrics raw{};
        raw.struct_size = sizeof raw;
        const tickertape_status status = tickertape_writer_get_metrics(h_, &raw);
        if (status < 0) return Error::from_last_call(status);
        WriterMetrics m;
        m.head = Position{raw.head_next_seq, raw.head_segment_base_seq, raw.head_offset};
        m.segment_size = raw.segment_size;
        if (raw.has_checkpoint != 0) {
            m.checkpoint = Checkpoint{raw.checkpoint_generation, raw.checkpoint_next_seq,
                                      raw.checkpoint_segment_base_seq, raw.checkpoint_offset};
        }
        m.since_checkpoint = detail::known_ns(raw.since_checkpoint_ns);
        m.uncheckpointed_records = raw.uncheckpointed_records;
        m.uncheckpointed_bytes = raw.uncheckpointed_bytes;
        m.poisoned = raw.poisoned != 0;
        m.retention_failures = raw.retention_failures;
        return m;
    }

    // The most recent retention error since the writer was opened, if any (retention never fails the writer).
    std::optional<Error> last_retention_error() const {
        const tickertape_status status = tickertape_writer_last_retention_error(h_);
        if (status < 0) return Error::from_last_call(status);
        return std::nullopt;
    }

    // Closes the writer now (without checkpointing) and reports the close's status; the writer is empty afterwards.
    // From inside this writer's fill it fails with Status::busy and closes nothing.
    result<void> close() {
        if (h_ == nullptr) return {};
        const tickertape_status status = tickertape_writer_close(h_);
        if (status == TICKERTAPE_E_BUSY) return Error::from_last_call(status);
        h_ = nullptr;
        return detail::status_to_result(status);
    }

    // The C handle, for calling C functions the binding does not cover. Do not close it.
    tickertape_writer *native_handle() const noexcept { return h_; }

private:
    explicit Writer(tickertape_writer *handle) noexcept : h_(handle) {}

    static result<Writer> open_impl(const detail::PathArg &dir, const tickertape_writer_options *options) {
        if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
        if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
        tickertape_writer *handle = nullptr;
        const tickertape_status status = tickertape_writer_open(dir.c_str(), options, &handle);
        if (status < 0) return Error::from_last_call(status);
        return Writer(handle);
    }

    // Closes the handle, or, inside its own fill, hands it to the fill's frame to close after the call.
    void release() noexcept {
        if (frame_ != nullptr) {
            frame_->close_on_return = true;
            frame_ = nullptr;
        } else if (h_ != nullptr) {
            (void)tickertape_writer_close(h_);
        }
        h_ = nullptr;
    }

    template <class F>
    struct FillContext : detail::FillFrame {
        F *fill = nullptr;
    };

    // Never lets an exception reach the C ABI: catches it, aborts the append, and append_with rethrows it.
    template <class F>
    static std::int32_t trampoline(void *user_data, std::uint8_t *buf, std::size_t len) noexcept {
        FillContext<F> *ctx = static_cast<FillContext<F> *>(user_data);
        using R = std::invoke_result_t<F &, std::uint8_t *, std::size_t>;
#if TICKERTAPE_HPP_EXCEPTIONS
        try {
#endif
            if constexpr (std::is_void_v<R>) {
                (*ctx->fill)(buf, len);
                return 0;
            } else {
                return (*ctx->fill)(buf, len) ? 0 : 1;
            }
#if TICKERTAPE_HPP_EXCEPTIONS
        } catch (...) {
            ctx->exception = std::current_exception();
            return 1;
        }
#endif
    }

    static std::int32_t refuse(void *, std::uint8_t *, std::size_t) noexcept { return 1; }

    // Static, and after the C call it touches only the frame on its own stack: `self` may have been destroyed or
    // moved from by then.
    template <class F>
    static result<std::uint64_t> append_with_impl(Writer *self, std::uint32_t schema_id, std::uint64_t venue_ts_ns,
                                                  std::uint64_t local_ts_ns, std::size_t size, F &fill) {
        std::uint64_t seq = 0;
        if (self->frame_ != nullptr) {
            // Re-entry from this writer's own fill: the C ABI refuses it with TICKERTAPE_E_BUSY before calling the
            // callback. Keep the outer frame installed.
            const tickertape_status status = tickertape_writer_append_with(self->h_, schema_id, venue_ts_ns,
                                                                           local_ts_ns, size, &refuse, nullptr, &seq);
            if (status < 0) return Error::from_last_call(status);
            return seq;
        }
        FillContext<F> ctx;
        ctx.handle = self->h_;
        ctx.owner = self;
        ctx.fill = &fill;
        self->frame_ = &ctx;
        const tickertape_status status = tickertape_writer_append_with(ctx.handle, schema_id, venue_ts_ns,
                                                                       local_ts_ns, size, &trampoline<F>, &ctx, &seq);
        // Detach the frame before anything that can throw. A handle whose owner went away is closed when this
        // function returns or unwinds, after the error has been read.
        const detail::CloseOnExit deferred_close(ctx.close_on_return ? ctx.handle : nullptr);
        if (!ctx.close_on_return) ctx.owner->frame_ = nullptr;
#if TICKERTAPE_HPP_EXCEPTIONS
        if (ctx.exception) {
            // A clean abort: rethrow the fill's exception before anything that allocates, so that a failed
            // allocation cannot replace it.
            if (status >= 0 || status == TICKERTAPE_E_ABORTED) std::rethrow_exception(ctx.exception);
            // The clean-up failed: report the library error, which says the writer has stopped, with the fill's
            // exception nested as its cause.
            Error error = Error::from_last_call(status);
            try {
                std::rethrow_exception(ctx.exception);
            } catch (...) {
                std::throw_with_nested(Exception(std::move(error)));
            }
        }
#endif
        if (status < 0) return Error::from_last_call(status);
        return seq;
    }

    tickertape_writer *h_ = nullptr;
    detail::FillFrame *frame_ = nullptr;
};

// ---- Reader ----

// A journal reader. Readers take no locks and never slow the writer down; any number can run, in any process.
// Every record is validated (length, CRC32C, sequence number) before it is returned. Move-only.
class Reader {
public:
    // An empty reader; calls on it fail with Status::invalid_argument.
    Reader() noexcept = default;
    Reader(Reader &&other) noexcept : h_(detail::take_handle(other.h_)) { other.generation_++; }
    Reader &operator=(Reader &&other) noexcept {
        if (this != &other) {
            (void)tickertape_reader_close(h_);
            h_ = detail::take_handle(other.h_);
            generation_++;
            other.generation_++;
        }
        return *this;
    }
    Reader(const Reader &) = delete;
    Reader &operator=(const Reader &) = delete;
    ~Reader() { (void)tickertape_reader_close(h_); }

    // Opens the **safe** reader on the journal in `dir`, at the oldest record. It reads with pread into its own
    // buffer, so it is memory-safe whatever other processes do to the files. Errors: Status::io with os_code()
    // ENOENT if `dir` does not exist (the writer has not started) and not_found if it has no segment yet (the
    // writer is creating it): a loop waiting for the writer retries on exactly these two. corrupt,
    // unsupported_version, io; unsupported_version also if the loaded library does not match this header.
    static result<Reader> open(const detail::PathArg &dir) { return open_impl(dir, nullptr, false); }
    static result<Reader> open(const detail::PathArg &dir, const ReaderOptions &options) {
        if (auto checked = detail::check_options(options); !checked) {
            return detail::take_error(std::move(checked));
        }
        return open_impl(dir, options.native_handle(), false);
    }

    // Opens the zero-copy **mapped** reader: payloads point straight into the memory-mapped segments.
    // PRECONDITION: while the reader exists, no process other than a tickertape writer may modify the journal's
    // segment files (no writing into, truncating or replacing their contents). A tickertape writer never changes
    // committed bytes, so this holds in normal operation. If it is broken, payloads can change under you, and a
    // truncated file kills the process with SIGBUS. Use open() when you cannot rule this out.
    static result<Reader> open_mapped_unsafe(const detail::PathArg &dir) { return open_impl(dir, nullptr, true); }
    static result<Reader> open_mapped_unsafe(const detail::PathArg &dir, const ReaderOptions &options) {
        if (auto checked = detail::check_options(options); !checked) {
            return detail::take_error(std::move(checked));
        }
        return open_impl(dir, options.native_handle(), true);
    }

    bool is_open() const noexcept { return h_ != nullptr; }
    explicit operator bool() const noexcept { return is_open(); }

    // Positions the reader so that the next record returned is `seq` (waiting for it if it is not written yet).
    // A successful seek discards a record held back and an error deferred by next_copy or read_batch; a failed
    // one changes nothing. Errors: Status::seq_unavailable (retention removed it: seek to list_segments()'s first
    // base_seq to continue), not_found, corrupt, io.
    result<void> seek(std::uint64_t seq) {
        generation_++;
        return detail::status_to_result(tickertape_reader_seek(h_, seq));
    }

    // The next record as a borrowed view, or nullopt if none arrived within `timeout` (see Timeout). Without
    // durable-only it returns **visible** records, including ones a power loss can still erase; with it, only
    // **durable** ones. Errors: Status::corrupt (the cursor does not move: calling again retries), discarded
    // (recovery or retention replaced the segment: seek() to continue), io, or an error deferred by read_batch.
    // Test the result first (did the call fail?), then the optional (did a record arrive?): `if (auto r = next())`
    // is true when no record arrived, and `(*r)->data()` on an empty optional is undefined behaviour.
    result<std::optional<RecordView>> next(Timeout timeout = no_wait) {
        generation_++;
        tickertape_record rec{};
        const tickertape_status status = tickertape_reader_next(h_, timeout.c_value(), &rec);
        if (status < 0) return Error::from_last_call(status);
        if (status == TICKERTAPE_NONE) return std::optional<RecordView>();
        return std::optional<RecordView>(RecordView(rec, &generation_));
    }

    // Like next(), but copies the payload into `buf` (`size` bytes); the view points into `buf`. If it does not
    // fit: Status::buffer_too_small with Error::needed_size(), and the record is returned by the next read.
    result<std::optional<RecordView>> next_copy(void *buf, std::size_t size, Timeout timeout = no_wait) {
        generation_++;
        tickertape_record rec{};
        const tickertape_status status =
            tickertape_reader_next_copy(h_, timeout.c_value(), static_cast<std::uint8_t *>(buf), size, &rec);
        if (status == TICKERTAPE_E_BUFFER_TOO_SMALL) return Error::from_last_call(status, rec.payload_len);
        if (status < 0) return Error::from_last_call(status);
        if (status == TICKERTAPE_NONE) return std::optional<RecordView>();
        return std::optional<RecordView>(RecordView(rec, nullptr));
    }
    // The same into a writable contiguous range of bytes or chars (std::array, std::vector, a C array, ...).
    template <class Bytes, std::enable_if_t<detail::is_writable_byte_range_v<Bytes>, int> = 0>
    result<std::optional<RecordView>> next_copy(Bytes &buffer, Timeout timeout = no_wait) {
        return next_copy(std::data(buffer), std::size(buffer), timeout);
    }

    // Reads up to batch.capacity() records into `batch` and returns how many (0 if none arrived within `timeout`).
    // Waits only for the first record, then takes what is already committed. A record that does not fit in the rest
    // of the buffer is held back for the next read; if even the first does not fit: Status::buffer_too_small with
    // Error::needed_size(). An error after some records were read is returned by the next read instead, so records
    // handed out are never lost to an error.
    result<std::size_t> read_batch(RecordBatch &batch, Timeout timeout = no_wait) {
        generation_++;
        batch.generation_++;
        batch.count_ = 0;
        std::size_t count = 0;
        const tickertape_status status =
            tickertape_reader_read_batch(h_, timeout.c_value(), batch.records_.get(), batch.capacity_,
                                         batch.buffer_.get(), batch.buffer_size_, &count);
        if (status == TICKERTAPE_E_BUFFER_TOO_SMALL) {
            return Error::from_last_call(status, batch.capacity_ ? batch.records_[0].payload_len : 0);
        }
        if (status < 0) return Error::from_last_call(status);
        batch.count_ = count;
        return count;
    }

    // The sequence number of the next record this reader will return. Does not invalidate a view.
    result<std::uint64_t> position() const {
        std::uint64_t seq = 0;
        const tickertape_status status = tickertape_reader_position(h_, &seq);
        if (status < 0) return Error::from_last_call(status);
        return seq;
    }

    // This reader's cursor and its lag behind `status` (behind the head, or the checkpoint for a durable-only
    // reader), as of that sample: take the sample after the reads you want counted. No system calls; does not
    // invalidate a view. Errors: Status::invalid_argument for an empty reader or sample.
    result<ReaderMetrics> metrics(const JournalStatus &status) const {
        tickertape_reader_metrics raw{};
        raw.struct_size = sizeof raw;
        const tickertape_status code = tickertape_reader_get_metrics(h_, status.native_handle(), &raw);
        if (code < 0) return Error::from_last_call(code);
        ReaderMetrics m;
        m.next_seq = raw.next_seq;
        m.slot = Position{raw.slot_seq, raw.segment_base_seq, raw.offset};
        m.seek_pending = raw.seek_pending != 0;
        m.durable_only = raw.durable_only != 0;
        m.held_record = raw.held_record != 0;
        if (raw.lag_records != TICKERTAPE_UNKNOWN) m.lag = Lag{raw.lag_records, detail::known(raw.lag_bytes)};
        m.lag_unknown = static_cast<LagUnknown>(raw.lag_unknown);
        return m;
    }

    // Closes the reader now and reports the close's status; the reader is empty afterwards.
    result<void> close() {
        generation_++;
        const tickertape_status status = tickertape_reader_close(detail::take_handle(h_));
        return detail::status_to_result(status);
    }

    // The C handle, for calling C functions the binding does not cover. Do not close it.
    tickertape_reader *native_handle() const noexcept { return h_; }

private:
    explicit Reader(tickertape_reader *handle) noexcept : h_(handle) {}

    static result<Reader> open_impl(const detail::PathArg &dir, const tickertape_reader_options *options, bool mapped) {
        if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
        if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
        tickertape_reader *handle = nullptr;
        const tickertape_status status = mapped ? tickertape_reader_open_mapped_unsafe(dir.c_str(), options, &handle)
                                                : tickertape_reader_open(dir.c_str(), options, &handle);
        if (status < 0) return Error::from_last_call(status);
        return Reader(handle);
    }

    tickertape_reader *h_ = nullptr;
    std::uint64_t generation_ = 0;  // bumped by every call that invalidates views, and by moves
};

// ---- Inspection and retention ----

// What verify() found.
struct VerifyReport {
    struct Problem {
        std::uint64_t segment_base_seq;  // from the file name; UINT64_MAX if the name does not parse
        std::uint64_t offset;            // byte offset in that segment
        CorruptReason reason;
    };
    std::uint64_t segments = 0;
    std::uint64_t records = 0;
    std::optional<std::uint64_t> first_seq;  // none if there is no valid record
    std::optional<std::uint64_t> last_seq;
    std::uint64_t problems = 0;  // 0: the journal is intact
    std::optional<Problem> first_problem;
};

// One segment file (<dir>/<base_seq as 20 digits>.tape).
struct SegmentInfo {
    std::uint64_t base_seq = 0;
    std::uint64_t segment_size = 0;
    std::uint64_t created_unix_ns = 0;  // from the segment header, by the writer's wall clock
    std::uint32_t format_version = 0;
};

// What apply_retention() removed, or plan_retention() would remove.
struct RetentionReport {
    std::uint64_t removed_segments = 0;
    std::uint64_t removed_bytes = 0;
    std::uint64_t already_removed_segments = 0;  // gone before this call removed them (always 0 when planning)
};

// Checks every segment of the journal in `dir` without taking a lock or changing anything (safe while a writer is
// active). Corruption is reported in the report, not as an error. Errors: Status::not_found, io.
inline result<VerifyReport> verify(const detail::PathArg &dir) {
    if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
    if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
    tickertape_verify_report raw{};
    raw.struct_size = sizeof raw;
    const tickertape_status status = tickertape_verify(dir.c_str(), &raw);
    if (status < 0) return Error::from_last_call(status);
    VerifyReport report;
    report.segments = raw.segments;
    report.records = raw.records;
    if (raw.records > 0) {
        report.first_seq = raw.first_seq;
        report.last_seq = raw.last_seq;
    }
    report.problems = raw.problems;
    if (raw.problems > 0) {
        report.first_problem = VerifyReport::Problem{raw.first_problem_base_seq, raw.first_problem_offset,
                                                     static_cast<CorruptReason>(raw.first_problem_reason)};
    }
    return report;
}

// The segments of the journal in `dir`, oldest first (allocates the vector). Errors: Status::not_found if there are
// none, corrupt or unsupported_version for an invalid header, io.
inline result<std::vector<SegmentInfo>> list_segments(const detail::PathArg &dir) {
    if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
    if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
    // The C structs go in the binding's own buffer, not a std::vector<tickertape_segment_info> (see detail::Array).
    detail::Array<tickertape_segment_info> raw;
    std::size_t count = 0;
    // The list can grow between the two calls while a writer is active; retry a few times.
    for (int attempt = 0;; attempt++) {
        tickertape_status status = tickertape_list_segments(dir.c_str(), nullptr, 0, &count);
        if (status < 0) return Error::from_last_call(status);
        const std::size_t room = count + 4;
        raw = detail::Array<tickertape_segment_info>(room);
        status = tickertape_list_segments(dir.c_str(), raw.get(), room, &count);
        if (status == TICKERTAPE_E_BUFFER_TOO_SMALL && attempt < 8) continue;
        if (status < 0) return Error::from_last_call(status);
        break;
    }
    std::vector<SegmentInfo> segments;
    segments.reserve(count);
    for (std::size_t i = 0; i < count; i++) {
        const tickertape_segment_info &s = raw[i];
        segments.push_back(SegmentInfo{s.base_seq, s.segment_size, s.created_unix_ns, s.format_version});
    }
    return segments;
}

// The checkpoint marker of the journal in `dir`, or nullopt if it is missing or neither slot is valid. Takes no
// lock; it observes the marker and does not validate the position. Errors: Status::io.
inline result<std::optional<Checkpoint>> read_checkpoint(const detail::PathArg &dir) {
    if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
    if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
    tickertape_checkpoint raw{};
    const tickertape_status status = tickertape_read_checkpoint(dir.c_str(), &raw);
    if (status < 0) return Error::from_last_call(status);
    if (status == TICKERTAPE_NONE) return std::optional<Checkpoint>();
    return std::optional<Checkpoint>(Checkpoint{raw.generation, raw.next_seq, raw.segment_base_seq, raw.offset});
}

namespace detail {
inline result<RetentionReport> retention(const detail::PathArg &dir, const Retention &settings, bool dry_run) {
    if (auto compatible = check_library(); !compatible) return detail::take_error(std::move(compatible));
    if (auto checked = dir.check(); !checked) return detail::take_error(std::move(checked));
    if (auto checked = check_options(settings); !checked) return detail::take_error(std::move(checked));
    tickertape_retention_report raw{};
    raw.struct_size = sizeof raw;
    const tickertape_status status =
        tickertape_retention_apply(dir.c_str(), settings.native_handle(), dry_run ? 1 : 0, &raw);
    if (status < 0) return Error::from_last_call(status);
    return RetentionReport{raw.removed_segments, raw.removed_bytes, raw.already_removed_segments};
}
}  // namespace detail

// Applies retention to the journal in `dir` once (safe while a writer is active). Stops at the first error;
// removals before it stay in effect. Errors: Status::invalid_argument if `settings` has no action, not_found,
// corrupt, io (including an existing destination file).
inline result<RetentionReport> apply_retention(const detail::PathArg &dir, const Retention &settings) {
    return detail::retention(dir, settings, false);
}

// What apply_retention() would remove, without changing anything (no action needed).
inline result<RetentionReport> plan_retention(const detail::PathArg &dir, const Retention &settings) {
    return detail::retention(dir, settings, true);
}

}  // inline namespace v1 or v1_noexcept
}  // namespace tickertape

// The header's internal macros (the public ones are TICKERTAPE_HPP_VERSION_*, TICKERTAPE_HPP_ABI_VERSION and
// TICKERTAPE_DEBUG_CHECKS).
#undef TICKERTAPE_HPP_MODE_DEPENDENT
#undef TICKERTAPE_HPP_DEBUG_CHECKS_TAG_
#undef TICKERTAPE_HPP_EXCEPTIONS

#endif  // TICKERTAPE_HPP
