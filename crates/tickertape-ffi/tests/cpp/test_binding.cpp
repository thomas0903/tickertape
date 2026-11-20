// Tests of the C++ binding (include/tickertape.hpp) against the real library, compiled by tests/c_api.rs as C++17,
// C++20 and C++17 without exceptions, with -Wall -Wextra -Wpedantic -Wconversion -Wshadow -Werror. Modes:
//
//   test_binding <scratch-dir>                         the binding's behaviour
//   test_binding <scratch-dir> throw-from-fill         lets an exception escape a raw C fill callback: must abort
//   test_binding <scratch-dir> stale-view              reads a view after the next call: must abort (debug checks)
//   test_binding <scratch-dir> stale-empty-span        as_span of a stale view of an empty record: must abort (C++20)
//   test_binding write-fixture <dir>                   writes the cross-language fixture journal
//   test_binding read-fixture <dir> <count> <durable>  reads a fixture journal written by another implementation
//
// The fixture is the one in tests/c/test_api.c: record `seq` has schema_id seq % 5 + 1, venue_ts_ns 1000000 + seq,
// local_ts_ns 2000000 + 3 * seq and (seq * 37) % 300 payload bytes, byte i = (seq + 7 * i) & 0xFF; 64 KiB segments.
#include "tickertape.hpp"

#include <algorithm>
#include <array>
#include <cerrno>
#include <chrono>
#include <climits>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iterator>
#include <limits>
#include <optional>
#include <ratio>
#include <stdexcept>
#include <string>
#include <thread>
#include <string_view>
#include <vector>

namespace tt = tickertape;
namespace fs = std::filesystem;
using namespace std::chrono_literals;

static int failures = 0;

#define CHECK(cond)                                                                       \
    do {                                                                                  \
        if (!(cond)) {                                                                    \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
            failures++;                                                                   \
        }                                                                                 \
    } while (0)

// Unwraps a result without relying on exceptions: prints the error and exits on failure.
template <class T>
static T must(tt::result<T> r, const char *expr, int line) {
    if (!r) {
        std::fprintf(stderr, "%s:%d: %s failed: %s\n", __FILE__, line, expr, r.error().to_string().c_str());
        std::exit(1);
    }
    return std::move(r).value();
}
static void must(tt::result<void> r, const char *expr, int line) {
    if (!r) {
        std::fprintf(stderr, "%s:%d: %s failed: %s\n", __FILE__, line, expr, r.error().to_string().c_str());
        std::exit(1);
    }
}
#define MUST(expr) must((expr), #expr, __LINE__)

// Whether `r` failed with `status`; prints what happened otherwise.
template <class R>
static bool fails_with(const R &r, tt::Status status, int line) {
    if (r) {
        std::fprintf(stderr, "%s:%d: expected %s, got success\n", __FILE__, line,
                     std::string(tt::status_name(status)).c_str());
        return false;
    }
    if (r.error().status() != status) {
        std::fprintf(stderr, "%s:%d: expected %s, got %s\n", __FILE__, line,
                     std::string(tt::status_name(status)).c_str(), r.error().to_string().c_str());
        return false;
    }
    return true;
}
#define CHECK_FAILS(expr, status) CHECK(fails_with((expr), (status), __LINE__))

static tt::WriterOptions small_segments() {
    tt::WriterOptions options;
    options.segment_size(65536).prefault(false);
    return options;
}

static std::string text(const tt::RecordView &r) { return std::string(r.as_string_view()); }

// ---- The fixture pattern ----

static std::size_t pattern_len(std::uint64_t seq) { return static_cast<std::size_t>(seq * 37u % 300u); }
static std::uint8_t pattern_byte(std::uint64_t seq, std::size_t i) {
    return static_cast<std::uint8_t>((seq + 7u * i) & 0xFFu);
}
static void pattern_fill(std::uint64_t seq, std::uint8_t *buf, std::size_t len) {
    for (std::size_t i = 0; i < len; i++) buf[i] = pattern_byte(seq, i);
}
static std::uint32_t pattern_schema(std::uint64_t seq) { return static_cast<std::uint32_t>(seq % 5u + 1u); }

static bool pattern_matches(const tt::RecordView &r, std::uint64_t seq) {
    if (r.seq() != seq || r.schema_id() != pattern_schema(seq) || r.venue_ts_ns() != 1000000u + seq ||
        r.local_ts_ns() != 2000000u + 3u * seq || r.size() != pattern_len(seq)) {
        std::fprintf(stderr, "record %llu: metadata mismatch\n", static_cast<unsigned long long>(seq));
        return false;
    }
    if ((r.size() == 0) != (r.data() == nullptr)) {
        std::fprintf(stderr, "record %llu: data() must be null exactly when size() is 0\n",
                     static_cast<unsigned long long>(seq));
        return false;
    }
    for (std::size_t i = 0; i < r.size(); i++) {
        if (r.data()[i] != pattern_byte(seq, i)) {
            std::fprintf(stderr, "record %llu: payload byte %zu differs\n", static_cast<unsigned long long>(seq), i);
            return false;
        }
    }
    return true;
}

// ---- Status and reason names ----

// Every Status enumerator. No default: adding an enumerator without listing it here fails -Wswitch.
static bool is_named(tt::Status status) {
    switch (status) {
        case tt::Status::ok:
        case tt::Status::none:
        case tt::Status::invalid_argument:
        case tt::Status::io:
        case tt::Status::storage_full:
        case tt::Status::locked:
        case tt::Status::corrupt:
        case tt::Status::unsupported_version:
        case tt::Status::not_found:
        case tt::Status::seq_unavailable:
        case tt::Status::record_too_large:
        case tt::Status::durability_lost:
        case tt::Status::sequence_exhausted:
        case tt::Status::discarded:
        case tt::Status::invalid_config:
        case tt::Status::buffer_too_small:
        case tt::Status::aborted:
        case tt::Status::busy:
        case tt::Status::panic:
        case tt::Status::beacon_in_use:
        case tt::Status::unknown:
            return true;
    }
    return false;
}

static bool is_named(tt::CorruptReason reason) {
    switch (reason) {
        case tt::CorruptReason::none:
        case tt::CorruptReason::bad_magic:
        case tt::CorruptReason::bad_segment_header:
        case tt::CorruptReason::size_mismatch:
        case tt::CorruptReason::base_seq_mismatch:
        case tt::CorruptReason::bad_offset:
        case tt::CorruptReason::sequence_exhausted:
        case tt::CorruptReason::bad_length:
        case tt::CorruptReason::bad_checksum:
        case tt::CorruptReason::bad_sequence:
        case tt::CorruptReason::bad_flags:
        case tt::CorruptReason::bad_seal:
        case tt::CorruptReason::broken_chain:
        case tt::CorruptReason::dangling_checkpoint:
        case tt::CorruptReason::empty_commit_word:
        case tt::CorruptReason::unchecked_tail:
        case tt::CorruptReason::unknown:
            return true;
    }
    return false;
}

// The library names exactly the codes the header names, so no status or reason is left unmapped.
static void names_match_the_library() {
    for (std::int32_t code = -300; code <= 300; code++) {
        const std::string c_name = tickertape_status_name(code);
        const bool library_knows = c_name != "TICKERTAPE_UNKNOWN_STATUS";
        const tt::Status status = static_cast<tt::Status>(code);
        if (library_knows != is_named(status)) {
            std::fprintf(stderr, "status %d: library name %s, header %s\n", code, c_name.c_str(),
                         is_named(status) ? "names it" : "does not name it");
            failures++;
        }
        CHECK(tt::status_name(status) == c_name);
    }
    for (std::int32_t code = -10; code <= 300; code++) {
        const std::string c_name = tickertape_corrupt_reason_name(code);
        const bool library_knows = c_name != "TICKERTAPE_CORRUPT_UNKNOWN" || code == TICKERTAPE_CORRUPT_UNKNOWN;
        const tt::CorruptReason reason = static_cast<tt::CorruptReason>(code);
        if (library_knows != is_named(reason)) {
            std::fprintf(stderr, "corrupt reason %d: library name %s\n", code, c_name.c_str());
            failures++;
        }
        CHECK(tt::corrupt_reason_name(reason) == c_name);
    }
}

