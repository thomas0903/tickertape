// Record, follow live and replay with the C++ binding (tickertape.hpp, next to tickertape.h).
//
//   example <dir> record <count> [interval-ms]   append <count> records, checkpointing every 10 and at the end
//   example <dir> follow <count>                 wait for the writer, then print <count> records as they arrive
//   example <dir> replay [from-seq]              print the records from <from-seq> (default: the oldest) on
//
// With TICKERTAPE_DURABLE_ONLY=1, follow and replay return only checkpointed (durable) records. Errors are handled
// through tickertape::result without exceptions, so this also builds with -fno-exceptions.
#include "tickertape.hpp"

#include <cerrno>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>

namespace tt = tickertape;
using namespace std::chrono_literals;

static int fail(const char *what, const tt::Error &error) {
    std::fprintf(stderr, "%s: %s\n", what, error.to_string().c_str());
    return 1;
}

static void print(const tt::RecordView &record) {
    // The payload is borrowed from the reader: valid until the next call on it. Its data() is null when it is empty.
    const std::string_view text = record.as_string_view();
    std::printf("%llu schema=%u %.*s\n", static_cast<unsigned long long>(record.seq()), record.schema_id(),
                static_cast<int>(text.size()), text.empty() ? "" : text.data());
}

static tt::ReaderOptions reader_options() {
    tt::ReaderOptions options;
    const char *durable = std::getenv("TICKERTAPE_DURABLE_ONLY");
    options.durable_only(durable != nullptr && std::strcmp(durable, "1") == 0);
    return options;
}

static int record(const std::string &dir, int count, int interval_ms) {
    tt::WriterOptions options;
    options.segment_size(64 * 1024);  // small segments, so that a short run rolls over
    tt::result<tt::Writer> opened = tt::Writer::open(dir, options);
    if (!opened) return fail("open", opened.error());
    tt::Writer writer = std::move(opened).value();
    for (int i = 0; i < count; i++) {
        const std::string payload = "order " + std::to_string(i);
        // Zero-copy for odd records: the lambda writes straight into the mapped segment.
        const auto fill = [&](std::uint8_t *buf, std::size_t len) { std::memcpy(buf, payload.data(), len); };
        tt::result<std::uint64_t> seq = i % 2 == 0 ? writer.append(1, 0, tt::realtime_ns(), payload)
                                                   : writer.append_with(2, 0, tt::realtime_ns(), payload.size(), fill);
        if (!seq) return fail("append", seq.error());  // visible to readers from here on
        if ((i + 1) % 10 == 0 || i + 1 == count) {
            if (tt::result<void> cp = writer.checkpoint(); !cp) return fail("checkpoint", cp.error());  // durable
        }
        if (interval_ms > 0) std::this_thread::sleep_for(std::chrono::milliseconds(interval_ms));
    }
    return 0;
}

static int follow(const std::string &dir, int count) {
    // The writer creates the journal: until then the open fails with io/ENOENT, or not_found while it creates the
    // first segment. Retry on exactly those two.
    const auto deadline = std::chrono::steady_clock::now() + 30s;
    tt::Reader reader;
    for (;;) {
        tt::result<tt::Reader> opened = tt::Reader::open(dir, reader_options());
        if (opened) {
            reader = std::move(opened).value();
            break;
        }
        const tt::Error &e = opened.error();
        const bool waiting =
            (e.status() == tt::Status::io && e.os_code() == ENOENT) || e.status() == tt::Status::not_found;
        if (!waiting || std::chrono::steady_clock::now() > deadline) return fail("open", e);
        std::this_thread::sleep_for(10ms);
    }
    for (int seen = 0; seen < count;) {
        // A finite timeout keeps the loop stoppable; a real program would check for a stop request here.
        tt::result<std::optional<tt::RecordView>> next = reader.next(100ms);
        if (!next) {
            if (next.error().status() == tt::Status::discarded) {  // recovery or retention replaced the segment
                tt::result<std::uint64_t> at = reader.position();
                if (at && reader.seek(*at)) continue;
            }
            return fail("next", next.error());
        }
        if (!*next) continue;  // nothing new yet
        print(**next);
        seen++;
    }
    return 0;
}

static int replay(const std::string &dir, std::uint64_t from, bool has_from) {
    tt::result<tt::Reader> opened = tt::Reader::open(dir, reader_options());
    if (!opened) return fail("open", opened.error());
    tt::Reader reader = std::move(opened).value();
    if (has_from) {
        if (tt::result<void> sought = reader.seek(from); !sought) return fail("seek", sought.error());
    }
    // Batches: one call returns many records, copied into memory the batch allocated once.
    tt::RecordBatch batch(64, 64 * 1024);
    for (;;) {
        tt::result<std::size_t> n = reader.read_batch(batch);  // don't wait: replay what is there
        if (!n) return fail("read", n.error());
        if (*n == 0) return 0;
        for (const tt::RecordView &record : batch) print(record);
    }
}

int main(int argc, char **argv) {
    if (argc < 3) {
        std::fprintf(stderr, "usage: %s <dir> record <count> [interval-ms] | follow <count> | replay [from-seq]\n",
                     argv[0]);
        return 2;
    }
    if (tt::result<void> compatible = tt::check_library(); !compatible) return fail("library", compatible.error());
    const std::string dir = argv[1];
    const std::string mode = argv[2];
    if (mode == "record" && argc >= 4) return record(dir, std::atoi(argv[3]), argc >= 5 ? std::atoi(argv[4]) : 0);
    if (mode == "follow" && argc == 4) return follow(dir, std::atoi(argv[3]));
    if (mode == "replay") return replay(dir, argc >= 4 ? std::strtoull(argv[3], nullptr, 10) : 0, argc >= 4);
    std::fprintf(stderr, "unknown mode %s\n", mode.c_str());
    return 2;
}
