// Compile-only test of the C++ binding (include/tickertape.hpp): uses every public declaration, including every
// template with each kind of argument it accepts, so that tests/c_api.rs can compile it as C++17, C++20 and C++23,
// with and without exceptions, with GCC and Clang, under -Wall -Wextra -Wpedantic -Wconversion -Wshadow -Werror.
// It is compiled to an object file and never run.
#include "tickertape.hpp"

#include <array>
#include <chrono>
#include <filesystem>
#include <string>
#include <string_view>
#include <type_traits>
#include <utility>
#include <vector>

#if __cplusplus >= 202002L
#include <span>
#endif

namespace tt = tickertape;
using namespace std::chrono_literals;

// The header's internal macros are gone after it (C8: they would leak into the includer's code).
#if defined(TICKERTAPE_HPP_MODE_DEPENDENT) || defined(TICKERTAPE_HPP_DEBUG_CHECKS_TAG_) ||   \
    defined(TICKERTAPE_HPP_EXCEPTIONS) || defined(TICKERTAPE_HPP_INLINE_NAMESPACE_) || \
    defined(TICKERTAPE_HPP_EXCEPTIONS_TAG_)
#error "tickertape.hpp leaks an internal macro"
#endif
#if !defined(TICKERTAPE_HPP_VERSION_NUMBER) || !defined(TICKERTAPE_HPP_ABI_VERSION) || !defined(TICKERTAPE_DEBUG_CHECKS)
#error "tickertape.hpp must keep its public macros"
#endif

