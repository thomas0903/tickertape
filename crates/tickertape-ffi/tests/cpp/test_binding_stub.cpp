// Tests of the C++ binding (include/tickertape.hpp) against stubbed C functions (no library is linked), compiled by
// tests/c_api.rs as C++17. It covers what the real library cannot be made to do on demand:
//
// - a fill that throws, followed by a failed clean-up (TICKERTAPE_E_IO instead of TICKERTAPE_E_ABORTED): the binding
//   must throw the library error with the fill's exception nested inside it, so that the caller learns both that
//   the writer has stopped and why the fill failed;
// - a fill that returns false, followed by a failed clean-up: a plain Status::io error;
// - every status code and every corruption reason, with the OS error, reaching an Error unchanged (including a code
//   from a newer library that this header does not name);
// - a library whose version does not match the header: opens, inspection and retention refuse it before calling
//   the C ABI.
#include "tickertape.hpp"

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>

namespace tt = tickertape;

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
static int stub_writer;
static int stub_retention;

extern "C" {
uint32_t tickertape_abi_version(void) { return TICKERTAPE_ABI_VERSION; }
uint32_t tickertape_version_number(void) { return stub_version; }
const char *tickertape_version(void) { return "stub"; }

tickertape_status tickertape_writer_open(const char *, const struct tickertape_writer_options *,
                                         struct tickertape_writer **writer_out) {
    stub_opens++;
    *writer_out = reinterpret_cast<struct tickertape_writer *>(&stub_writer);
    return TICKERTAPE_OK;
}

tickertape_status tickertape_writer_close(struct tickertape_writer *writer) {
    if (writer) stub_closes++;
    return TICKERTAPE_OK;
}

tickertape_status tickertape_writer_append_with(struct tickertape_writer *, uint32_t, uint64_t, uint64_t,
                                                size_t payload_len, tickertape_fill_fn fill, void *user_data,
                                                uint64_t *seq_out) {
    unsigned char buf[16] = {0};
    if (fill(user_data, payload_len ? buf : nullptr, payload_len) == 0) {
        if (seq_out) *seq_out = 42;
        return TICKERTAPE_OK;
    }
    return stub_status;
}

tickertape_status tickertape_writer_checkpoint(struct tickertape_writer *) { return stub_status; }

tickertape_status tickertape_verify(const char *, tickertape_verify_report *) {
    stub_inspections++;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_list_segments(const char *, tickertape_segment_info *, size_t, size_t *count_out) {
    stub_inspections++;
    *count_out = 0;
    return TICKERTAPE_OK;
}
tickertape_status tickertape_read_checkpoint(const char *, tickertape_checkpoint *) {
    stub_inspections++;
    return TICKERTAPE_NONE;
}
tickertape_status tickertape_retention_apply(const char *, const struct tickertape_retention *, int32_t,
                                             tickertape_retention_report *) {
    stub_inspections++;
    return TICKERTAPE_OK;
}
struct tickertape_retention *tickertape_retention_new(void) {
    return reinterpret_cast<struct tickertape_retention *>(&stub_retention);
}
void tickertape_retention_free(struct tickertape_retention *) {}

const char *tickertape_last_error_message(void) { return stub_message.c_str(); }
int32_t tickertape_last_error_os_code(void) { return stub_os_code; }
int32_t tickertape_last_error_corrupt_reason(void) { return stub_reason; }
const char *tickertape_status_name(tickertape_status status) {
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
        TICKERTAPE_E_BUSY,             TICKERTAPE_E_PANIC,       TICKERTAPE_E_UNKNOWN,
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
        TICKERTAPE_CORRUPT_DANGLING_CHECKPOINT, TICKERTAPE_CORRUPT_UNKNOWN,       77,
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
    // With a matching library the same calls reach the C ABI.
    CHECK(tt::verify("unused").has_value() && tt::list_segments("unused").has_value());
    CHECK(tt::read_checkpoint("unused").has_value() && tt::plan_retention("unused", retention).has_value());
    CHECK(stub_inspections == inspections + 5);  // list_segments calls the C function twice (count, then fill)
}

int main() {
    failed_cleanup();
    every_status_and_reason();
    mismatched_library();
    if (failures) {
        std::fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
