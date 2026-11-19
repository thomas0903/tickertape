// Record and replay with the C++ RAII wrapper (tickertape.hpp) over the C ABI.
//
//   example <dir>
#include "tickertape.hpp"

#include <cstdio>
#include <cstring>
#include <string>

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(stderr, "usage: %s <dir>\n", argv[0]);
        return 2;
    }
    if (!tickertape::library_is_compatible()) {
        std::fprintf(stderr, "library %s does not match this header\n", tickertape_version());
        return 1;
    }
    try {
        tickertape::Writer writer(argv[1]);
        uint64_t first = writer.next_seq();
        for (int i = 0; i < 5; i++) {
            std::string payload = "order " + std::to_string(i);
            writer.append(2, 0, tickertape_realtime_ns(), payload.data(), payload.size());
        }
        // Zero-copy: the lambda writes straight into the mapped segment.
        writer.append_with(2, 0, tickertape_realtime_ns(), 9, [](uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "zero-copy", len);
        });
        writer.checkpoint();  // durable from here on

        tickertape::Reader reader(argv[1]);
        reader.seek(first);
        tickertape_record r;
        while (reader.next(r)) {  // don't wait: replay what is there
            // r.payload is NULL when the payload is empty; printf("%.*s") must not get NULL.
            const char *text = r.payload_len > 0 ? reinterpret_cast<const char *>(r.payload) : "";
            std::printf("%llu %.*s\n", static_cast<unsigned long long>(r.seq), static_cast<int>(r.payload_len), text);
        }
    } catch (const tickertape::Error &e) {
        std::fprintf(stderr, "%s\n", e.what());
        return 1;
    }
    return 0;
}