static void versions() {
    CHECK(tt::library_compatible());
    MUST(tt::check_library());
    CHECK(tt::library_abi_version() == static_cast<std::uint32_t>(TICKERTAPE_ABI_VERSION));
    CHECK(tt::library_version_number() == static_cast<std::uint32_t>(TICKERTAPE_HPP_VERSION_NUMBER));
    const std::string expected = std::to_string(TICKERTAPE_HPP_VERSION_MAJOR) + "." +
                                 std::to_string(TICKERTAPE_HPP_VERSION_MINOR) + "." +
                                 std::to_string(TICKERTAPE_HPP_VERSION_PATCH);
    CHECK(tt::library_version() == expected);
    CHECK(tt::realtime_ns() > 1600000000ull * 1000000000ull);
}

// ---- Exact duration conversion (ADR 0036 -> Timeouts) ----

// Periods whose num * den (in nanoseconds) does not fit 64 bits, so the conversion needs a 128-bit product or the
// header's 64-bit long multiplication. One tick of the first is just over 1 ns.
using JustOverOneNs = std::ratio<1000000000, 999999999999999999>;  // 10^18 / (10^18 - 1) ns
using WideRatio = std::ratio<999999937, 1000000007>;              // 999999937 * 10^9 / 1000000007 ns

#if defined(__SIZEOF_INT128__)
__extension__ typedef unsigned __int128 u128;

// ceil(count * num / den) nanoseconds, saturated at the largest Int: the exact answer, computed independently.
template <class Int, class Period>
static std::uint64_t exact_ns(std::uint64_t count) {
    using R = std::ratio_divide<Period, std::nano>;
    const u128 product = static_cast<u128>(count) * static_cast<u128>(R::num);
    const u128 den = static_cast<u128>(R::den);
    const u128 ceil = product / den + (product % den != 0 ? 1 : 0);
    const u128 cap = static_cast<u128>(std::numeric_limits<Int>::max());
    return static_cast<std::uint64_t>(ceil > cap ? cap : ceil);
}

// Both conversions (Timeout, saturating at INT64_MAX; duration_ns, at UINT64_MAX) give the exact answer for `count`.
template <class Rep, class Period>
static bool converts_exactly(Rep count) {
    const std::chrono::duration<Rep, Period> d(count);
    const std::uint64_t c = static_cast<std::uint64_t>(count);
    return static_cast<std::uint64_t>(tt::Timeout(d).c_value()) == exact_ns<std::int64_t, Period>(c) &&
           tt::detail::duration_ns(d) == exact_ns<std::uint64_t, Period>(c);
}
#endif

static void exact_durations() {
    // One tick of just over 1 ns is 2 ns (a 53-bit long double, as on Apple arm64, gave 1).
    static_assert(tt::Timeout(std::chrono::duration<int, JustOverOneNs>(1)).c_value() == 2, "just over 1 ns");
    static_assert(tt::detail::duration_ns(std::chrono::duration<int, JustOverOneNs>(1)) == 2, "just over 1 ns");
    static_assert(tt::Timeout(std::chrono::duration<std::int64_t, WideRatio>(1)).c_value() == 999999931,
                  "one wide tick: ceil(999999937000000000 / 1000000007)");
    // The 64-bit method on its own (the header uses it where there is no 128-bit type).
    static_assert(tt::detail::mul_div_ceil_portable(1, 1000000000000000000, 999999999999999999) == 2, "portable");
    static_assert(tt::detail::mul_div_ceil_portable(1000000006, 999999937000000000, 1000000007) ==
                      999999936000000070,
                  "portable, counts below den");
    static_assert(tt::detail::mul_div_ceil_portable(0, 5, 7) == 0 && tt::detail::mul_div_ceil_portable(6, 7, 7) == 6,
                  "portable, exact quotients");
    static_assert(tt::detail::mul_div_ceil_portable(INTMAX_MAX - 1, INTMAX_MAX, INTMAX_MAX) == INTMAX_MAX - 1,
                  "portable, the widest operands");
#if defined(__SIZEOF_INT128__)
    // Every count just below 1000000007 (where 986 of 1999 came out low through long double), and its neighbours.
    int wrong = 0;
    for (std::int64_t count = 1000000007 - 1999; count <= 1000000007 + 1999; count++) {
        if (!converts_exactly<std::int64_t, WideRatio>(count)) wrong++;
    }
    CHECK(wrong == 0);
    // The portable method against the 128-bit product, over a spread of operands (a < d <= INTMAX_MAX).
    std::uint64_t x = 0x9E3779B97F4A7C15u;
    int portable_wrong = 0;
    for (int i = 0; i < 20000; i++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        const std::uint64_t d = (x >> 1) | 1u;
        const std::uint64_t b = (x * 0x2545F4914F6CDD1Du) >> 1;
        const std::uint64_t a = (x >> (i % 63)) % d;
        const u128 product = static_cast<u128>(a) * b;
        const std::uint64_t expected = static_cast<std::uint64_t>(product / d + (product % d != 0 ? 1 : 0));
        if (tt::detail::mul_div_ceil_portable(a, b, d) != expected || tt::detail::mul_div_ceil(a, b, d) != expected) {
            portable_wrong++;
        }
    }
    CHECK(portable_wrong == 0);
    // Just below, at and above each cap, for the wide periods: exact, then saturated.
    for (std::int64_t delta = -3; delta <= 3; delta++) {
        // INT64_MAX ns is about 9223372036854775807 * 1000000007 / 999999937000000000 = 9.22e9 wide ticks.
        const std::int64_t below_int64 = 9223372682 + delta;
        CHECK((converts_exactly<std::int64_t, WideRatio>(below_int64)));
        // UINT64_MAX ns is about 1.84e10 wide ticks.
        const std::uint64_t below_uint64 = static_cast<std::uint64_t>(18446745364 + delta);
        CHECK((converts_exactly<std::uint64_t, WideRatio>(below_uint64)));
        CHECK((converts_exactly<std::int64_t, JustOverOneNs>(INT64_MAX - 12 + delta)));
        const std::uint64_t near_uint64_max = UINT64_MAX - 22 + static_cast<std::uint64_t>(delta + 3);
        CHECK((converts_exactly<std::uint64_t, JustOverOneNs>(near_uint64_max)));
    }
    CHECK((converts_exactly<std::int64_t, WideRatio>(INT64_MAX)));
    CHECK((converts_exactly<int, JustOverOneNs>(INT_MAX)));
    // The periods that already worked.
    CHECK((converts_exactly<std::int64_t, std::milli>(123456789)));
    CHECK((converts_exactly<std::int64_t, std::ratio<60>>(153722867)));   // minutes, just below 2^63 ns
    CHECK((converts_exactly<std::int64_t, std::ratio<60>>(153722868)));   // and just above
    CHECK((converts_exactly<std::int64_t, std::pico>(2001)));
    CHECK((converts_exactly<std::int64_t, std::ratio<1, 3000000000>>(1)));  // a third of a nanosecond
    CHECK((converts_exactly<std::uint8_t, std::ratio<1>>(255)));          // uint8_t seconds
    CHECK((converts_exactly<std::int64_t, std::nano>(INT64_MAX)));
    CHECK((converts_exactly<std::int64_t, std::ratio<3600>>(std::chrono::hours::max().count())));
#endif
    // A positive duration never becomes 0.
    static_assert(tt::Timeout(std::chrono::duration<std::int64_t, std::ratio<1, 3000000000>>(1)).c_value() == 1,
                  "a third of a nanosecond");
    static_assert(tt::Timeout(std::chrono::duration<std::uint8_t>(255)).c_value() == 255000000000, "uint8_t seconds");
    // Floating point is unchanged: NaN does not wait, infinities saturate or do not wait.
    CHECK(tt::Timeout(std::chrono::duration<double>(std::numeric_limits<double>::quiet_NaN())).c_value() == 0);
    CHECK(tt::Timeout(std::chrono::duration<double>(std::numeric_limits<double>::infinity())).c_value() == INT64_MAX);
    CHECK(tt::Timeout(std::chrono::duration<double>(-std::numeric_limits<double>::infinity())).c_value() == 0);
    CHECK(tt::detail::duration_ns(std::chrono::duration<double>(std::numeric_limits<double>::infinity())) ==
          UINT64_MAX);
}

