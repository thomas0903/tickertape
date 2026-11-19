// Checks that the C++ binding allocates nothing on the append and read paths, compiled by tests/c_api.rs as C++17.
// It replaces the global operator new to count C++ allocations made by the binding (the library's own allocations
// go through malloc and are not the binding's), then appends and reads in a loop with every hot-path function.
//
//   test_no_alloc <scratch-dir>
#include "tickertape.hpp"

#include <array>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <string_view>

namespace tt = tickertape;

static bool counting = false;
static unsigned long allocations = 0;

void *operator new(std::size_t size) {
    if (counting) allocations++;
    if (void *p = std::malloc(size ? size : 1)) return p;
    std::abort();
}
void *operator new[](std::size_t size) { return operator new(size); }
void operator delete(void *p) noexcept { std::free(p); }
void operator delete[](void *p) noexcept { std::free(p); }
void operator delete(void *p, std::size_t) noexcept { std::free(p); }
void operator delete[](void *p, std::size_t) noexcept { std::free(p); }

template <class T>
static T must(tt::result<T> r) {
    if (!r) {
        std::fprintf(stderr, "unexpected error: %s\n", r.error().to_string().c_str());
        std::exit(1);
    }
    return std::move(r).value();
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    const std::string dir = std::string(argv[1]) + "/no-alloc";
    tt::WriterOptions options;
    options.segment_size(1 << 20).prefault(false);
    tt::Writer writer = must(tt::Writer::open(dir, options));
    tt::Reader reader = must(tt::Reader::open(dir));
    tt::RecordBatch batch(16, 4096);
    std::array<std::uint8_t, 64> buffer{};
    const std::array<std::uint8_t, 8> bytes{1, 2, 3, 4, 5, 6, 7, 8};
    const std::array<tt::NewRecord, 3> records{tt::NewRecord(1, 0, 0, bytes), tt::NewRecord(1, 0, 0, bytes),
                                               tt::NewRecord(1, 0, 0, std::string_view("batch"))};

    unsigned long seen = 0;
    for (int round = 0; round < 2; round++) {  // the first round warms up; the second is counted
        counting = round == 1;
        for (int i = 0; i < 500; i++) {
            (void)must(writer.append(1, 2, 3, std::string_view("copied")));
            (void)must(writer.append(1, 2, 3, bytes.data(), bytes.size()));
            (void)must(writer.append_with(1, 2, 3, 8, [](std::uint8_t *buf, std::size_t len) {
                std::memset(buf, 7, len);
            }));
            (void)must(writer.append_with(1, 2, 3, 8, [](std::uint8_t *, std::size_t) { return true; }));
            tt::BatchAppend appended = writer.append_batch(records);
            if (!appended.outcome || appended.appended != 3) return 1;
            (void)must(writer.next_seq());
            for (int k = 0; k < 2; k++) {
                if (auto r = must(reader.next(tt::no_wait))) seen += r->size();
            }
            if (auto r = must(reader.next_copy(buffer.data(), buffer.size()))) seen += r->size();
            seen += must(reader.read_batch(batch));
            for (tt::RecordView r : batch) seen += r.size();
            (void)must(reader.position());
        }
        while (must(reader.read_batch(batch, std::chrono::microseconds(1))) > 0) seen += batch.size();
    }
    counting = false;
    if (allocations != 0) {
        std::fprintf(stderr, "the binding allocated %lu times on the hot path\n", allocations);
        return 1;
    }
    std::printf("ok (%lu payload bytes and records seen)\n", seen);
    return 0;
}
