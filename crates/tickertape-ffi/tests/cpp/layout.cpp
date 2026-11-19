// Pins the size and alignment of every public class of the C++ binding (include/tickertape.hpp), compiled by
// tests/c_api.rs in the compile-only matrix (every compiler, C++17/20/23, with and without exceptions and NDEBUG).
//
// ADR 0036 -> Versioning: within a major version no data member is added to (or removed from) any existing class,
// because two components built against different header versions would then disagree about one type. Each
// `Mirror` below lists a class's data members in order; if a static_assert fails, a class's layout changed. That
// is allowed only in a new major version, together with a new inline namespace (v2). Do not "fix" a failure by
// editing the mirror within a major version. A test in tests/c_api.rs fails if a public class is missing here.
#include "tickertape.hpp"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string>
#include <type_traits>
#include <variant>
#include <vector>

namespace tt = tickertape;

#define TT_PIN(Type, Mirror)                                                                                 \
    static_assert(sizeof(Type) == sizeof(Mirror) && alignof(Type) == alignof(Mirror),                         \
                  #Type " changed its size or alignment: no data member may be added within a major version " \
                        "(ADR 0036 -> Versioning)")

// The layout version is the inline namespace: tickertape::X is tickertape::v1::X.
static_assert(std::is_same_v<tt::Writer, tt::v1::Writer> && std::is_same_v<tt::Error, tt::v1::Error> &&
                  std::is_same_v<tt::result<void>, tt::v1::result<void>>,
              "the public types must live in the inline namespace tickertape::v1");

namespace mirror {
struct Error {
    tt::Status status;
    std::int32_t os_code;
    tt::CorruptReason corrupt_reason;
    std::size_t needed_size;
    std::string message;
};
struct Exception : std::runtime_error {
    tt::Error error;
};
template <class T>
struct Result {
    std::variant<T, tt::Error> v;
};
struct ResultVoid {
    std::optional<tt::Error> error;
};
struct PathArg {
    std::string owned;
    const char *ptr;
    bool has_nul;
};
struct Timeout {
    std::int64_t ns;
};
struct Empty {};
struct Options {  // Retention, WriterOptions and ReaderOptions
    void *handle;
    std::optional<tt::Error> error;
};
struct RecordView {
    tickertape_record rec;
    const std::uint64_t *generation;
    std::uint64_t snapshot;
};
struct RecordBatch {
    std::unique_ptr<tickertape_record[]> records;
    std::unique_ptr<std::uint8_t[]> buffer;
    std::size_t capacity;
    std::size_t buffer_size;
    std::size_t count;
    std::uint64_t generation;
};
struct RecordBatchIterator {
    const void *batch;
    std::size_t index;
};
struct BatchAppend {
    std::uint64_t first_seq;
    std::size_t appended;
    tt::result<void> outcome;
};
struct Writer {
    void *handle;
    void *frame;
};
struct Reader {
    void *handle;
    std::uint64_t generation;
};
struct Problem {
    std::uint64_t segment_base_seq;
    std::uint64_t offset;
    tt::CorruptReason reason;
};
struct VerifyReport {
    std::uint64_t segments;
    std::uint64_t records;
    std::optional<std::uint64_t> first_seq;
    std::optional<std::uint64_t> last_seq;
    std::uint64_t problems;
    std::optional<tt::VerifyReport::Problem> first_problem;
};
struct SegmentInfo {
    std::uint64_t base_seq;
    std::uint64_t segment_size;
    std::uint64_t created_unix_ns;
    std::uint32_t format_version;
};
struct Checkpoint {
    std::uint64_t generation;
    std::uint64_t next_seq;
    std::uint64_t segment_base_seq;
    std::uint64_t offset;
};
struct RetentionReport {
    std::uint64_t removed_segments;
    std::uint64_t removed_bytes;
    std::uint64_t already_removed_segments;
};
}  // namespace mirror

TT_PIN(tt::Status, std::int32_t);
TT_PIN(tt::CorruptReason, std::int32_t);
TT_PIN(tt::RecoveryPolicy, std::int32_t);
static_assert(std::is_same_v<std::underlying_type_t<tt::Status>, std::int32_t> &&
                  std::is_same_v<std::underlying_type_t<tt::CorruptReason>, std::int32_t> &&
                  std::is_same_v<std::underlying_type_t<tt::RecoveryPolicy>, std::int32_t>,
              "the enumerations' underlying types are part of the layout");
TT_PIN(tt::Error, mirror::Error);
TT_PIN(tt::Exception, mirror::Exception);
TT_PIN(tt::result<void>, mirror::ResultVoid);
// result<T> for every T the binding returns.
TT_PIN(tt::result<std::uint64_t>, mirror::Result<std::uint64_t>);
TT_PIN(tt::result<std::size_t>, mirror::Result<std::size_t>);
TT_PIN(tt::result<tt::Writer>, mirror::Result<tt::Writer>);
TT_PIN(tt::result<tt::Reader>, mirror::Result<tt::Reader>);
TT_PIN(tt::result<std::optional<tt::RecordView>>, mirror::Result<std::optional<tt::RecordView>>);
TT_PIN(tt::result<tt::VerifyReport>, mirror::Result<tt::VerifyReport>);
TT_PIN(tt::result<std::vector<tt::SegmentInfo>>, mirror::Result<std::vector<tt::SegmentInfo>>);
TT_PIN(tt::result<std::optional<tt::Checkpoint>>, mirror::Result<std::optional<tt::Checkpoint>>);
TT_PIN(tt::result<tt::RetentionReport>, mirror::Result<tt::RetentionReport>);
TT_PIN(tt::PathArg, mirror::PathArg);
TT_PIN(tt::Timeout, mirror::Timeout);
TT_PIN(tt::Timeout::Forever, mirror::Empty);
TT_PIN(tt::Retention, mirror::Options);
TT_PIN(tt::WriterOptions, mirror::Options);
TT_PIN(tt::ReaderOptions, mirror::Options);
TT_PIN(tt::NewRecord, tickertape_new_record);
TT_PIN(tt::RecordView, mirror::RecordView);
TT_PIN(tt::RecordBatch, mirror::RecordBatch);
TT_PIN(tt::RecordBatch::iterator, mirror::RecordBatchIterator);
TT_PIN(tt::BatchAppend, mirror::BatchAppend);
TT_PIN(tt::Writer, mirror::Writer);
TT_PIN(tt::Reader, mirror::Reader);
TT_PIN(tt::VerifyReport::Problem, mirror::Problem);
TT_PIN(tt::VerifyReport, mirror::VerifyReport);
TT_PIN(tt::SegmentInfo, mirror::SegmentInfo);
TT_PIN(tt::Checkpoint, mirror::Checkpoint);
TT_PIN(tt::RetentionReport, mirror::RetentionReport);