static void timeouts() {
    static_assert(tt::Timeout().c_value() == 0, "default is no wait");
    static_assert(tt::no_wait.c_value() == 0, "no_wait");
    static_assert(tt::Timeout(tt::wait_forever).c_value() == TICKERTAPE_WAIT_FOREVER, "forever");
    static_assert(tt::Timeout(std::chrono::milliseconds(-5)).c_value() == 0, "a passed deadline does not wait");
    static_assert(tt::Timeout(std::chrono::milliseconds(3)).c_value() == 3000000, "milliseconds");
    static_assert(tt::Timeout(std::chrono::hours::max()).c_value() == INT64_MAX, "saturates");
    // Integral counts are exact up to INT64_MAX nanoseconds; only longer durations saturate.
    using std::chrono::nanoseconds;
    using std::chrono::seconds;
    static_assert(tt::Timeout(nanoseconds(1)).c_value() == 1, "1 ns");
    static_assert(tt::Timeout(nanoseconds(9'200'000'000'000'000'000)).c_value() == 9'200'000'000'000'000'000,
                  "a representable count is not saturated early");
    static_assert(tt::Timeout(nanoseconds(INT64_MAX - 1)).c_value() == INT64_MAX - 1, "just below the cap");
    static_assert(tt::Timeout(nanoseconds::max()).c_value() == INT64_MAX, "the cap itself");
    static_assert(tt::Timeout(seconds(9'223'372'036)).c_value() == 9'223'372'036'000'000'000, "seconds below the cap");
    static_assert(tt::Timeout(seconds(9'223'372'037)).c_value() == INT64_MAX, "seconds above the cap");
    static_assert(tt::Timeout(std::chrono::duration<std::uint64_t, std::nano>(UINT64_MAX)).c_value() == INT64_MAX,
                  "an unsigned count above the cap");
    static_assert(tt::Timeout(std::chrono::duration<std::int64_t, std::pico>(1)).c_value() == 1,
                  "a sub-nanosecond count rounds up");
    static_assert(tt::Timeout(std::chrono::duration<std::int64_t, std::pico>(2001)).c_value() == 3, "rounds up");
    static_assert(tt::Timeout(std::chrono::duration<std::int64_t, std::ratio<1, 3>>(1)).c_value() == 333333334,
                  "a third of a second rounds up");
    // Floating point: compared with 2^63 (the first value an int64 cannot hold) before the cast.
    static_assert(tt::Timeout(std::chrono::duration<double, std::nano>(0.25)).c_value() == 1, "sub-nanosecond");
    static_assert(tt::Timeout(std::chrono::duration<double, std::nano>(0x1p63 - 1024)).c_value() ==
                      9'223'372'036'854'774'784,
                  "the largest double below 2^63");
    static_assert(tt::Timeout(std::chrono::duration<double, std::nano>(0x1p63)).c_value() == INT64_MAX, "2^63");
    static_assert(tt::Timeout(std::chrono::duration<double>(1e300)).c_value() == INT64_MAX, "huge");
    CHECK(tt::Timeout(std::chrono::duration<double, std::nano>(1.5)).c_value() == 2);  // rounded up
    CHECK(tt::Timeout(std::chrono::duration<double>(std::nan(""))).c_value() == 0);
    CHECK(tt::Timeout(std::chrono::duration<double>(-1.0)).c_value() == 0);
    CHECK(tt::Timeout(tt::wait_forever).is_forever());
}

// ---- The whole surface ----

static void surface(const std::string &root) {
    const std::string dir = root + "/journal";
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    CHECK(writer.is_open() && static_cast<bool>(writer));
    CHECK_FAILS(tt::Writer::open(dir), tt::Status::locked);  // one writer per journal
    {
        auto second = tt::Writer::open(dir);
        CHECK(!second && second.error().to_string().find("TICKERTAPE_E_LOCKED") == 0);
    }

    // Every kind of payload.
    CHECK(MUST(writer.next_seq()) == 0);
    CHECK(MUST(writer.append(1, 10, 20, std::string_view("view"))) == 0);
    CHECK(MUST(writer.append(1, 10, 20, std::string("string"))) == 1);
    const std::vector<std::uint8_t> bytes{'v', 'e', 'c'};
    CHECK(MUST(writer.append(2, 0, 0, bytes)) == 2);
    const std::array<std::byte, 2> raw{std::byte{'a'}, std::byte{'b'}};
    CHECK(MUST(writer.append(2, 0, 0, raw)) == 3);
    CHECK(MUST(writer.append(3, 0, 0, "pointer", 7)) == 4);
    CHECK(MUST(writer.append(3, 0, 0, nullptr, 0)) == 5);  // empty
    // Zero-copy: void commits, false aborts.
    CHECK(MUST(writer.append_with(4, 1, 2, 9, [](std::uint8_t *buf, std::size_t len) {
              std::memcpy(buf, "zero-copy", len);
          })) == 6);
    CHECK(MUST(writer.append_with(4, 1, 2, 0, [](std::uint8_t *buf, std::size_t len) {
              CHECK(buf == nullptr && len == 0);
              return true;
          })) == 7);
    bool zeroed = false;
    CHECK_FAILS(writer.append_with(4, 0, 0, 16,
                                   [&](std::uint8_t *buf, std::size_t len) {
                                       zeroed = std::all_of(buf, buf + len, [](std::uint8_t b) { return b == 0; });
                                       return false;
                                   }),
                tt::Status::aborted);
    CHECK(zeroed);
    CHECK(MUST(writer.next_seq()) == 8);
    // Batches: from a vector, and an argument error that appends nothing.
    std::vector<std::string> texts;
    for (int i = 0; i < 10; i++) texts.push_back("batch " + std::to_string(i));
    std::vector<tt::NewRecord> batch;
    for (const std::string &t : texts) batch.push_back(tt::new_record(5, 100, 200, t));
    CHECK(batch[3].schema_id == 5 && batch[3].venue_ts_ns == 100 && batch[3].local_ts_ns == 200);
    CHECK(batch[3].payload == reinterpret_cast<const std::uint8_t *>(texts[3].data()) && batch[3].payload_len == 7);
    {
        tt::BatchAppend appended = writer.append_batch(batch);
        MUST(appended.outcome);
        CHECK(appended.first_seq == 8 && appended.appended == 10);
    }
    {
        const std::array<tt::NewRecord, 2> bad{tt::new_record(5, 0, 0, std::string_view("fine")),
                                               tt::new_record(5, 0, 0, nullptr, 3)};
        tt::BatchAppend appended = writer.append_batch(bad);
        CHECK_FAILS(appended.outcome, tt::Status::invalid_argument);
        CHECK(appended.appended == 0);
    }
    CHECK(MUST(writer.next_seq()) == 18);
    CHECK_FAILS(writer.append(1, 0, 0, std::vector<char>(70000)), tt::Status::record_too_large);
    CHECK(!writer.last_retention_error().has_value());

    // Durability: nothing is covered until the checkpoint; later records are visible, not durable.
    CHECK(!MUST(tt::read_checkpoint(dir)).has_value());
    MUST(writer.checkpoint());
    MUST(writer.append(9, 0, 0, std::string_view("late")));
    {
        const std::optional<tt::Checkpoint> checkpoint = MUST(tt::read_checkpoint(dir));
        CHECK(checkpoint.has_value() && checkpoint->next_seq == 18 && checkpoint->generation >= 1);
    }

    // The safe reader, opened through every path type, including temporaries, which live until the call returns.
    tt::Reader reader = MUST(tt::Reader::open(fs::path(dir)));
    MUST(tt::Reader::open(std::string_view(dir)).value().close());
    MUST(tt::Reader::open(dir.c_str()).value().close());
    MUST(tt::Reader::open(fs::path(root) / "journal").value().close());
    MUST(tt::Reader::open(root + "/journal").value().close());
    CHECK(MUST(tt::list_segments(fs::path(root) / "journal")).size() == 1);
    const char *expected[] = {"view", "string", "vec", "ab", "pointer", "", "zero-copy", ""};
    for (std::uint64_t s = 0; s < 8; s++) {
        std::optional<tt::RecordView> r = MUST(reader.next());
        CHECK(r.has_value() && r->seq() == s && text(*r) == expected[s]);
        if (s == 0) CHECK(r->schema_id() == 1 && r->venue_ts_ns() == 10 && r->local_ts_ns() == 20);
        if (s == 5) CHECK(r->empty() && r->data() == nullptr && r->size() == 0);
    }
    CHECK(MUST(reader.position()) == 8);
    // A batch with room for 4 records and 20 bytes: "batch 0".."batch 1" (7 bytes each) fit, the third is held back.
    tt::RecordBatch records(4, 20);
    CHECK(records.capacity() == 4 && records.buffer_size() == 20 && records.empty());
    CHECK(MUST(reader.read_batch(records)) == 2);
    {
        std::uint64_t s = 8;
        for (tt::RecordView r : records) {
            CHECK(r.seq() == s && text(r) == "batch " + std::to_string(s - 8));
            s++;
        }
        CHECK(s == 10 && records[1].seq() == 9);
    }
    tt::RecordBatch tiny(4, 3);
    {
        auto none_fits = reader.read_batch(tiny);
        CHECK_FAILS(none_fits, tt::Status::buffer_too_small);
        CHECK(!none_fits && none_fits.error().needed_size() == 7 && tiny.empty());
    }
    // next_copy: too small, then into a buffer of our own (the held-back record comes first).
    std::array<std::uint8_t, 64> buffer{};
    {
        auto small = reader.next_copy(buffer.data(), 2);
        CHECK_FAILS(small, tt::Status::buffer_too_small);
        CHECK(!small && small.error().needed_size() == 7);
        std::optional<tt::RecordView> copied = MUST(reader.next_copy(buffer.data(), buffer.size()));
        CHECK(copied && copied->seq() == 10 && copied->data() == buffer.data() && text(*copied) == "batch 2");
        // Untyped memory and writable byte ranges.
        char chars[16];
        copied = MUST(reader.next_copy(chars, sizeof chars));
        CHECK(copied && copied->seq() == 11 && copied->data() == reinterpret_cast<std::uint8_t *>(chars));
        std::array<std::byte, 16> bytes_out{};
        copied = MUST(reader.next_copy(bytes_out));
        CHECK(copied && copied->seq() == 12 && copied->data() == reinterpret_cast<std::uint8_t *>(bytes_out.data()) &&
              text(*copied) == "batch 4");
    }
    // Seek back, and the end of the journal with each kind of timeout.
    MUST(reader.seek(17));
    CHECK(text(*MUST(reader.next(tt::wait_forever))) == "batch 9");  // available: returns at once
    CHECK(text(*MUST(reader.next(1s))) == "late");
    const auto before = std::chrono::steady_clock::now();
    CHECK(!MUST(reader.next(-1h)).has_value());  // a passed deadline does not wait (C: negative waits forever)
    CHECK(!MUST(reader.next(2ms)).has_value());
    CHECK(std::chrono::steady_clock::now() - before < 30s);
    CHECK(MUST(reader.position()) == 19);

    // Durable-only, on the mapped reader: stops at the checkpoint.
    {
        tt::ReaderOptions durable;
        durable.durable_only(true).notify(false).backoff(10, 2, 1us, 100us);
        tt::Reader mapped = MUST(tt::Reader::open_mapped_unsafe(dir, durable));
        MUST(mapped.seek(15));
        std::uint64_t last = 0;
        while (std::optional<tt::RecordView> r = MUST(mapped.next())) last = r->seq();
        CHECK(last == 17 && MUST(mapped.position()) == 18);
        tt::ReaderOptions spin;
        spin.busy_spin();
        tt::Reader spinning = MUST(tt::Reader::open(dir, spin));
        CHECK(MUST(spinning.next())->seq() == 0);
    }

    // Inspection.
    {
        const tt::VerifyReport report = MUST(tt::verify(dir));
        CHECK(report.records == 19 && report.problems == 0 && !report.first_problem);
        CHECK(report.first_seq == std::optional<std::uint64_t>(0));
        CHECK(report.last_seq == std::optional<std::uint64_t>(18));
        const std::vector<tt::SegmentInfo> segments = MUST(tt::list_segments(dir));
        CHECK(segments.size() == 1 && segments[0].base_seq == 0 && segments[0].segment_size == 65536 &&
              segments[0].format_version == 1 && segments[0].created_unix_ns > 0);
    }
    MUST(writer.close());
    CHECK(!writer.is_open());
    MUST(writer.close());  // closing an empty writer does nothing
}

// Retention, list_segments across rolls, seq_unavailable.
static void retention(const std::string &root) {
    const std::string dir = root + "/retention";
    const std::string archive = root + "/archive";
    fs::create_directories(archive);
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    const std::vector<char> payload(20000, 'x');
    for (int i = 0; i < 20; i++) MUST(writer.append(1, 0, 0, payload));
    MUST(writer.checkpoint());
    MUST(writer.append(1, 0, 0, payload));
    const std::vector<tt::SegmentInfo> before = MUST(tt::list_segments(dir));
    CHECK(before.size() >= 4);

    tt::Retention no_action;
    no_action.max_segments(1);
    CHECK_FAILS(tt::apply_retention(dir, no_action), tt::Status::invalid_argument);  // no default action
    const tt::RetentionReport planned = MUST(tt::plan_retention(dir, no_action));
    CHECK(planned.removed_segments >= 2 && planned.removed_bytes == planned.removed_segments * 65536);
    CHECK(MUST(tt::list_segments(dir)).size() == before.size());  // planning changes nothing

    // A negative or NaN age is an error, never "every sealed segment is old enough" (which would remove them all).
    for (int which = 0; which < 3; which++) {
        tt::Retention bad;
        if (which == 0) bad.max_age(-std::chrono::hours(1));
        if (which == 1) bad.max_age(std::chrono::duration<double>(std::nan("")));
        if (which == 2) bad.max_age(std::chrono::duration<double, std::nano>(-0.5));
        bad.delete_segments();
        CHECK(bad.error() && bad.error()->status() == tt::Status::invalid_argument);
        CHECK_FAILS(tt::plan_retention(dir, bad), tt::Status::invalid_argument);
        CHECK_FAILS(tt::apply_retention(dir, bad), tt::Status::invalid_argument);
        tt::WriterOptions with_bad_age;
        with_bad_age.retention(bad);
        CHECK(with_bad_age.error() && with_bad_age.error()->status() == tt::Status::invalid_argument);
    }
    CHECK(MUST(tt::list_segments(dir)).size() == before.size());
    // Ages round up to whole nanoseconds (towards keeping), and zero stays a valid, explicit choice.
    CHECK(tt::detail::duration_ns(std::chrono::duration<double, std::nano>(0.25)) == 1);
    CHECK(tt::detail::duration_ns(std::chrono::duration<double, std::nano>(2.5)) == 3);
    CHECK(tt::detail::duration_ns(std::chrono::seconds(2)) == 2000000000u);
    // Exact up to UINT64_MAX nanoseconds, then saturated; floating point compared with 2^64 before the cast.
    static_assert(tt::detail::duration_ns(std::chrono::nanoseconds(1)) == 1, "1 ns");
    static_assert(tt::detail::duration_ns(std::chrono::nanoseconds::max()) == INT64_MAX, "int64 counts are exact");
    static_assert(tt::detail::duration_ns(std::chrono::duration<std::uint64_t, std::nano>(UINT64_MAX - 1)) ==
                      UINT64_MAX - 1,
                  "just below the cap");
    static_assert(tt::detail::duration_ns(std::chrono::duration<std::uint64_t, std::micro>(18'446'744'073'709'551)) ==
                      18'446'744'073'709'551'000u,
                  "microseconds below the cap");
    static_assert(tt::detail::duration_ns(std::chrono::duration<std::uint64_t, std::micro>(18'446'744'073'709'552)) ==
                      UINT64_MAX,
                  "microseconds above the cap");
    static_assert(tt::detail::duration_ns(std::chrono::duration<double, std::nano>(0x1p64 - 2048)) ==
                      18'446'744'073'709'549'568u,
                  "the largest double below 2^64");
    static_assert(tt::detail::duration_ns(std::chrono::duration<double, std::nano>(1.8e19)) ==
                      18'000'000'000'000'000'000u,
                  "not saturated early");
    static_assert(tt::detail::duration_ns(std::chrono::duration<double, std::nano>(0x1p64)) == UINT64_MAX, "2^64");
    {
        tt::Retention zero;
        zero.max_age(std::chrono::seconds(0));
        CHECK(!zero.error());
    }

    // A C setter the binding does not wrap can be called through the non-const native handle.
    {
        tt::Retention raw;
        CHECK(tickertape_retention_set_max_segments(raw.native_handle(), 1) == TICKERTAPE_OK);
        CHECK(MUST(tt::plan_retention(dir, raw)).removed_segments == planned.removed_segments);
        tt::WriterOptions writer_options;
        CHECK(tickertape_writer_options_set_prefault(writer_options.native_handle(), 0) == TICKERTAPE_OK);
        tt::ReaderOptions reader_options;
        CHECK(tickertape_reader_options_set_notify(reader_options.native_handle(), 0) == TICKERTAPE_OK);
    }

    tt::Retention move;
    move.max_segments(1).max_bytes(UINT64_MAX).max_age(std::chrono::hours(24 * 365)).move_to(fs::path(archive));
    const tt::RetentionReport moved = MUST(tt::apply_retention(dir, move));
    CHECK(moved.removed_segments == planned.removed_segments && moved.already_removed_segments == 0);
    CHECK(MUST(tt::list_segments(dir)).size() == before.size() - moved.removed_segments);
    CHECK(static_cast<std::size_t>(std::distance(fs::directory_iterator(archive), fs::directory_iterator())) ==
          moved.removed_segments);

    tt::Reader reader = MUST(tt::Reader::open(dir));
    CHECK_FAILS(reader.seek(0), tt::Status::seq_unavailable);
    const std::uint64_t oldest = MUST(tt::list_segments(dir))[0].base_seq;
    MUST(reader.seek(oldest));
    CHECK(MUST(reader.next())->seq() == oldest);

    // Writer retention: errors never fail the writer; without an action the options refuse it.
    tt::Retention remove;
    remove.max_segments(2).delete_segments();
    tt::WriterOptions with_retention = small_segments();
    with_retention.retention(remove);
    writer = tt::Writer();  // closes the first writer
    tt::Writer retaining = MUST(tt::Writer::open(dir, with_retention));
    for (int i = 0; i < 8; i++) MUST(retaining.append(1, 0, 0, payload));
    MUST(retaining.checkpoint());
    CHECK(!retaining.last_retention_error());
    CHECK(MUST(tt::list_segments(dir)).size() <= 3);
    MUST(retaining.close());
    tt::WriterOptions refused = small_segments();
    refused.retention(no_action);
    CHECK(refused.error().has_value());
    CHECK_FAILS(tt::Writer::open(dir, refused), tt::Status::invalid_argument);
    refused.no_retention();  // the first error stays
    CHECK_FAILS(tt::Writer::open(dir, refused), tt::Status::invalid_argument);
}

// Errors from the library and from the binding, with their details.
static void errors(const std::string &root) {
    {
        auto missing = tt::Reader::open(root + "/does-not-exist");
        CHECK_FAILS(missing, tt::Status::io);
        CHECK(!missing && missing.error().os_code() == ENOENT && !missing.error().message().empty());
        CHECK(!missing && missing.error().corrupt_reason() == tt::CorruptReason::none);
    }
    fs::create_directories(root + "/empty");
    CHECK_FAILS(tt::Reader::open(root + "/empty"), tt::Status::not_found);
    CHECK_FAILS(tt::verify(root + "/empty"), tt::Status::not_found);
    CHECK_FAILS(tt::list_segments(root + "/empty"), tt::Status::not_found);
    {
        tt::WriterOptions options;
        options.segment_size(1000);
        CHECK(!options.error());  // checked by open
        CHECK_FAILS(tt::Writer::open(root + "/bad-config", options), tt::Status::invalid_config);
    }
    {
        tt::WriterOptions options;
        options.recovery_policy(static_cast<tt::RecoveryPolicy>(7)).recovery_policy(tt::RecoveryPolicy::fail);
        CHECK(options.error() && options.error()->status() == tt::Status::invalid_argument);
        CHECK_FAILS(tt::Writer::open(root + "/bad-policy", options), tt::Status::invalid_argument);
        tt::ReaderOptions backoff;
        backoff.backoff(1, 1, 2ms, 1ms);
        CHECK_FAILS(tt::Reader::open(root + "/empty", backoff), tt::Status::invalid_argument);
    }
    // A NUL inside a path is refused before it reaches the C ABI, which would truncate it.
    const std::string with_nul = root + std::string("/a\0b", 4);
    CHECK_FAILS(tt::Writer::open(with_nul), tt::Status::invalid_argument);
    CHECK_FAILS(tt::Reader::open(std::string_view(with_nul)), tt::Status::invalid_argument);
    CHECK_FAILS(tt::verify(fs::path(with_nul)), tt::Status::invalid_argument);
    {
        tt::Retention retention;
        retention.move_to(with_nul).delete_segments();
        CHECK(retention.error() && retention.error()->status() == tt::Status::invalid_argument);
    }
    CHECK_FAILS(tt::Writer::open(static_cast<const char *>(nullptr)), tt::Status::invalid_argument);

    // Corruption, with its reason, from a reader and from verify.
    const std::string dir = root + "/corrupt";
    {
        tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
        MUST(writer.append(1, 0, 0, std::string_view("first record")));
        MUST(writer.append(1, 0, 0, std::string_view("FLIP-THIS-PAYLOAD")));
    }
    const fs::path segment = fs::path(dir) / "00000000000000000000.tape";
    {
        std::fstream file(segment, std::ios::in | std::ios::out | std::ios::binary);
        std::string contents((std::istreambuf_iterator<char>(file)), std::istreambuf_iterator<char>());
        const std::size_t at = contents.find("FLIP-THIS-PAYLOAD");
        CHECK(at != std::string::npos);
        file.seekp(static_cast<std::streamoff>(at));
        file.put('f');
    }
    tt::Reader reader = MUST(tt::Reader::open(dir));
    CHECK(text(*MUST(reader.next())) == "first record");
    for (int attempt = 0; attempt < 2; attempt++) {  // the cursor does not move: the error repeats
        auto bad = reader.next();
        CHECK_FAILS(bad, tt::Status::corrupt);
        CHECK(!bad && bad.error().corrupt_reason() == tt::CorruptReason::bad_checksum && bad.error().os_code() == 0);
    }
    const tt::VerifyReport report = MUST(tt::verify(dir));
    CHECK(report.problems == 1 && report.records == 1 && report.first_problem.has_value());
    CHECK(report.first_problem && report.first_problem->reason == tt::CorruptReason::bad_checksum &&
          report.first_problem->segment_base_seq == 0 && report.first_problem->offset > 0);

    // A library error built by hand keeps what it was given.
    const tt::Error error(tt::Status::storage_full, "disk full", 28);
    CHECK(error.status() == tt::Status::storage_full && error.os_code() == 28);
    CHECK(error.name() == "TICKERTAPE_E_STORAGE_FULL");
    CHECK(error.to_string() == "TICKERTAPE_E_STORAGE_FULL: disk full");
    // The message of a temporary error is returned by value: binding it to a reference keeps it alive.
    const auto failing = [] { return tt::result<void>(tt::Error(tt::Status::busy, "a message too long for SSO")); };
    const std::string &message = failing().error().message();
    CHECK(message == "a message too long for SSO");
}

// What a fill callback may do to its own writer.
static void fill_callbacks(const std::string &root) {
    const std::string dir = root + "/fill";
    const auto lock_is_free = [&]() {
        auto again = tt::Writer::open(dir, small_segments());
        return again.has_value();
    };

    // Calls on the same writer from inside its fill are refused with busy, and the writer is fine afterwards.
    {
        tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
        std::optional<tt::Status> append, checkpoint, close, nested, next_seq;
        const std::uint64_t seq = MUST(writer.append_with(1, 0, 0, 4, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "busy", len);
            auto a = writer.append(1, 0, 0, std::string_view("x"));
            append = a ? tt::Status::ok : a.error().status();
            auto c = writer.checkpoint();
            checkpoint = c ? tt::Status::ok : c.error().status();
            auto cl = writer.close();
            close = cl ? tt::Status::ok : cl.error().status();
            auto n = writer.append_with(1, 0, 0, 1, [](std::uint8_t *, std::size_t) { CHECK(false); });
            nested = n ? tt::Status::ok : n.error().status();
            auto s = writer.next_seq();
            next_seq = s ? tt::Status::ok : s.error().status();
        }));
        CHECK(seq == 0);
        CHECK(append == tt::Status::busy && checkpoint == tt::Status::busy && close == tt::Status::busy);
        CHECK(nested == tt::Status::busy && next_seq == tt::Status::busy);
        CHECK(writer.is_open());
        // The frame was removed: the next append_with runs normally.
        CHECK(MUST(writer.append_with(1, 0, 0, 1, [](std::uint8_t *buf, std::size_t) { buf[0] = 'z'; })) == 1);
        CHECK(MUST(writer.next_seq()) == 2);
    }
    CHECK(lock_is_free());

    // Destroying the writer from its own fill: the append completes, then the handle is closed.
    {
        std::optional<tt::Writer> writer(MUST(tt::Writer::open(dir, small_segments())));
        auto seq = writer->append_with(1, 0, 0, 4, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "gone", len);
            writer.reset();
        });
        CHECK(seq && *seq == 2);
        CHECK(!writer.has_value());
        CHECK(lock_is_free());
    }

    // Move-assigning an empty writer, then another journal's writer, into it from its fill.
    {
        tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
        auto seq = writer.append_with(1, 0, 0, 5, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "empty", len);
            writer = tt::Writer();
        });
        CHECK(seq && *seq == 3 && !writer.is_open());
        CHECK(lock_is_free());

        writer = MUST(tt::Writer::open(dir, small_segments()));
        auto other_seq = writer.append_with(1, 0, 0, 5, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "other", len);
            writer = MUST(tt::Writer::open(root + "/fill-other", small_segments()));
        });
        CHECK(other_seq && *other_seq == 4);
        CHECK(lock_is_free());
        CHECK(MUST(writer.append(1, 0, 0, std::string_view("into the other journal"))) == 0);
    }

    // Moving the writer out of itself from its fill: the new owner keeps the handle and can use it afterwards.
    {
        tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
        tt::Writer moved;
        auto seq = writer.append_with(1, 0, 0, 4, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "move", len);
            moved = std::move(writer);
            auto refused = moved.append(1, 0, 0, std::string_view("still inside"));
            CHECK(!refused && refused.error().status() == tt::Status::busy);
        });
        CHECK(seq && *seq == 5 && !writer.is_open() && moved.is_open());
        CHECK(MUST(moved.append_with(1, 0, 0, 1, [](std::uint8_t *buf, std::size_t) { buf[0] = 'm'; })) == 6);
        CHECK(!lock_is_free());

        // Moved into a local that dies inside the fill: closed after the call.
        tt::Writer chained(std::move(moved));
        auto local_seq = chained.append_with(1, 0, 0, 5, [&](std::uint8_t *buf, std::size_t len) {
            std::memcpy(buf, "local", len);
            tt::Writer local(std::move(chained));
        });
        CHECK(local_seq && *local_seq == 7 && !chained.is_open());
        CHECK(lock_is_free());
    }

#if defined(__cpp_exceptions)
    // A throwing fill: nothing is committed and the exception reaches the caller unchanged.
    {
        tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
        const std::uint64_t next = MUST(writer.next_seq());
        try {
            (void)writer.append_with(1, 0, 0, 4, [](std::uint8_t *, std::size_t) { throw std::out_of_range("no"); });
            CHECK(false);
        } catch (const std::out_of_range &e) {
            CHECK(std::string(e.what()) == "no");
        }
        CHECK(MUST(writer.next_seq()) == next);
        // Destroying the writer and then throwing: the handle is still closed.
        std::optional<tt::Writer> doomed(std::move(writer));
        try {
            (void)doomed->append_with(1, 0, 0, 4, [&](std::uint8_t *, std::size_t) {
                doomed.reset();
                throw std::runtime_error("after reset");
            });
            CHECK(false);
        } catch (const std::runtime_error &e) {
            CHECK(std::string(e.what()) == "after reset");
        }
        CHECK(lock_is_free());
    }
#endif

    tt::Reader reader = MUST(tt::Reader::open(dir));
    const char *expected[] = {"busy", "z", "gone", "empty", "other", "move", "m", "local"};
    for (const char *e : expected) CHECK(text(*MUST(reader.next())) == e);
    CHECK(!MUST(reader.next()).has_value());
}

