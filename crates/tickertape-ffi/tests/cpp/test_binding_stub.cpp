// Tests of the C++ binding (include/tickertape.hpp) against stubbed C functions (no library is linked), compiled by
// tests/c_api.rs as C++17. It covers what the real library cannot be made to do on demand:
//
// - a fill that throws, followed by a failed clean-up (TICKERTAPE_E_IO instead of TICKERTAPE_E_ABORTED): the binding
//   must throw the library error with the fill's exception nested inside it, so that the caller learns both that
//   the writer has stopped and why the fill failed;
// - a fill that returns false, followed by a failed clean-up: a plain Status::io error;
// - every status code and every corruption reason, with the OS error, reaching an Error unchanged (including a code
//   from a newer library that this header does not name);
// - a library whose version does not match the header: opens, inspection, retention and the option builders refuse
//   it before calling any C function other than the version queries;
// - an allocation failure after a clean abort of a throwing fill must not replace the fill's exception.
//
// It replaces operator new so that allocations can be made to fail on demand. Under Valgrind it must run with
// --soname-synonyms=somalloc=nouserintercepts (tests/c_api.rs does), or Valgrind would replace that operator new;
// a positive control checks that the injection works.
#include "tickertape.hpp"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <stdexcept>
#include <string>
#include <vector>

namespace tt = tickertape;
using namespace std::chrono_literals;

// While set, every operator new throws std::bad_alloc.
static bool fail_allocations = false;

void *operator new(std::size_t size) {
    if (fail_allocations) throw std::bad_alloc();
    if (void *p = std::malloc(size ? size : 1)) return p;
    throw std::bad_alloc();
}
// Not inlined: GCC's -Wmismatched-new-delete would otherwise see free() applied to the result of operator new once
// the replacement operator delete is inlined into a caller.
__attribute__((noinline)) static void release(void *p) noexcept { std::free(p); }
void operator delete(void *p) noexcept { release(p); }
void operator delete(void *p, std::size_t) noexcept { release(p); }

static int failures = 0;

#define CHECK(cond)                                                                       \
    do {                                                                                  \
        if (!(cond)) {                                                                    \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                                   \
        }                                                                                 \
    } while (0)

// What the stubs return, and the last-error state they report.
static tickertape_status stub_status = TICKERTAPE_OK;
static std::string stub_message;
static std::int32_t stub_os_code = 0;
static std::int32_t stub_reason = 0;
static std::uint32_t stub_version = TICKERTAPE_HPP_VERSION_NUMBER;
static int stub_opens = 0;
static int stub_closes = 0;
static int stub_inspections = 0;  // calls of the inspection and retention functions
static int stub_calls = 0;        // calls of every C function other than the three version queries
static int stub_writer;
static int stub_retention;
static int stub_writer_options;
static int stub_reader_options;

