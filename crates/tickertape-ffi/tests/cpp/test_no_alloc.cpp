// Checks that the C++ binding allocates nothing on the append and read paths, compiled by tests/c_api.rs as C++17.
// It replaces every global operator new (plain, array, aligned and nothrow, so that no C++ allocation bypasses the
// counter) to count C++ allocations made by the binding (the library's own allocations go through malloc and are not
// the binding's), then appends and reads in a loop with every hot-path function. A positive control first checks
// that a deliberate allocation reaches the counter, so the test cannot pass without counting.
//
// Under Valgrind it must run with --soname-synonyms=somalloc=nouserintercepts (tests/c_api.rs does): otherwise
// Valgrind replaces these operators with its own, the counter never moves, and the positive control fails.
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

static void *allocate(std::size_t size, std::size_t alignment) noexcept {
    if (counting) allocations++;
    if (size == 0) size = 1;
    if (alignment <= alignof(std::max_align_t)) return std::malloc(size);
    void *p = nullptr;
    return posix_memalign(&p, alignment, size) == 0 ? p : nullptr;
}
static void *allocate_or_throw(std::size_t size, std::size_t alignment) {
    if (void *p = allocate(size, alignment)) return p;
    throw std::bad_alloc();
}

void *operator new(std::size_t size) { return allocate_or_throw(size, 0); }
void *operator new[](std::size_t size) { return allocate_or_throw(size, 0); }
void *operator new(std::size_t size, std::align_val_t al) {
    return allocate_or_throw(size, static_cast<std::size_t>(al));
}
void *operator new[](std::size_t size, std::align_val_t al) {
    return allocate_or_throw(size, static_cast<std::size_t>(al));
}
void *operator new(std::size_t size, const std::nothrow_t &) noexcept { return allocate(size, 0); }
void *operator new[](std::size_t size, const std::nothrow_t &) noexcept { return allocate(size, 0); }
void *operator new(std::size_t size, std::align_val_t al, const std::nothrow_t &) noexcept {
    return allocate(size, static_cast<std::size_t>(al));
}
void *operator new[](std::size_t size, std::align_val_t al, const std::nothrow_t &) noexcept {
    return allocate(size, static_cast<std::size_t>(al));
}
// Not inlined: GCC's -Wmismatched-new-delete would otherwise see free() applied to the result of operator new once
// the replacement operator delete is inlined into a caller.
__attribute__((noinline)) static void release(void *p) noexcept { std::free(p); }
void operator delete(void *p) noexcept { release(p); }
void operator delete[](void *p) noexcept { release(p); }
void operator delete(void *p, std::size_t) noexcept { release(p); }
void operator delete[](void *p, std::size_t) noexcept { release(p); }
void operator delete(void *p, std::align_val_t) noexcept { release(p); }
void operator delete[](void *p, std::align_val_t) noexcept { release(p); }
void operator delete(void *p, std::size_t, std::align_val_t) noexcept { release(p); }
void operator delete[](void *p, std::size_t, std::align_val_t) noexcept { release(p); }
void operator delete(void *p, const std::nothrow_t &) noexcept { release(p); }
void operator delete[](void *p, const std::nothrow_t &) noexcept { release(p); }
void operator delete(void *p, std::align_val_t, const std::nothrow_t &) noexcept { release(p); }
void operator delete[](void *p, std::align_val_t, const std::nothrow_t &) noexcept { release(p); }

template <class T>
static T must(tt::result<T> r) {
    if (!r) {
        std::fprintf(stderr, "unexpected error: %s\n", r.error().to_string().c_str());
        std::exit(1);
    }
    return std::move(r).value();
}

// Each kind of allocation must reach the counter. Explicit calls of the replaceable functions are never elided
// (unlike new-expressions), so the compiler cannot optimise these away.
static bool counting_works() {
    counting = true;
    const unsigned long before = allocations;
    ::operator delete(::operator new(16));
    ::operator delete[](::operator new[](16));
    ::operator delete(::operator new(16, std::align_val_t(64)), std::align_val_t(64));
    ::operator delete(::operator new(16, std::nothrow));
    ::operator delete(::operator new(16, std::align_val_t(64), std::nothrow), std::align_val_t(64));
    counting = false;
    const unsigned long seen = allocations - before;
    allocations = before;
    if (seen != 5) {
        std::fprintf(stderr, "positive control: %lu of 5 deliberate allocations were counted\n", seen);
        return false;
    }
    return true;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    if (!counting_works()) return 1;
    char dir[4096];  // not a std::string: no allocation of the test's own
    if (std::snprintf(dir, sizeof dir, "%s/no-alloc", argv[1]) >= static_cast<int>(sizeof dir)) return 2;
    tt::WriterOptions options;
    options.segment_size(1 << 20).prefault(false);
    tt::Writer writer = must(tt::Writer::open(dir, options));
    tt::Reader reader = must(tt::Reader::open(dir));
    tt::RecordBatch batch(16, 4096);
    std::array<std::uint8_t, 64> buffer{};
    const std::array<std::uint8_t, 8> bytes{1, 2, 3, 4, 5, 6, 7, 8};
    const std::array<tt::NewRecord, 3> records{tt::new_record(1, 0, 0, bytes), tt::new_record(1, 0, 0, bytes),
                                               tt::new_record(1, 0, 0, std::string_view("batch"))};

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
            if (auto r = must(reader.next_copy(buffer))) seen += r->size();
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