// Moved-from and default-constructed objects are empty: safe to destroy and assign, and calls fail cleanly.
static void move_semantics(const std::string &root) {
    const std::string dir = root + "/moves";
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    tt::Writer taken(std::move(writer));
    CHECK(!writer.is_open() && taken.is_open());  // NOLINT(bugprone-use-after-move): testing the moved-from state
    CHECK_FAILS(writer.append(1, 0, 0, std::string_view("x")), tt::Status::invalid_argument);
    CHECK_FAILS(writer.append_with(1, 0, 0, 1, [](std::uint8_t *, std::size_t) { CHECK(false); }),
                tt::Status::invalid_argument);
    CHECK_FAILS(writer.checkpoint(), tt::Status::invalid_argument);
    CHECK_FAILS(writer.next_seq(), tt::Status::invalid_argument);
    CHECK(writer.last_retention_error().has_value());
    CHECK_FAILS(writer.append_batch(nullptr, 0).outcome, tt::Status::invalid_argument);
    MUST(writer.close());
    tt::Writer &alias = taken;
    taken = std::move(alias);  // self-move keeps the handle
    CHECK(taken.is_open());
    CHECK(MUST(taken.append(1, 0, 0, std::string_view("kept"))) == 0);
    writer = std::move(taken);  // assigning into an empty writer
    CHECK(writer.is_open() && !taken.is_open());
    MUST(writer.checkpoint());

    tt::Reader reader = MUST(tt::Reader::open(dir));
    tt::Reader other(std::move(reader));
    CHECK(!reader.is_open() && other.is_open());
    CHECK_FAILS(reader.next(), tt::Status::invalid_argument);
    CHECK_FAILS(reader.seek(0), tt::Status::invalid_argument);
    CHECK_FAILS(reader.position(), tt::Status::invalid_argument);
    tt::RecordBatch batch(2, 64);
    CHECK_FAILS(reader.read_batch(batch), tt::Status::invalid_argument);
    CHECK_FAILS(reader.next_copy(nullptr, 0), tt::Status::invalid_argument);
    MUST(reader.close());
    reader = std::move(other);
    CHECK(text(*MUST(reader.next())) == "kept");
    reader = tt::Reader();  // closes it

    tt::RecordBatch moved_batch(std::move(batch));
    CHECK(batch.capacity() == 0 && moved_batch.capacity() == 2);
    batch = std::move(moved_batch);
    CHECK(batch.capacity() == 2);

    tt::WriterOptions options = small_segments();
    tt::WriterOptions taken_options(std::move(options));
    CHECK(options.native_handle() == nullptr);
    CHECK_FAILS(tt::Writer::open(dir + "-x", options), tt::Status::invalid_argument);  // not the defaults
    options.notify(true);  // records an error instead of crashing
    CHECK(options.error().has_value());
    options = std::move(taken_options);
    CHECK(options.native_handle() != nullptr);
    tt::ReaderOptions reader_options;
    tt::ReaderOptions taken_reader_options(std::move(reader_options));
    CHECK_FAILS(tt::Reader::open(dir, reader_options), tt::Status::invalid_argument);
    reader_options = std::move(taken_reader_options);
    MUST(tt::Reader::open(dir, reader_options));
    tt::Retention retention;
    tt::Retention taken_retention(std::move(retention));
    CHECK_FAILS(tt::plan_retention(dir, retention), tt::Status::invalid_argument);
    tt::WriterOptions with_moved_retention;
    with_moved_retention.retention(retention);  // an error, not "no retention"
    CHECK(with_moved_retention.error() && with_moved_retention.error()->status() == tt::Status::invalid_argument);
    retention = std::move(taken_retention);
    MUST(tt::plan_retention(dir, retention));
}