extern "C" {
uint32_t tickertape_abi_version(void) { return TICKERTAPE_ABI_VERSION; }
uint32_t tickertape_version_number(void) { return stub_version; }
const char *tickertape_version(void) { return "stub"; }

tickertape_status tickertape_writer_open(const char *, const struct tickertape_writer_options *,
                                         struct tickertape_writer **writer_out) {
    stub_calls++;
    stub_opens++;
    *writer_out = reinterpret_cast<struct tickertape_writer *>(&stub_writer);
    return TICKERTAPE_OK;
}

tickertape_status tickertape_writer_close(struct tickertape_writer *writer) {
    stub_calls++;
    if (writer) stub_closes++;
    return TICKERTAPE_OK;
}

tickertape_status tickertape_writer_append_with(struct tickertape_writer *, uint32_t, uint64_t, uint64_t,
                                                size_t payload_len, tickertape_fill_fn fill, void *user_data,
                                                uint64_t *seq_out) {
    stub_calls++;
    unsigned char buf[16] = {0};
    if (fill(user_data, payload_len ? buf : nullptr, payload_len) == 0) {
        if (seq_out) *seq_out = 42;
        return TICKERTAPE_OK;
    }
    return stub_status;
}

tickertape_status tickertape_writer_checkpoint(struct tickertape_writer *) {
    stub_calls++;
    return stub_status;
}

tickertape_status tickertape_verify(const char *, tickertape_verify_report *) {
    stub_calls++;
    stub_inspections++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_list_segments(const char *, tickertape_segment_info *, size_t, size_t *count_out) {
    stub_calls++;
    stub_inspections++;
    *count_out = 0;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_read_checkpoint(const char *, tickertape_checkpoint *) {
    stub_calls++;
    stub_inspections++;
    return TICKERTAPE_NONE;
}
tickertape_status tickertape_retention_apply(const char *, const struct tickertape_retention *, int32_t,
                                             tickertape_retention_report *) {
    stub_calls++;
    stub_inspections++;
    return TICKERTAPE_OK;
}
struct tickertape_retention *tickertape_retention_new(void) {
    stub_calls++;
    return reinterpret_cast<struct tickertape_retention *>(&stub_retention);
}
void tickertape_retention_free(struct tickertape_retention *) { stub_calls++; }
tickertape_status tickertape_retention_set_max_segments(struct tickertape_retention *, uint64_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_retention_set_max_bytes(struct tickertape_retention *, uint64_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_retention_set_max_age_ns(struct tickertape_retention *, uint64_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_retention_set_delete(struct tickertape_retention *) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_retention_set_move_to(struct tickertape_retention *, const char *) {
    stub_calls++;
    return TICKERTAPE_OK;
}

struct tickertape_writer_options *tickertape_writer_options_new(void) {
    stub_calls++;
    return reinterpret_cast<struct tickertape_writer_options *>(&stub_writer_options);
}
void tickertape_writer_options_free(struct tickertape_writer_options *) { stub_calls++; }
tickertape_status tickertape_writer_options_set_segment_size(struct tickertape_writer_options *, uint64_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_writer_options_set_recovery_policy(struct tickertape_writer_options *, int32_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_writer_options_set_notify(struct tickertape_writer_options *, int32_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_writer_options_set_prefault(struct tickertape_writer_options *, int32_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_writer_options_set_retention(struct tickertape_writer_options *,
                                                          const struct tickertape_retention *) {
    stub_calls++;
    return TICKERTAPE_OK;
}

struct tickertape_reader_options *tickertape_reader_options_new(void) {
    stub_calls++;
    return reinterpret_cast<struct tickertape_reader_options *>(&stub_reader_options);
}
void tickertape_reader_options_free(struct tickertape_reader_options *) { stub_calls++; }
tickertape_status tickertape_reader_options_set_durable_only(struct tickertape_reader_options *, int32_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_reader_options_set_notify(struct tickertape_reader_options *, int32_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_reader_options_set_backoff(struct tickertape_reader_options *, uint32_t, uint32_t,
                                                        uint64_t, uint64_t) {
    stub_calls++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_reader_options_set_busy_spin(struct tickertape_reader_options *) {
    stub_calls++;
    return TICKERTAPE_OK;
}

const char *tickertape_last_error_message(void) {
    stub_calls++;
    return stub_message.c_str();
}
int32_t tickertape_last_error_os_code(void) {
    stub_calls++;
    return stub_os_code;
}
int32_t tickertape_last_error_corrupt_reason(void) {
    stub_calls++;
    return stub_reason;
}
const char *tickertape_status_name(tickertape_status status) {
    stub_calls++;
    switch (status) {
        case TICKERTAPE_E_IO: return "TICKERTAPE_E_IO";
        case TICKERTAPE_E_ABORTED: return "TICKERTAPE_E_ABORTED";
        default: return "OTHER";
    }
}
}  // extern "C"

static void throwing_fill(std::uint8_t *, std::size_t) { throw std::runtime_error("fill failed"); }

static void failed_cleanup() {
    tt::Writer writer = tt::Writer::open("unused").value();

    // A clean abort: the fill's own exception reaches the caller.
    stub_status = TICKERTAPE_E_ABORTED;
    stub_message = "the fill callback returned non-zero; nothing was appended";
    try {
        (void)writer.append_with(1, 0, 0, 8, throwing_fill);
        CHECK(false);
    } catch (const tt::Exception &) {
        CHECK(false);  // the library error must not replace the caller's exception
    } catch (const std::runtime_error &e) {
        CHECK(std::string(e.what()) == "fill failed");
    }

    // The clean-up failed: the library error reaches the caller, with the fill's exception nested inside.
    stub_status = TICKERTAPE_E_IO;
    stub_os_code = 28;  // ENOSPC on Linux; the code is TICKERTAPE_E_IO whatever the OS error
    stub_message = "the fill callback returned non-zero, and zeroing the abandoned bytes durably failed";
    try {
        (void)writer.append_with(1, 0, 0, 8, throwing_fill);
        CHECK(false);
    } catch (const tt::Exception &e) {
        CHECK(e.status() == tt::Status::io && e.error().os_code() == 28);
        CHECK(std::strstr(e.what(), "zeroing") != nullptr);
        bool nested = false;
        try {
            std::rethrow_if_nested(e);
        } catch (const std::runtime_error &cause) {
            nested = std::string(cause.what()) == "fill failed";
        }
        CHECK(nested);
    } catch (const std::exception &e) {
        std::fprintf(stderr, "the library error was dropped; caught: %s\n", e.what());
        CHECK(false);
    }

    // The same failure after a fill that returned false: a plain error, no exception.
    auto refused = writer.append_with(1, 0, 0, 8, [](std::uint8_t *, std::size_t) { return false; });
    CHECK(!refused && refused.error().status() == tt::Status::io && refused.error().os_code() == 28);

    // A clean abort whose error message cannot be copied (allocations fail from inside the fill on): the fill's own
    // exception still reaches the caller, not std::bad_alloc. The binding must rethrow before it builds an Error.
    struct FillError {
        int code;
    };
    stub_status = TICKERTAPE_E_ABORTED;
    stub_message = "the fill callback returned non-zero; nothing was appended (a message too long for SSO)";
    bool injected = false;  // positive control: the injection works (Valgrind must not replace operator new)
    fail_allocations = true;
    try {
        ::operator delete(::operator new(64));
    } catch (const std::bad_alloc &) {
        injected = true;
    }
    fail_allocations = false;
    CHECK(injected);
    try {
        (void)writer.append_with(1, 0, 0, 8, [](std::uint8_t *, std::size_t) {
            fail_allocations = true;
            throw FillError{7};  // allocated by the C++ runtime with malloc, not operator new
        });
        fail_allocations = false;
        CHECK(false);
    } catch (const FillError &e) {
        fail_allocations = false;
        CHECK(e.code == 7);
    } catch (const std::bad_alloc &) {
        fail_allocations = false;
        std::fprintf(stderr, "std::bad_alloc replaced the fill's exception\n");
        CHECK(false);
    }

    // A fill that does not throw still commits; the writer was never closed behind our back.
    stub_os_code = 0;
    CHECK(writer.append_with(1, 0, 0, 8, [](std::uint8_t *, std::size_t) {}).value() == 42);
    CHECK(stub_closes == 0);
}

// Every status and reason the C ABI defines, and one it does not, reach the Error unchanged.
static void every_status_and_reason() {
    tt::Writer writer = tt::Writer::open("unused").value();
    const std::vector<tickertape_status> statuses = {
        TICKERTAPE_E_INVALID_ARGUMENT, TICKERTAPE_E_IO,          TICKERTAPE_E_STORAGE_FULL,
        TICKERTAPE_E_LOCKED,           TICKERTAPE_E_CORRUPT,     TICKERTAPE_E_UNSUPPORTED_VERSION,
        TICKERTAPE_E_NOT_FOUND,        TICKERTAPE_E_SEQ_UNAVAILABLE, TICKERTAPE_E_RECORD_TOO_LARGE,
        TICKERTAPE_E_DURABILITY_LOST,  TICKERTAPE_E_SEQUENCE_EXHAUSTED, TICKERTAPE_E_DISCARDED,
        TICKERTAPE_E_INVALID_CONFIG,   TICKERTAPE_E_BUFFER_TOO_SMALL, TICKERTAPE_E_ABORTED,
        TICKERTAPE_E_BUSY,             TICKERTAPE_E_PANIC,       TICKERTAPE_E_BEACON_IN_USE,
        TICKERTAPE_E_UNKNOWN,
        -100,  // a code from a newer library
    };
    for (tickertape_status status : statuses) {
        stub_status = status;
        stub_message = "message " + std::to_string(status);
        stub_os_code = status == TICKERTAPE_E_IO || status == TICKERTAPE_E_STORAGE_FULL ? 5 : 0;
        stub_reason = status == TICKERTAPE_E_CORRUPT ? TICKERTAPE_CORRUPT_BAD_CHECKSUM : 0;
        auto r = writer.checkpoint();
        CHECK(!r);
        if (r) continue;
        CHECK(static_cast<tickertape_status>(r.error().status()) == status);
        CHECK(r.error().message() == stub_message && r.error().os_code() == stub_os_code);
        CHECK(static_cast<std::int32_t>(r.error().corrupt_reason()) == stub_reason);
    }
    const std::vector<std::int32_t> reasons = {
        TICKERTAPE_CORRUPT_BAD_MAGIC,      TICKERTAPE_CORRUPT_BAD_SEGMENT_HEADER, TICKERTAPE_CORRUPT_SIZE_MISMATCH,
        TICKERTAPE_CORRUPT_BASE_SEQ_MISMATCH, TICKERTAPE_CORRUPT_BAD_OFFSET,   TICKERTAPE_CORRUPT_SEQUENCE_EXHAUSTED,
        TICKERTAPE_CORRUPT_BAD_LENGTH,     TICKERTAPE_CORRUPT_BAD_CHECKSUM,       TICKERTAPE_CORRUPT_BAD_SEQUENCE,
        TICKERTAPE_CORRUPT_BAD_FLAGS,      TICKERTAPE_CORRUPT_BAD_SEAL,           TICKERTAPE_CORRUPT_BROKEN_CHAIN,
        TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT, TICKERTAPE_CORRUPT_EMPTY_COMMIT_WORD,
        TICKERTAPE_CORRUPT_UNCHECKED_TAIL, TICKERTAPE_CORRUPT_UNKNOWN,            77,
    };
    stub_status = TICKERTAPE_E_CORRUPT;
    stub_os_code = 0;
    for (std::int32_t reason : reasons) {
        stub_reason = reason;
        auto r = writer.checkpoint();
        CHECK(!r && static_cast<std::int32_t>(r.error().corrupt_reason()) == reason);
    }
    stub_reason = 0;
    stub_status = TICKERTAPE_OK;
    CHECK(writer.checkpoint().has_value());
    stub_status = TICKERTAPE_NONE;  // a success
    CHECK(writer.checkpoint().has_value());
}

static void mismatched_library() {
    const int opens = stub_opens;
    stub_version = TICKERTAPE_HPP_VERSION_NUMBER + 1;
    auto refused = tt::Writer::open("unused");
    CHECK(!refused && refused.error().status() == tt::Status::unsupported_version);
    CHECK(!refused && refused.error().message().find("stub") != std::string::npos);
    CHECK(!tt::library_compatible() && !tt::check_library());
    CHECK(stub_opens == opens);  // refused before the C ABI was called

    // Every function that takes a journal path checks the library, not only the opens.
    const int inspections = stub_inspections;
    const tt::Retention retention;
    CHECK(!tt::verify("unused") && tt::verify("unused").error().status() == tt::Status::unsupported_version);
    CHECK(!tt::list_segments("unused") &&
          tt::list_segments("unused").error().status() == tt::Status::unsupported_version);
    CHECK(!tt::read_checkpoint("unused") &&
          tt::read_checkpoint("unused").error().status() == tt::Status::unsupported_version);
    CHECK(!tt::plan_retention("unused", retention) &&
          tt::plan_retention("unused", retention).error().status() == tt::Status::unsupported_version);
    CHECK(!tt::apply_retention("unused", retention) &&
          tt::apply_retention("unused", retention).error().status() == tt::Status::unsupported_version);
    CHECK(stub_inspections == inspections);
    stub_version = TICKERTAPE_HPP_VERSION_NUMBER;
    CHECK(tt::library_compatible());
    // With a matching library the same calls reach the C ABI. (`retention` was made with the mismatched library, so
    // it holds that error: make new settings.)
    CHECK(retention.error() && retention.error()->status() == tt::Status::unsupported_version);
    const tt::Retention compatible;
    CHECK(tt::verify("unused").has_value() && tt::list_segments("unused").has_value());
    CHECK(tt::read_checkpoint("unused").has_value() && tt::plan_retention("unused", compatible).has_value());
    CHECK(stub_inspections == inspections + 5);  // list_segments calls the C function twice (count, then fill)
}

// The option builders check the library before they create their C handle: with a mismatched library neither they
// nor their setters call any C function other than the version queries, and the open that uses them reports it.
static void builders_check_the_library_first() {
    stub_version = TICKERTAPE_HPP_VERSION_NUMBER + 1;
    const int calls = stub_calls;
    {
        tt::Retention retention;
        retention.max_segments(1).max_bytes(2).max_age(1s).delete_segments().move_to("unused");
        tt::WriterOptions writer_options;
        writer_options.segment_size(65536).recovery_policy(tt::RecoveryPolicy::fail).notify(true).prefault(false);
        writer_options.retention(retention).no_retention();
        tt::ReaderOptions reader_options;
        reader_options.durable_only(true).notify(false).backoff(1, 1, 1us, 2us).busy_spin();
        for (const std::optional<tt::Error> *error : {&retention.error(), &writer_options.error(), &reader_options.error()}) {
            CHECK(error->has_value() && (*error)->status() == tt::Status::unsupported_version);
            CHECK(error->has_value() && (*error)->message().find("stub") != std::string::npos);
        }
        CHECK(retention.native_handle() == nullptr && writer_options.native_handle() == nullptr);
        CHECK(reader_options.native_handle() == nullptr);
        auto opened = tt::Writer::open("unused", writer_options);
        CHECK(!opened && opened.error().status() == tt::Status::unsupported_version);
        tt::WriterOptions moved(std::move(writer_options));  // the error moves with it
        CHECK(moved.error() && moved.error()->status() == tt::Status::unsupported_version && !writer_options.error());
        CHECK(!tt::plan_retention("unused", retention));
    }
    CHECK(stub_calls == calls);  // nothing but the version queries
    stub_version = TICKERTAPE_HPP_VERSION_NUMBER;
    {
        tt::WriterOptions writer_options;
        writer_options.segment_size(65536);
        CHECK(!writer_options.error() && writer_options.native_handle() != nullptr);
    }
    CHECK(stub_calls == calls + 3);  // positive control: new, the setter, free
}

int main() {
    failed_cleanup();
    every_status_and_reason();
    mismatched_library();
    builders_check_the_library_first();
    if (failures) {
        std::fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