// ---- Compile-time properties of the API (ADR 0036) ----
namespace properties {
template <class T, class = void>
struct has_const_rvalue_value : std::false_type {};
template <class T>
struct has_const_rvalue_value<T, std::void_t<decltype(std::declval<const T &&>().value())>> : std::true_type {};
template <class T, class = void>
struct has_const_rvalue_deref : std::false_type {};
template <class T>
struct has_const_rvalue_deref<T, std::void_t<decltype(*std::declval<const T &&>())>> : std::true_type {};
template <class Bytes, class = void>
struct makes_record : std::false_type {};
template <class Bytes>
struct makes_record<Bytes, std::void_t<decltype(tt::new_record(1, 2, 3, std::declval<Bytes>()))>> : std::true_type {};
template <class Buffer, class = void>
struct copies_into : std::false_type {};
template <class Buffer>
struct copies_into<Buffer, std::void_t<decltype(std::declval<tt::Reader &>().next_copy(std::declval<Buffer>()))>>
    : std::true_type {};

// A1: NewRecord is the C struct, so an array of them is an array of the C type.
static_assert(std::is_same_v<tt::NewRecord, tickertape_new_record>, "NewRecord is tickertape_new_record");
static_assert(std::is_same_v<decltype(tt::new_record(1, 2, 3, nullptr, 0)), tt::NewRecord>, "new_record()");
static_assert(makes_record<std::string &>::value && makes_record<const std::vector<char> &>::value &&
                  makes_record<std::string_view>::value && makes_record<std::array<std::byte, 2> &>::value,
              "new_record takes lvalues of byte ranges and rvalue views");
static_assert(!makes_record<std::string>::value && !makes_record<std::vector<std::uint8_t>>::value &&
                  !makes_record<std::array<char, 4>>::value && !makes_record<const std::string>::value,
              "new_record refuses rvalues that own their bytes");
#if __cplusplus >= 202002L
static_assert(makes_record<std::span<const char>>::value, "a span rvalue is a view");
#endif

// B1 (a): a payload type with a throwing move is refused (compile-fail test in tests/c_api.rs).
static_assert(std::is_nothrow_move_constructible_v<tt::result<tt::Writer>> &&
                  std::is_nothrow_move_constructible_v<tt::result<std::vector<tt::SegmentInfo>>>,
              "results move without throwing");
// B1 (b): result(const T&) only for copyable T.
static_assert(!std::is_constructible_v<tt::result<tt::Writer>, const tt::Writer &>, "a Writer cannot be copied");
static_assert(!std::is_constructible_v<tt::result<tt::Writer>, tt::Writer &>, "not from an lvalue either");
static_assert(std::is_constructible_v<tt::result<tt::Writer>, tt::Writer &&>, "moved in");
static_assert(std::is_constructible_v<tt::result<std::uint64_t>, const std::uint64_t &>, "copied in");
// B1 (c): the converting constructors do not throw.
static_assert(std::is_nothrow_constructible_v<tt::result<tt::Writer>, tt::Writer &&> &&
                  std::is_nothrow_constructible_v<tt::result<tt::Writer>, tt::Error &&> &&
                  std::is_nothrow_constructible_v<tt::result<std::uint64_t>, std::uint64_t &&> &&
                  std::is_nothrow_constructible_v<tt::result<std::uint64_t>, const std::uint64_t &> &&
                  std::is_nothrow_constructible_v<tt::result<void>, tt::Error &&>,
              "noexcept converting constructors");
// B1 (d): a const rvalue result hands out copies, never references, and nothing for a type it cannot copy.
static_assert(std::is_same_v<decltype(std::declval<const tt::result<std::uint64_t> &&>().value()), std::uint64_t> &&
                  std::is_same_v<decltype(*std::declval<const tt::result<std::uint64_t> &&>()), std::uint64_t> &&
                  std::is_same_v<decltype(std::declval<const tt::result<std::uint64_t> &&>().error()), tt::Error> &&
                  std::is_same_v<decltype(std::declval<const tt::result<void> &&>().error()), tt::Error>,
              "const rvalue accessors return by value");
static_assert(!has_const_rvalue_value<tt::result<tt::Writer>>::value &&
                  !has_const_rvalue_deref<tt::result<tt::Writer>>::value,
              "a const rvalue result<Writer> cannot hand out its Writer");
static_assert(has_const_rvalue_value<tt::result<std::uint64_t>>::value, "detector works");
// B1 (e): Error::message() on a temporary returns a string, not a reference into it.
static_assert(std::is_same_v<decltype(std::declval<tt::Error &&>().message()), std::string> &&
                  std::is_same_v<decltype(std::declval<const tt::Error &&>().message()), std::string> &&
                  std::is_same_v<decltype(std::declval<const tt::Error &>().message()), const std::string &>,
              "message() lends a reference only from an lvalue");
// B2: copying an Exception never throws (as for every standard exception).
static_assert(std::is_nothrow_copy_constructible_v<tt::Exception> && std::is_nothrow_copy_assignable_v<tt::Exception>,
              "Exception copies without throwing");
// B2 (round 3): moving an Exception copies it (it has no move operations), so a moved-from one keeps its Error;
// the copy, and therefore the move, never throws. test_binding.cpp checks every accessor after a move.
static_assert(std::is_nothrow_move_constructible_v<tt::Exception> && std::is_nothrow_move_assignable_v<tt::Exception>,
              "moving an Exception does not throw");
// enable_borrowed_bytes, the binding's own opt-in for rvalue views in new_record(), is the same at every language
// level: string views (and, where it exists, std::span) only; owning ranges stay out.
static_assert(tt::enable_borrowed_bytes<std::string_view> && tt::detail::is_byte_view_v<const std::string_view &&>,
              "string views are borrowed");
static_assert(!tt::enable_borrowed_bytes<std::string> && !tt::enable_borrowed_bytes<std::vector<std::uint8_t>> &&
                  !tt::enable_borrowed_bytes<std::array<char, 4>>,
              "owning ranges are not");
#if __cplusplus >= 202002L && defined(__cpp_lib_span)
static_assert(tt::enable_borrowed_bytes<std::span<const std::uint8_t>> && tt::enable_borrowed_bytes<std::span<char, 4>>,
              "spans are borrowed");
#endif
// B3: next_copy takes untyped memory and writable byte ranges.
static_assert(copies_into<std::array<char, 8> &>::value && copies_into<std::vector<std::byte> &>::value &&
                  copies_into<std::string &>::value,
              "next_copy into a writable byte range");
static_assert(!copies_into<const std::array<char, 8> &>::value && !copies_into<std::string_view &>::value,
              "not into read-only memory");
// B4: tickertape::PathArg no longer exists, so a variable of it cannot be declared (compile-fail test in
// tests/c_api.rs); temporaries as path arguments are used in use_everything() below.
}  // namespace properties

std::uint64_t use_everything(const std::filesystem::path &dir);