#if defined(__cpp_exceptions)
static void exceptions(const std::string &root) {
    try {
        (void)tt::Reader::open(root + "/does-not-exist").value();
        CHECK(false);
    } catch (const tt::Exception &e) {
        CHECK(e.status() == tt::Status::io && e.error().os_code() == ENOENT);
        CHECK(std::string(e.what()).find("TICKERTAPE_E_IO: ") == 0);
    }
    // Copies of an Exception share its Error (copying never throws).
    try {
        (void)tt::result<int>(tt::Error(tt::Status::locked, "held")).value();
        CHECK(false);
    } catch (const tt::Exception &e) {
        const tt::Exception copy(e);
        CHECK(&copy.error() == &e.error() && copy.status() == tt::Status::locked);
        CHECK(std::string(copy.what()) == "TICKERTAPE_E_LOCKED: held");
    }
    // A moved-from or move-assigned-from Exception keeps its Error: moving one copies it (no null shared Error).
    {
        tt::Exception original(tt::Error(tt::Status::corrupt, "bad", 5, tt::CorruptReason::bad_magic, 7));
        const tt::Exception moved(std::move(original));
        tt::Exception assigned(tt::Error(tt::Status::busy, "other"));
        tt::Exception source(tt::Error(tt::Status::corrupt, "bad", 5, tt::CorruptReason::bad_magic, 7));
        assigned = std::move(source);
        const std::initializer_list<const tt::Exception *> all = {&original, &moved, &source, &assigned};
        for (const tt::Exception *e : all) {
            CHECK(e->status() == tt::Status::corrupt);
            CHECK(e->error().status() == tt::Status::corrupt);
            CHECK(e->error().message() == "bad");
            CHECK(e->error().os_code() == 5);
            CHECK(e->error().corrupt_reason() == tt::CorruptReason::bad_magic);
            CHECK(e->error().needed_size() == 7);
            CHECK(e->error().name() == "TICKERTAPE_E_CORRUPT");
            CHECK(e->error().to_string() == "TICKERTAPE_E_CORRUPT: bad");
            CHECK(std::string(e->what()) == "TICKERTAPE_E_CORRUPT: bad");
        }
    }
    try {
        tt::check_library().value();
        tt::result<int> failed = tt::Error(tt::Status::busy, "x");
        CHECK(failed.value_or(5) == 5);
        (void)failed.value();
        CHECK(false);
    } catch (const std::runtime_error &e) {  // Exception is a std::runtime_error
        CHECK(std::string(e.what()) == "TICKERTAPE_E_BUSY: x");
    }
}
#endif

