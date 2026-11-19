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
#include <vector>

#if __cplusplus >= 202002L
#include <span>
#endif

namespace tt = tickertape;
using namespace std::chrono_literals;

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
    tt::WriterOptions writer_options;
    writer_options.segment_size(1 << 20).recovery_policy(tt::RecoveryPolicy::fail).notify(true).prefault(false);
    writer_options.retention(retention).no_retention();
    tt::ReaderOptions reader_options;
    reader_options.durable_only(true).notify(false).backoff(100, 10, 10us, 1ms).busy_spin();
    sum += writer_options.error() ? 1u : 0u;
    sum += reader_options.native_handle() != nullptr ? 1u : 0u;

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
    std::vector<tt::NewRecord> records{tt::NewRecord(1, 2, 3, s), tt::NewRecord(1, 2, 3, v.data(), v.size())};
    const std::array<tt::NewRecord, 1> fixed{tt::NewRecord()};
    tt::BatchAppend batch = writer.append_batch(records);
    sum += batch.first_seq + batch.appended + (batch.outcome ? 1u : 0u);
    sum += writer.append_batch(fixed).appended + writer.append_batch(records.data(), records.size()).appended;
    sum += records[0].schema_id() + records[0].venue_ts_ns() + records[0].local_ts_ns() + records[0].size();
    sum += records[0].data() != nullptr ? 1u : 0u;
    writer.checkpoint().value();
    sum += writer.next_seq().value() + (writer.last_retention_error() ? 1u : 0u);
    sum += writer.native_handle() != nullptr ? 1u : 0u;
    sum += writer.is_open() && static_cast<bool>(writer) ? 1u : 0u;

    tt::Reader reader = tt::Reader::open(dir, reader_options).value();
    tt::Reader mapped = tt::Reader::open_mapped_unsafe(dir.string()).value();
    if (auto m = tt::Reader::open_mapped_unsafe(dir.c_str(), reader_options)) mapped = std::move(*m);
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
    tt::RecordBatch records_in(8, 1024);
    sum += reader.read_batch(records_in, 1ms).value_or(0) + records_in.size() + records_in.capacity();
    sum += records_in.buffer_size() + (records_in.empty() ? 1u : 0u);
    for (tt::RecordView view : records_in) sum += view.seq();
    for (auto it = records_in.begin(); it != records_in.end(); it++) sum += (*it).size();
    if (!records_in.empty()) sum += records_in[0].seq();
    sum += reader.position().value() + (reader.native_handle() != nullptr ? 1u : 0u);
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
    return sum;
}