std::uint64_t use_everything(const std::filesystem::path &dir) {
    std::uint64_t sum = tt::library_abi_version() + tt::library_version_number() + tt::library_version().size() +
                        tt::realtime_ns() + (tt::library_compatible() ? 1u : 0u);
    if (!tt::check_library()) return 0;
    sum += tt::status_name(tt::Status::io).size() + tt::corrupt_reason_name(tt::CorruptReason::bad_seal).size();

    tt::Retention retention;
    retention.max_segments(3).max_bytes(1 << 30).max_age(24h).max_age(std::chrono::duration<double>(1.5));
    retention.delete_segments().move_to(dir / "archive").move_to("archive").move_to(std::string("a"));
    retention.move_to(std::string_view("b"));
    // B4: temporaries as path arguments live until the call returns.
    sum += tt::verify(std::filesystem::path(dir) / "journal").has_value() ? 1u : 0u;
    sum += tt::list_segments(dir.string() + "/journal").has_value() ? 1u : 0u;
    tt::WriterOptions writer_options;
    writer_options.segment_size(1 << 20).recovery_policy(tt::RecoveryPolicy::fail).notify(true).prefault(false);
    writer_options.retention(retention).no_retention();
    tt::ReaderOptions reader_options;
    reader_options.durable_only(true).notify(false).backoff(100, 10, 10us, 1ms).busy_spin();
    sum += writer_options.error() ? 1u : 0u;
    sum += reader_options.native_handle() != nullptr ? 1u : 0u;
    // The non-const handles take the C setters the binding does not wrap, without a const_cast.
    sum += static_cast<std::uint64_t>(-tickertape_retention_set_max_segments(retention.native_handle(), 3));
    sum += static_cast<std::uint64_t>(-tickertape_writer_options_set_notify(writer_options.native_handle(), 1));
    sum += static_cast<std::uint64_t>(-tickertape_reader_options_set_notify(reader_options.native_handle(), 1));
    const tt::ReaderOptions &const_options = reader_options;
    sum += const_options.native_handle() != nullptr ? 1u : 0u;

    tt::result<tt::Writer> opened = tt::Writer::open(dir, writer_options);
    if (!opened) return static_cast<std::uint64_t>(opened.error().os_code()) + opened.error().needed_size();
    tt::Writer writer = std::move(opened).value();
    const std::string s = "string";
    const std::vector<std::uint8_t> v{1, 2};
    const std::vector<char> c{'a'};
    const std::array<std::byte, 1> b{std::byte{1}};
    sum += writer.append(1, 2, 3, s).value_or(0) + writer.append(1, 2, 3, std::string_view("sv")).value_or(0);
    sum += writer.append(1, 2, 3, v).value_or(0) + writer.append(1, 2, 3, c).value_or(0);
    sum += writer.append(1, 2, 3, b).value_or(0) + writer.append(1, 2, 3, s.data(), s.size()).value_or(0);
#if __cplusplus >= 202002L
    sum += writer.append(1, 2, 3, std::span<const std::uint8_t>(v)).value_or(0);
    sum += writer.append(1, 2, 3, std::u8string_view(u8"utf-8")).value_or(0);
#endif
    sum += *writer.append_with(1, 2, 3, 4, [](std::uint8_t *buf, std::size_t len) { buf[len - 1] = 0; });
    sum += writer.append_with(1, 2, 3, 4, [](std::uint8_t *, std::size_t) { return false; }).value_or(0);
    struct Functor {
        bool operator()(std::uint8_t *, std::size_t) const { return true; }
    };
    const Functor functor;
    sum += writer.append_with(1, 2, 3, 0, functor).value_or(0);
    std::vector<tt::NewRecord> records{tt::new_record(1, 2, 3, s), tt::new_record(1, 2, 3, v.data(), v.size())};
    records.push_back(tt::new_record(1, 2, 3, std::string_view("view")));
    const std::array<tt::NewRecord, 1> fixed{tt::NewRecord{}};
    tt::BatchAppend batch = writer.append_batch(records);
    sum += batch.first_seq + batch.appended + (batch.outcome ? 1u : 0u);
    sum += writer.append_batch(fixed).appended + writer.append_batch(records.data(), records.size()).appended;
    sum += records[0].schema_id + records[0].venue_ts_ns + records[0].local_ts_ns + records[0].payload_len;
    sum += records[0].payload != nullptr ? 1u : 0u;
    writer.checkpoint().value();
    sum += writer.next_seq().value() + (writer.last_retention_error() ? 1u : 0u);
    sum += writer.native_handle() != nullptr ? 1u : 0u;
    sum += writer.is_open() && static_cast<bool>(writer) ? 1u : 0u;

    tt::Reader reader = tt::Reader::open(dir, reader_options).value();
    tt::Reader mapped = tt::Reader::open_mapped_unsafe(dir.string()).value();
    if (auto m = tt::Reader::open_mapped_unsafe(dir.c_str(), reader_options)) mapped = std::move(*m);
    reader.seek_to_end().value();
    reader.seek(0).value();
    for (tt::Timeout timeout : {tt::Timeout(), tt::no_wait, tt::Timeout(tt::wait_forever), tt::Timeout(5ms),
                                tt::Timeout(std::chrono::duration<double>(0.25)), tt::Timeout(-1s)}) {
        if (tt::result<std::optional<tt::RecordView>> r = reader.next(timeout); r && *r) {
            const tt::RecordView &view = **r;
            sum += view.seq() + view.schema_id() + view.venue_ts_ns() + view.local_ts_ns() + view.size();
            sum += view.as_string_view().size() + (view.data() != nullptr ? 1u : 0u) + (view.empty() ? 1u : 0u);
#if __cplusplus >= 202002L
            sum += tt::as_span(view).size();
#endif
        }
    }
    sum += reader.next(tt::wait_forever).value().has_value() ? 1u : 0u;
    std::array<std::uint8_t, 16> buffer{};
    if (auto r = reader.next_copy(buffer.data(), buffer.size(), 1ms); r && *r) sum += (*r)->size();
    char chars[16];
    if (auto r = reader.next_copy(chars, sizeof chars); r && *r) sum += (*r)->size();
    if (auto r = reader.next_copy(buffer, 1ms); r && *r) sum += (*r)->size();
    tt::RecordBatch records_in(8, 1024);
    sum += reader.read_batch(records_in, 1ms).value_or(0) + records_in.size() + records_in.capacity();
    sum += records_in.buffer_size() + (records_in.empty() ? 1u : 0u);
    for (tt::RecordView view : records_in) sum += view.seq();
    for (auto it = records_in.begin(); it != records_in.end(); it++) sum += (*it).size();
    if (!records_in.empty()) sum += records_in[0].seq();
    sum += reader.position().value() + (reader.native_handle() != nullptr ? 1u : 0u);
    {
        tt::JournalStatus status = tt::JournalStatus::sample(dir).value();
        tt::JournalStatus moved(std::move(status));
        status = tt::JournalStatus::sample(dir, std::filesystem::path(dir) / "readers").value();
        sum += (status.is_open() && moved && moved.native_handle() != nullptr) ? 1u : 0u;
        const tt::StatusReport &r = moved.report();
        sum += r.sampled_unix_ns + r.head.position.next_seq + r.head.position.segment_base_seq;
        sum += r.head.position.offset + r.head.segment_size + r.head.last_local_ts_ns.value_or(0);
        sum += static_cast<std::uint64_t>(r.head.end) + static_cast<std::uint64_t>(r.head.corrupt_reason);
        sum += (r.head.verified ? 1u : 0u) + (r.head.chain_broken ? 1u : 0u);
        sum += r.segments.count + r.segments.first_seq + r.segments.total_bytes + r.segments.before_checkpoint;
        sum += r.segments.oldest_created_unix_ns.value_or(0);
        if (r.checkpoint) {
            sum += r.checkpoint->marker.next_seq + static_cast<std::uint64_t>(r.checkpoint->problem);
            sum += r.checkpoint->modified_unix_ns.value_or(0) + static_cast<std::uint64_t>(r.checkpoint->age_unknown);
            sum += static_cast<std::uint64_t>(r.checkpoint->estimated_age.value_or(1ns).count());
        }
        sum += r.exposure.records.value_or(0) + r.exposure.bytes.value_or(0) + r.exposure.oldest_local_ts_ns.value_or(0);
        sum += static_cast<std::uint64_t>(r.exposure.estimated_age.value_or(1ns).count()) + (r.exposure.verified ? 1u : 0u);
        sum += r.storage.available_bytes + r.storage.total_bytes + r.storage.next_segment_size;
        sum += r.storage.estimated_rolls_left + (r.storage.spare_segment ? 1u : 0u);
        sum += r.writer_pid.value_or(0) + (r.writer_alive.value_or(false) ? 1u : 0u);
        sum += static_cast<std::uint64_t>(r.readers_dir_state);
        for (const tt::BeaconStatus &beacon : moved.beacons()) {
            sum += beacon.name.size() + static_cast<std::uint64_t>(beacon.state) + static_cast<std::uint64_t>(beacon.lag_unknown);
            if (beacon.report) {
                sum += beacon.report->pid + beacon.report->next_seq + beacon.report->slot.offset + beacon.report->updated_unix_ns;
                sum += (beacon.report->seek_pending ? 1u : 0u) + (beacon.report->durable_only ? 1u : 0u);
            }
            if (beacon.lag) sum += beacon.lag->records + beacon.lag->bytes.value_or(0);
            sum += static_cast<std::uint64_t>(beacon.update_age.value_or(1ns).count() + beacon.unread_age.value_or(1ns).count());
            sum += beacon.oldest_unread_local_ts_ns.value_or(0);
        }
        const tt::ReaderMetrics rm = reader.metrics(moved).value();
        sum += rm.next_seq + rm.slot.next_seq + (rm.seek_pending ? 1u : 0u) + (rm.durable_only ? 1u : 0u);
        sum += (rm.held_record ? 1u : 0u) + static_cast<std::uint64_t>(rm.lag_unknown);
        if (rm.lag) sum += rm.lag->records + rm.lag->bytes.value_or(0);
        const tt::WriterMetrics wm = writer.metrics().value();
        sum += wm.head.next_seq + wm.segment_size + wm.uncheckpointed_records + wm.uncheckpointed_bytes;
        sum += wm.retention_failures + (wm.poisoned ? 1u : 0u);
        sum += static_cast<std::uint64_t>(wm.since_checkpoint.value_or(1ns).count());
        if (wm.checkpoint) sum += wm.checkpoint->generation;
        status = tt::JournalStatus();
    }
    reader.close().value();
    writer.close().value();

    const tt::VerifyReport report = tt::verify(dir).value();
    sum += report.segments + report.records + report.first_seq.value_or(0) + report.last_seq.value_or(0);
    if (report.first_problem) {
        sum += report.first_problem->segment_base_seq + report.first_problem->offset;
        sum += static_cast<std::uint64_t>(report.first_problem->reason);
    }
    for (const tt::SegmentInfo &segment : tt::list_segments(dir).value()) {
        sum += segment.base_seq + segment.segment_size + segment.created_unix_ns + segment.format_version;
    }
    if (std::optional<tt::Checkpoint> cp = tt::read_checkpoint(dir).value()) {
        sum += cp->generation + cp->next_seq + cp->segment_base_seq + cp->offset;
    }
    const tt::RetentionReport plan = tt::plan_retention(dir, retention).value();
    const tt::RetentionReport done = tt::apply_retention(dir, retention).value();
    sum += plan.removed_segments + plan.removed_bytes + done.already_removed_segments;

    tt::result<void> ok;
    tt::result<int> failed = tt::Error(tt::Status::busy, "x", 0, tt::CorruptReason::none, 0);
    const tt::Error &error = failed.error();
    sum += ok.has_value() ? 1u : 0u;
    sum += error.message().size() + error.name().size() + error.to_string().size();
    sum += static_cast<std::uint64_t>(error.corrupt_reason());
    sum += static_cast<std::uint64_t>(-static_cast<std::int32_t>(error.status()));
    const tt::Exception exception(error);
    sum += exception.error().message().size() + static_cast<std::uint64_t>(exception.status() == tt::Status::busy);
    const tt::Exception copy(exception);
    sum += copy.error().message().size() + std::move(failed).error().message().size();
    const tt::result<std::uint64_t> const_value = std::uint64_t{7};
    sum += std::move(const_value).value() + *std::move(const_value);
    return sum;
}