// ---- Fixture modes ----

static const std::uint64_t fixture_records = 1500;
static const std::uint64_t fixture_checkpoint_at = 1000;

// Blocks of ten records: five single appends (copying and zero-copy in turn), then five in one batch.
static int write_fixture(const std::string &dir) {
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    std::vector<std::vector<std::uint8_t>> payloads(5);
    for (std::uint64_t block = 0; block < fixture_records; block += 10) {
        if (block == fixture_checkpoint_at) MUST(writer.checkpoint());
        for (std::uint64_t seq = block; seq < block + 5; seq++) {
            std::uint64_t got;
            if (seq % 2 == 0) {
                std::vector<std::uint8_t> payload(pattern_len(seq));
                pattern_fill(seq, payload.data(), payload.size());
                got = MUST(writer.append(pattern_schema(seq), 1000000 + seq, 2000000 + 3 * seq, payload));
            } else {
                const auto fill = [&](std::uint8_t *buf, std::size_t len) { pattern_fill(seq, buf, len); };
                got = MUST(writer.append_with(pattern_schema(seq), 1000000 + seq, 2000000 + 3 * seq, pattern_len(seq),
                                              fill));
            }
            CHECK(got == seq);
        }
        std::array<tt::NewRecord, 5> batch;
        for (std::size_t i = 0; i < 5; i++) {
            const std::uint64_t seq = block + 5 + i;
            payloads[i].resize(pattern_len(seq));
            pattern_fill(seq, payloads[i].data(), payloads[i].size());
            batch[i] = tt::new_record(pattern_schema(seq), 1000000 + seq, 2000000 + 3 * seq, payloads[i]);
        }
        tt::BatchAppend appended = writer.append_batch(batch);
        MUST(appended.outcome);
        CHECK(appended.first_seq == block + 5 && appended.appended == 5);
    }
    MUST(writer.close());  // records after the checkpoint stay visible, not durable
    return failures;
}

