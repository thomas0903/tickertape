// C++11 test of the C ABI through the RAII wrapper in examples/c/tickertape.hpp, compiled against the committed
// header by tests/c_api.rs and linked against the shared library. Usage: test_wrapper <scratch-dir>
#include "tickertape.hpp"

#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <string>
#include <vector>

static int failures = 0;

#define CHECK(cond)                                                                       \
    do {                                                                                  \
        if (!(cond)) {                                                                    \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                                   \
        }                                                                                 \
    } while (0)

static std::string payload_for(uint64_t seq) { return "event-" + std::to_string(seq); }

static void roundtrip(const std::string &dir) {
    tickertape::WriterOptions options;
    options.segment_size(65536).prefault(false);
    tickertape::Writer writer(dir, &options);

    // A second writer is refused with a typed exception.
    try {
        tickertape::Writer second(dir);
        CHECK(false);
    } catch (const tickertape::Error &e) {
        CHECK(e.status() == TICKERTAPE_E_LOCKED);
        CHECK(std::strstr(e.what(), "TICKERTAPE_E_LOCKED") != nullptr);
    }

    for (uint64_t seq = 0; seq < 2000; seq++) {
        std::string p = payload_for(seq);
        CHECK(writer.append(7, seq, seq * 2, p.data(), p.size()) == seq);
    }
    // Zero-copy append with a lambda.
    const std::string zc = "zero-copy";
    uint64_t seq = writer.append_with(8, 1, 2, zc.size(), [&](uint8_t *buf, std::size_t len) {
        std::memcpy(buf, zc.data(), len);
    });
    CHECK(seq == 2000);
    // An exception thrown by the fill function propagates and nothing is committed.
    try {
        writer.append_with(8, 0, 0, 4, [](uint8_t *, std::size_t) { throw std::runtime_error("no"); });
        CHECK(false);
    } catch (const std::runtime_error &e) {
        CHECK(std::string(e.what()) == "no");
    }
    CHECK(writer.next_seq() == 2001);
    writer.checkpoint();
    writer.append(9, 0, 0, "late", 4);  // visible, not durable

    tickertape_record r;
    tickertape::Reader reader(dir);
    for (uint64_t s = 0; s < 2000; s++) {
        CHECK(reader.next(r));
        CHECK(r.seq == s && r.schema_id == 7 && r.venue_ts_ns == s && r.local_ts_ns == s * 2);
        CHECK(std::string(reinterpret_cast<const char *>(r.payload), r.payload_len) == payload_for(s));
    }
    CHECK(reader.next(r));
    CHECK(std::string(reinterpret_cast<const char *>(r.payload), r.payload_len) == zc);
    CHECK(reader.next(r) && r.seq == 2001);
    CHECK(!reader.next(r, std::chrono::milliseconds(2)));
    CHECK(reader.position() == 2002);

    tickertape::ReaderOptions durable;
    durable.durable_only(true);
    tickertape::Reader mapped = tickertape::Reader::open_mapped_unsafe(dir, &durable);
    mapped.seek(1990);
    uint64_t last = 0;
    while (mapped.next(r)) last = r.seq;
    CHECK(last == 2000);  // 2001 is not covered by the checkpoint
    CHECK(mapped.position() == 2001);

    try {
        reader.seek(0);
        tickertape::Reader missing(dir + "/does-not-exist");
        CHECK(false);
    } catch (const tickertape::Error &e) {
        CHECK(e.status() == TICKERTAPE_E_IO);
    }
}

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(stderr, "usage: %s <scratch-dir>\n", argv[0]);
        return 2;
    }
    if (tickertape_abi_version() != TICKERTAPE_ABI_VERSION) return 1;
    try {
        roundtrip(std::string(argv[1]) + "/cpp");
    } catch (const std::exception &e) {
        std::fprintf(stderr, "unexpected exception: %s\n", e.what());
        return 1;
    }
    if (failures) {
        std::fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
