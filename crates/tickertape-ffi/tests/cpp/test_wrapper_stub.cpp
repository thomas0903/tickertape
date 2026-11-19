// C++11 test of examples/c/tickertape.hpp against stubbed C functions (no library is linked), compiled by
// tests/c_api.rs. It covers what the real library cannot be made to do on demand: a fill that throws, followed by a
// failed clean-up (TICKERTAPE_E_IO instead of TICKERTAPE_E_ABORTED). The wrapper must then throw the library error,
// with the fill's exception nested as its cause, so that the caller learns the writer has stopped.
#include "tickertape.hpp"

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>

static int failures = 0;

#define CHECK(cond)                                                                       \
    do {                                                                                  \
        if (!(cond)) {                                                                    \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                                   \
        }                                                                                 \
    } while (0)

// What the stubbed tickertape_writer_append_with returns after calling the fill, and the message it records.
static tickertape_status stub_status = TICKERTAPE_OK;
static const char *stub_message = "";
static int stub_writer;

extern "C" {
tickertape_status tickertape_writer_open(const char *, const struct tickertape_writer_options *,
                                         struct tickertape_writer **writer_out) {
    *writer_out = reinterpret_cast<struct tickertape_writer *>(&stub_writer);
    return TICKERTAPE_OK;
}

tickertape_status tickertape_writer_close(struct tickertape_writer *) { return TICKERTAPE_OK; }

tickertape_status tickertape_writer_append_with(struct tickertape_writer *, uint32_t, uint64_t, uint64_t,
                                                size_t payload_len, tickertape_fill_fn fill, void *user_data,
                                                uint64_t *seq_out) {
    unsigned char buf[16] = {0};
    if (fill(user_data, buf, payload_len) == 0) {
        if (seq_out) *seq_out = 42;
        return TICKERTAPE_OK;
    }
    return stub_status;
}

const char *tickertape_last_error_message(void) { return stub_message; }

const char *tickertape_status_name(tickertape_status status) {
    switch (status) {
        case TICKERTAPE_E_IO: return "TICKERTAPE_E_IO";
        case TICKERTAPE_E_ABORTED: return "TICKERTAPE_E_ABORTED";
        default: return "OTHER";
    }
}
}  // extern "C"

static void throwing_fill(uint8_t *, std::size_t) { throw std::runtime_error("fill failed"); }

int main() {
    tickertape::Writer writer("unused");

    // A clean abort: the fill's own exception reaches the caller.
    stub_status = TICKERTAPE_E_ABORTED;
    stub_message = "the fill callback returned non-zero; nothing was appended";
    try {
        writer.append_with(1, 0, 0, 8, throwing_fill);
        CHECK(false);
    } catch (const tickertape::Error &) {
        CHECK(false);  // the library error must not replace the caller's exception
    } catch (const std::runtime_error &e) {
        CHECK(std::string(e.what()) == "fill failed");
    }

    // The clean-up failed: the library error reaches the caller, with the fill's exception nested inside.
    stub_status = TICKERTAPE_E_IO;
    stub_message = "the fill callback returned non-zero, and zeroing the abandoned bytes durably failed";
    try {
        writer.append_with(1, 0, 0, 8, throwing_fill);
        CHECK(false);
    } catch (const tickertape::Error &e) {
        CHECK(e.status() == TICKERTAPE_E_IO);
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

    // A fill that does not throw still commits.
    CHECK(writer.append_with(1, 0, 0, 8, [](uint8_t *, std::size_t) {}) == 42);

    if (failures) {
        std::fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