static int read_fixture(const std::string &dir, std::uint64_t count, std::uint64_t durable_next) {
    const std::optional<tt::Checkpoint> checkpoint = MUST(tt::read_checkpoint(dir));
    CHECK(checkpoint && checkpoint->next_seq == durable_next);
    CHECK(MUST(tt::list_segments(dir)).size() >= 2);  // sequence boundaries across a roll are covered
    const tt::VerifyReport report = MUST(tt::verify(dir));
    CHECK(report.problems == 0 && report.records == count && report.first_seq == std::optional<std::uint64_t>(0) &&
          report.last_seq == std::optional<std::uint64_t>(count - 1));

    for (int mapped = 0; mapped <= 1; mapped++) {
        tt::Reader reader = MUST(mapped ? tt::Reader::open_mapped_unsafe(dir) : tt::Reader::open(dir));
        for (std::uint64_t seq = 0; seq < count; seq++) {
            std::optional<tt::RecordView> r = MUST(reader.next());
            if (!r || !pattern_matches(*r, seq)) failures++;
        }
        CHECK(!MUST(reader.next()).has_value());
    }
    // Batches across segment boundaries, with a buffer small enough to force hold-backs.
    {
        tt::Reader reader = MUST(tt::Reader::open(dir));
        tt::RecordBatch batch(64, 2048);
        std::uint64_t expected = 0;
        while (MUST(reader.read_batch(batch)) > 0) {
            for (tt::RecordView r : batch) {
                if (!pattern_matches(r, expected)) failures++;
                expected++;
            }
        }
        CHECK(expected == count);
    }
    // Durable-only: exactly the records below the checkpoint.
    {
        tt::ReaderOptions durable;
        durable.durable_only(true);
        tt::Reader reader = MUST(tt::Reader::open_mapped_unsafe(dir, durable));
        for (std::uint64_t seq = 0; seq < durable_next; seq++) {
            std::optional<tt::RecordView> r = MUST(reader.next());
            if (!r || !pattern_matches(*r, seq)) failures++;
        }
        CHECK(!MUST(reader.next()).has_value());
        CHECK(MUST(reader.position()) == durable_next);
    }
    return failures;
}

// ---- Death modes ----

// A raw C fill callback that breaks the contract: the exception must not unwind through the library.
#if defined(__cpp_exceptions)
static std::int32_t throwing_fill(void *, std::uint8_t *, std::size_t) { throw std::runtime_error("escaped"); }
#endif

static int throw_from_fill(const std::string &dir) {
#if defined(__cpp_exceptions)
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    try {
        (void)tickertape_writer_append_with(writer.native_handle(), 1, 0, 0, 8, &throwing_fill, nullptr, nullptr);
    } catch (...) {
        std::printf("caught\n");  // must not happen: the process terminates first
        return 0;
    }
    std::printf("returned\n");
#else
    (void)dir;
    std::printf("built without exceptions\n");
#endif
    return 0;
}

// C++20: as_span of a stale view whose record is empty must still be checked (it used to return before the check).
static int stale_empty_span(const std::string &dir) {
#if __cplusplus >= 202002L && defined(__cpp_lib_span)
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    MUST(writer.append(1, 0, 0, nullptr, 0));
    MUST(writer.append(1, 0, 0, std::string_view("two")));
    tt::Reader reader = MUST(tt::Reader::open(dir));
    const tt::RecordView empty = *MUST(reader.next());
    CHECK(empty.empty() && tt::as_span(empty).empty());
    (void)MUST(reader.next());
    std::printf("about to read a stale view\n");
    std::fflush(stdout);
    std::printf("not detected: %zu\n", tt::as_span(empty).size());  // must abort here
#else
    (void)dir;
    std::printf("built without std::span\n");
#endif
    return 0;
}

static int stale_view(const std::string &dir) {
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    MUST(writer.append(1, 0, 0, std::string_view("one")));
    MUST(writer.append(1, 0, 0, std::string_view("two")));
    tt::Reader reader = MUST(tt::Reader::open(dir));
    const tt::RecordView first = *MUST(reader.next());
    CHECK(text(first) == "one");
    const tt::RecordView second = *MUST(reader.next());
    CHECK(first.seq() == 0 && second.seq() == 1);  // metadata are copies and stay readable
    std::printf("about to read a stale view\n");
    std::fflush(stdout);
    std::printf("not detected: %zu\n", first.as_string_view().size());  // must abort here
    return 0;
}

// Operational metrics (ADR 0032): a status sample, the writer's metrics and a lagging reader's.
static void metrics(const std::string &root) {
    const std::string dir = root + "/metrics";
    tt::Writer writer = MUST(tt::Writer::open(dir, small_segments()));
    const tt::WriterMetrics fresh = MUST(writer.metrics());
    CHECK(fresh.head.next_seq == 0 && !fresh.checkpoint && !fresh.since_checkpoint);
    const std::string payload(200, 'x');
    for (std::uint64_t seq = 0; seq < 400; seq++) MUST(writer.append(1, 0, 2000000 + seq, payload));
    MUST(writer.checkpoint());
    for (std::uint64_t seq = 400; seq < 1000; seq++) MUST(writer.append(1, 0, 2000000 + seq, payload));
    tt::Reader lagging = MUST(tt::Reader::open(dir));
    for (int i = 0; i < 100; i++) CHECK(MUST(lagging.next()).has_value());

    const tt::WriterMetrics w = MUST(writer.metrics());
    CHECK(w.head.next_seq == 1000 && w.head.segment_base_seq > 0);
    CHECK(w.checkpoint && w.checkpoint->next_seq == 400);
    CHECK(w.since_checkpoint && w.since_checkpoint->count() >= 0);
    CHECK(w.uncheckpointed_records == 600 && w.uncheckpointed_bytes > 0);
    CHECK(!w.poisoned && w.retention_failures == 0 && w.segment_size == 65536);

    tt::JournalStatus status = MUST(tt::JournalStatus::sample(dir));
    const tt::StatusReport &r = status.report();
    CHECK(r.head.position.next_seq == 1000 && r.head.position.offset == w.head.offset);
    CHECK(r.head.end == tt::HeadEnd::open && r.head.corrupt_reason == tt::CorruptReason::none);
    CHECK(r.head.last_local_ts_ns == std::optional<std::uint64_t>(2000999));
    CHECK(!r.head.chain_broken && r.segments.count > 2 && r.segments.first_seq == 0);
    CHECK(r.checkpoint && r.checkpoint->marker.next_seq == 400 && r.checkpoint->problem == tt::CheckpointProblem::usable);
    CHECK(r.exposure.records == std::optional<std::uint64_t>(600) && r.exposure.bytes.value_or(0) > 0);
    CHECK(r.exposure.oldest_local_ts_ns == std::optional<std::uint64_t>(2000400));
    CHECK(r.storage.available_bytes > 0 && r.storage.next_segment_size == 65536);
    CHECK(r.writer_alive == std::optional<bool>(true) && r.writer_pid.has_value());
    CHECK(r.readers_dir_state == tt::ReadersDirState::missing && status.beacons().empty());

    // The reader's lag; the metrics call does not invalidate a view.
    const std::optional<tt::RecordView> view = MUST(lagging.next());
    const tt::ReaderMetrics m = MUST(lagging.metrics(status));
    CHECK(view && view->seq() == 100 && view->size() == 200 && view->data()[0] == 'x');
    CHECK(m.next_seq == 101 && m.slot.next_seq == 101 && !m.seek_pending && !m.durable_only && !m.held_record);
    CHECK(m.lag && m.lag->records == 899 && m.lag->bytes.value_or(0) > 0 && m.lag_unknown == tt::LagUnknown::known);

    // Past the sample: unknown until a new sample.
    for (std::uint64_t seq = 1000; seq < 1400; seq++) MUST(writer.append(1, 0, 0, payload));
    while (MUST(lagging.next()).has_value()) {
    }
    const tt::ReaderMetrics ahead = MUST(lagging.metrics(status));
    CHECK(!ahead.lag && ahead.lag_unknown == tt::LagUnknown::ahead_of_sample);
    status = MUST(tt::JournalStatus::sample(dir, fs::path(dir) / "readers"));
    const tt::ReaderMetrics caught_up = MUST(lagging.metrics(status));
    CHECK(caught_up.lag && caught_up.lag->records == 0 && caught_up.lag->bytes == std::optional<std::uint64_t>(0));

    // Empty objects and errors.
    tt::JournalStatus moved = std::move(status);
    CHECK(moved && !status.is_open());
    CHECK_FAILS(lagging.metrics(status), tt::Status::invalid_argument);
    CHECK_FAILS(tt::Reader().metrics(moved), tt::Status::invalid_argument);
    CHECK_FAILS(tt::Writer().metrics(), tt::Status::invalid_argument);
    fs::create_directories(root + "/metrics-empty");
    CHECK_FAILS(tt::JournalStatus::sample(root + "/metrics-empty"), tt::Status::not_found);
}

// Background checkpoints (ADR 0038): the request returns its frontier at once, appends continue, the progress
// reports the pending frontier and then the marker, a durable-only reader sees the marker move, and close waits for
// a checkpoint in flight. (Failures on the background path are tested through the C ABI with fault injection, in the
// library's Rust tests.)
static void background_checkpoints(const std::string &root) {
    const std::string dir = root + "/background";
    tt::WriterOptions options = small_segments();
    options.early_writeback(true).helper_cpus(tt::HelperCpus::process);
    tt::Writer writer = MUST(tt::Writer::open(dir, options));
    const tt::CheckpointProgress fresh = MUST(writer.checkpoint_progress());
    CHECK(!fresh.durable && !fresh.pending);
    const std::string payload(200, 'x');
    for (std::uint64_t seq = 0; seq < 400; seq++) MUST(writer.append(1, 0, 0, payload));
    tt::ReaderOptions durable_only;
    durable_only.durable_only(true);
    tt::Reader reader = MUST(tt::Reader::open(dir, durable_only));
    CHECK(!MUST(reader.next()).has_value());

    CHECK(MUST(writer.request_checkpoint()) == 400);
    for (std::uint64_t seq = 400; seq < 800; seq++) MUST(writer.append(1, 0, 0, payload));  // rolls meanwhile
    tt::CheckpointProgress progress = MUST(writer.checkpoint_progress());
    for (int polls = 0; progress.pending && polls < 30000; polls++) {
        CHECK(*progress.pending == 400);
        std::this_thread::sleep_for(std::chrono::milliseconds(1));
        progress = MUST(writer.checkpoint_progress());
    }
    CHECK(!progress.pending && progress.durable && progress.durable->next_seq == 400);
    const std::optional<tt::Checkpoint> marker = MUST(tt::read_checkpoint(dir));
    CHECK(marker && marker->generation == progress.durable->generation && marker->offset == progress.durable->offset);
    std::uint64_t durable = 0;
    while (const std::optional<tt::RecordView> r = MUST(reader.next())) {
        CHECK(r->seq() == durable);
        durable++;
    }
    CHECK(durable == 400);

    CHECK(MUST(writer.request_checkpoint()) == 800);
    MUST(writer.close());  // waits for the checkpoint in flight
    CHECK(MUST(tt::read_checkpoint(dir))->next_seq == 800);
    CHECK_FAILS(writer.request_checkpoint(), tt::Status::invalid_argument);
    CHECK_FAILS(writer.checkpoint_progress(), tt::Status::invalid_argument);
    tt::WriterOptions cpus;
    cpus.helper_cpus({0});
    CHECK(!cpus.error());
}

int main(int argc, char **argv) {
    if (argc == 3 && std::string(argv[1]) == "write-fixture") return write_fixture(argv[2]) ? 1 : 0;
    if (argc == 5 && std::string(argv[1]) == "read-fixture") {
        const int result =
            read_fixture(argv[2], std::strtoull(argv[3], nullptr, 10), std::strtoull(argv[4], nullptr, 10));
        if (result) std::fprintf(stderr, "%d check(s) failed\n", result);
        return result ? 1 : 0;
    }
    if (argc == 3 && std::string(argv[2]) == "throw-from-fill") return throw_from_fill(std::string(argv[1]) + "/throw");
    if (argc == 3 && std::string(argv[2]) == "stale-view") return stale_view(std::string(argv[1]) + "/stale");
    if (argc == 3 && std::string(argv[2]) == "stale-empty-span") {
        return stale_empty_span(std::string(argv[1]) + "/stale-empty");
    }
    if (argc != 2) {
        std::fprintf(stderr, "usage: %s <scratch-dir> [throw-from-fill|stale-view|stale-empty-span] | write-fixture <dir> | "
                             "read-fixture <dir> <count> <durable_next>\n",
                     argv[0]);
        return 2;
    }
    const std::string root = argv[1];
    versions();
    names_match_the_library();
    timeouts();
    exact_durations();
    surface(root);
    retention(root);
    errors(root);
    fill_callbacks(root);
    move_semantics(root);
    metrics(root);
    background_checkpoints(root);
#if defined(__cpp_exceptions)
    exceptions(root);
#endif
    if (failures) {
        std::fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("ok\n");
    return 0;
}
