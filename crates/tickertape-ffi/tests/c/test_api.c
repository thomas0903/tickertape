/*
 * C99 tests of the tickertape C ABI, compiled against the committed header (include/tickertape.h) by
 * tests/c_api.rs. Modes:
 *
 *   test_api basic <scratch-dir>                       API behaviour, misuse and error cases
 *   test_api write-fixture <dir>                       writes the cross-language fixture journal
 *   test_api read-fixture <dir> <count> <durable_next> reads a fixture journal written by another implementation
 *
 * Fixture record `seq`: schema_id = seq % 5 + 1, venue_ts_ns = 1000000 + seq, local_ts_ns = 2000000 + 3 * seq,
 * payload = (seq * 37) % 300 bytes, byte i = (seq + 7 * i) & 0xFF. 64 KiB segments, so the journal rolls.
 */
#define _POSIX_C_SOURCE 200809L

#include "tickertape.h"

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* The frozen struct layouts, checked at compile time (C99 has no _Static_assert). The Rust side asserts the same
 * offsets and sizes, so swapping or resizing a field breaks the build on one side or the other. */
#define STATIC_ASSERT(name, cond) typedef char static_assert_##name[(cond) ? 1 : -1]
#if UINTPTR_MAX == UINT64_MAX
STATIC_ASSERT(record_size, sizeof(tickertape_record) == 48);
STATIC_ASSERT(record_seq, offsetof(tickertape_record, seq) == 0);
STATIC_ASSERT(record_venue_ts, offsetof(tickertape_record, venue_ts_ns) == 8);
STATIC_ASSERT(record_local_ts, offsetof(tickertape_record, local_ts_ns) == 16);
STATIC_ASSERT(record_payload, offsetof(tickertape_record, payload) == 24);
STATIC_ASSERT(record_payload_len, offsetof(tickertape_record, payload_len) == 32);
STATIC_ASSERT(record_schema_id, offsetof(tickertape_record, schema_id) == 40);
STATIC_ASSERT(record_reserved, offsetof(tickertape_record, reserved) == 44);
STATIC_ASSERT(new_record_size, sizeof(tickertape_new_record) == 40);
STATIC_ASSERT(new_record_payload, offsetof(tickertape_new_record, payload) == 0);
STATIC_ASSERT(new_record_payload_len, offsetof(tickertape_new_record, payload_len) == 8);
STATIC_ASSERT(new_record_venue_ts, offsetof(tickertape_new_record, venue_ts_ns) == 16);
STATIC_ASSERT(new_record_local_ts, offsetof(tickertape_new_record, local_ts_ns) == 24);
STATIC_ASSERT(new_record_schema_id, offsetof(tickertape_new_record, schema_id) == 32);
STATIC_ASSERT(new_record_reserved, offsetof(tickertape_new_record, reserved) == 36);
#endif
STATIC_ASSERT(segment_info_size, sizeof(tickertape_segment_info) == 32);
STATIC_ASSERT(segment_info_base_seq, offsetof(tickertape_segment_info, base_seq) == 0);
STATIC_ASSERT(segment_info_segment_size, offsetof(tickertape_segment_info, segment_size) == 8);
STATIC_ASSERT(segment_info_created, offsetof(tickertape_segment_info, created_unix_ns) == 16);
STATIC_ASSERT(segment_info_format_version, offsetof(tickertape_segment_info, format_version) == 24);
STATIC_ASSERT(segment_info_reserved, offsetof(tickertape_segment_info, reserved) == 28);
STATIC_ASSERT(checkpoint_size, sizeof(tickertape_checkpoint) == 32);
STATIC_ASSERT(checkpoint_generation, offsetof(tickertape_checkpoint, generation) == 0);
STATIC_ASSERT(checkpoint_next_seq, offsetof(tickertape_checkpoint, next_seq) == 8);
STATIC_ASSERT(checkpoint_segment_base_seq, offsetof(tickertape_checkpoint, segment_base_seq) == 16);
STATIC_ASSERT(checkpoint_offset, offsetof(tickertape_checkpoint, offset) == 24);
STATIC_ASSERT(verify_report_size, sizeof(tickertape_verify_report) == 72);
STATIC_ASSERT(verify_report_struct_size, offsetof(tickertape_verify_report, struct_size) == 0);
STATIC_ASSERT(verify_report_segments, offsetof(tickertape_verify_report, segments) == 8);
STATIC_ASSERT(verify_report_records, offsetof(tickertape_verify_report, records) == 16);
STATIC_ASSERT(verify_report_first_seq, offsetof(tickertape_verify_report, first_seq) == 24);
STATIC_ASSERT(verify_report_last_seq, offsetof(tickertape_verify_report, last_seq) == 32);
STATIC_ASSERT(verify_report_problems, offsetof(tickertape_verify_report, problems) == 40);
STATIC_ASSERT(verify_report_problem_base, offsetof(tickertape_verify_report, first_problem_base_seq) == 48);
STATIC_ASSERT(verify_report_problem_offset, offsetof(tickertape_verify_report, first_problem_offset) == 56);
STATIC_ASSERT(verify_report_problem_reason, offsetof(tickertape_verify_report, first_problem_reason) == 64);
STATIC_ASSERT(verify_report_reserved, offsetof(tickertape_verify_report, reserved) == 68);
STATIC_ASSERT(retention_report_size, sizeof(tickertape_retention_report) == 32);
STATIC_ASSERT(retention_report_struct_size, offsetof(tickertape_retention_report, struct_size) == 0);
STATIC_ASSERT(retention_report_removed, offsetof(tickertape_retention_report, removed_segments) == 8);
STATIC_ASSERT(retention_report_bytes, offsetof(tickertape_retention_report, removed_bytes) == 16);
STATIC_ASSERT(retention_report_already, offsetof(tickertape_retention_report, already_removed_segments) == 24);
STATIC_ASSERT(struct_size_is_64_bit, sizeof(((tickertape_verify_report *)0)->struct_size) == 8);

static int failures = 0;

#define CHECK(cond)                                                                                  \
    do {                                                                                             \
        if (!(cond)) {                                                                               \
            fprintf(stderr, "%s:%d: CHECK failed: %s (last error: %s %s)\n", __FILE__, __LINE__,     \
                    #cond, tickertape_status_name(tickertape_last_error_code()),                     \
                    tickertape_last_error_message());                                                \
            failures++;                                                                              \
        }                                                                                            \
    } while (0)

#define CHECK_STATUS(expr, expected)                                                                 \
    do {                                                                                             \
        tickertape_status s_ = (expr);                                                               \
        if (s_ != (expected)) {                                                                      \
            fprintf(stderr, "%s:%d: %s returned %s, expected %s (last error: %s)\n", __FILE__,       \
                    __LINE__, #expr, tickertape_status_name(s_), tickertape_status_name(expected),   \
                    tickertape_last_error_message());                                                \
            failures++;                                                                              \
        }                                                                                            \
    } while (0)

#define REQUIRE_OK(expr)                                                                             \
    do {                                                                                             \
        tickertape_status s_ = (expr);                                                               \
        if (s_ != TICKERTAPE_OK) {                                                                   \
            fprintf(stderr, "%s:%d: %s returned %s: %s\n", __FILE__, __LINE__, #expr,                \
                    tickertape_status_name(s_), tickertape_last_error_message());                    \
            exit(1);                                                                                 \
        }                                                                                            \
    } while (0)

#define FIXTURE_SEGMENT_SIZE 65536u

static size_t pattern_len(uint64_t seq) { return (size_t)(seq * 37u % 300u); }

static void pattern_fill(uint64_t seq, uint8_t *buf, size_t len) {
    size_t i;
    for (i = 0; i < len; i++) buf[i] = (uint8_t)((seq + 7u * i) & 0xFFu);
}

/* Checks a record against the fixture pattern; returns 1 if it matches. */
static int pattern_matches(const tickertape_record *r, uint64_t seq) {
    size_t i;
    if (r->seq != seq || r->schema_id != (uint32_t)(seq % 5u + 1u) || r->venue_ts_ns != 1000000u + seq ||
        r->local_ts_ns != 2000000u + 3u * seq || r->payload_len != pattern_len(seq) || r->reserved != 0) {
        fprintf(stderr, "record %" PRIu64 ": metadata mismatch (seq %" PRIu64 ", len %zu)\n", seq, r->seq,
                r->payload_len);
        return 0;
    }
    if ((r->payload_len == 0) != (r->payload == NULL)) {
        fprintf(stderr, "record %" PRIu64 ": payload must be NULL exactly when payload_len is 0\n", seq);
        return 0;
    }
    for (i = 0; i < r->payload_len; i++) {
        if (r->payload[i] != (uint8_t)((seq + 7u * i) & 0xFFu)) {
            fprintf(stderr, "record %" PRIu64 ": payload byte %zu differs\n", seq, i);
            return 0;
        }
    }
    return 1;
}

static tickertape_writer *open_writer(const char *dir, uint64_t segment_size) {
    tickertape_writer_options *options = tickertape_writer_options_new();
    tickertape_writer *writer = NULL;
    CHECK(options != NULL);
    REQUIRE_OK(tickertape_writer_options_set_segment_size(options, segment_size));
    REQUIRE_OK(tickertape_writer_options_set_prefault(options, 0));
    REQUIRE_OK(tickertape_writer_open(dir, options, &writer));
    tickertape_writer_options_free(options); /* options are copied by open */
    return writer;
}

static tickertape_reader *open_reader(const char *dir, int durable_only, int mapped) {
    tickertape_reader_options *options = tickertape_reader_options_new();
    tickertape_reader *reader = NULL;
    REQUIRE_OK(tickertape_reader_options_set_durable_only(options, durable_only));
    if (mapped) {
        REQUIRE_OK(tickertape_reader_open_mapped_unsafe(dir, options, &reader));
    } else {
        REQUIRE_OK(tickertape_reader_open(dir, options, &reader));
    }
    tickertape_reader_options_free(options);
    return reader;
}

static void append_pattern(tickertape_writer *writer, uint64_t seq) {
    uint8_t buf[300];
    uint64_t got = UINT64_MAX;
    size_t len = pattern_len(seq);
    pattern_fill(seq, buf, len);
    REQUIRE_OK(tickertape_writer_append(writer, (uint32_t)(seq % 5u + 1u), 1000000u + seq, 2000000u + 3u * seq,
                                        len ? buf : NULL, len, &got));
    CHECK(got == seq);
}

/* ---- fill callbacks ---- */

struct fill_ctx {
    tickertape_writer *writer;
    uint64_t seq;
    int abort;
    int calls;
    tickertape_status reentrant;
};

static int32_t fill_pattern(void *user_data, uint8_t *buf, size_t len) {
    struct fill_ctx *ctx = (struct fill_ctx *)user_data;
    size_t i;
    uint64_t next = 0;
    ctx->calls++;
    for (i = 0; i < len; i++) {
        if (buf[i] != 0) return 1; /* must start zeroed */
    }
    pattern_fill(ctx->seq, buf, len);
    /* Calling back into the same writer is detected. */
    ctx->reentrant = tickertape_writer_next_seq(ctx->writer, &next);
    return ctx->abort;
}

/* ---- basic mode ---- */

static void join(char *out, size_t cap, const char *dir, const char *name) {
    snprintf(out, cap, "%s/%s", dir, name);
}

/* The start-up check from the header (TICKERTAPE_ABI_VERSION): before 1.0 the exact version, from 1.0 the same ABI
 * version and a library at least as new as the header. */
static int library_is_compatible(void) {
    uint32_t lib = tickertape_version_number();
    return tickertape_abi_version() == TICKERTAPE_ABI_VERSION &&
           (TICKERTAPE_ABI_VERSION == 0 ? lib == TICKERTAPE_VERSION_NUMBER : lib >= TICKERTAPE_VERSION_NUMBER);
}

static void test_versions_and_names(void) {
    char expected[64];
    CHECK(tickertape_abi_version() == TICKERTAPE_ABI_VERSION);
    CHECK(tickertape_version_number() == TICKERTAPE_VERSION_NUMBER);
    CHECK(TICKERTAPE_VERSION_NUMBER ==
          TICKERTAPE_VERSION_MAJOR * 1000000u + TICKERTAPE_VERSION_MINOR * 1000u + TICKERTAPE_VERSION_PATCH);
    snprintf(expected, sizeof expected, "%d.%d.%d", TICKERTAPE_VERSION_MAJOR, TICKERTAPE_VERSION_MINOR,
             TICKERTAPE_VERSION_PATCH);
    CHECK(strcmp(tickertape_version(), expected) == 0);
    CHECK(library_is_compatible());
    CHECK(TICKERTAPE_E_UNKNOWN == -255);
    CHECK(strcmp(tickertape_status_name(TICKERTAPE_E_DISCARDED), "TICKERTAPE_E_DISCARDED") == 0);
    CHECK(strcmp(tickertape_status_name(-999), "TICKERTAPE_UNKNOWN_STATUS") == 0);
    CHECK(strcmp(tickertape_corrupt_reason_name(TICKERTAPE_CORRUPT_BAD_SEAL), "TICKERTAPE_CORRUPT_BAD_SEAL") == 0);
    CHECK(tickertape_realtime_ns() > 1600000000000000000ull);
    CHECK(sizeof(tickertape_record) == 48);
    CHECK(sizeof(tickertape_new_record) == 40);
}

static void test_misuse(const char *scratch) {
    tickertape_writer *writer = (tickertape_writer *)1; /* overwritten with NULL on failure */
    tickertape_reader *reader = (tickertape_reader *)1;
    tickertape_record record;
    uint64_t seq = 0;
    size_t count = 7;
    char missing[4096];
    join(missing, sizeof missing, scratch, "missing");

    CHECK_STATUS(tickertape_writer_open(NULL, NULL, &writer), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK(writer == NULL);
    CHECK(tickertape_last_error_code() == TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK(strstr(tickertape_last_error_message(), "dir") != NULL);
    CHECK_STATUS(tickertape_writer_open(missing, NULL, NULL), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK_STATUS(tickertape_writer_append(NULL, 1, 0, 0, NULL, 0, &seq), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK_STATUS(tickertape_writer_checkpoint(NULL), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK_STATUS(tickertape_writer_close(NULL), TICKERTAPE_OK);

    CHECK_STATUS(tickertape_reader_open(missing, NULL, &reader), TICKERTAPE_E_IO);
    CHECK(reader == NULL);
    CHECK(tickertape_last_error_os_code() == ENOENT);
    CHECK(mkdir(missing, 0755) == 0);
    CHECK_STATUS(tickertape_reader_open(missing, NULL, &reader), TICKERTAPE_E_NOT_FOUND);
    CHECK_STATUS(tickertape_reader_next(NULL, 0, &record), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK_STATUS(tickertape_reader_read_batch(NULL, 0, &record, 1, NULL, 0, &count), TICKERTAPE_E_INVALID_ARGUMENT);
    CHECK(count == 0);
    CHECK_STATUS(tickertape_reader_close(NULL), TICKERTAPE_OK);
    {
        /* An overlap is rejected before anything is written: every slot keeps its non-zero sentinel. */
        tickertape_record records[2], records_before[2];
        tickertape_segment_info segments[2], segments_before[2];
        memset(records, 0xA5, sizeof records);
        memcpy(records_before, records, sizeof records);
        CHECK_STATUS(tickertape_reader_read_batch(NULL, 0, records, 2, NULL, 0, &records[1].payload_len),
                     TICKERTAPE_E_INVALID_ARGUMENT);
        CHECK(memcmp(records, records_before, sizeof records) == 0);
        memset(segments, 0xA5, sizeof segments);
        memcpy(segments_before, segments, sizeof segments);
        CHECK_STATUS(tickertape_list_segments(missing, segments, 2, (size_t *)&segments[1].segment_size),
                     TICKERTAPE_E_INVALID_ARGUMENT);
        CHECK(memcmp(segments, segments_before, sizeof segments) == 0);
    }

    tickertape_writer_options_free(NULL);
    tickertape_reader_options_free(NULL);
    tickertape_retention_free(NULL);
    CHECK_STATUS(tickertape_writer_options_set_recovery_policy(NULL, TICKERTAPE_RECOVERY_FAIL),
                 TICKERTAPE_E_INVALID_ARGUMENT);
    {
        tickertape_writer_options *options = tickertape_writer_options_new();
        CHECK_STATUS(tickertape_writer_options_set_recovery_policy(options, 42), TICKERTAPE_E_INVALID_ARGUMENT);
        CHECK_STATUS(tickertape_writer_options_set_segment_size(options, 1000), TICKERTAPE_OK);
        CHECK_STATUS(tickertape_writer_open(missing, options, &writer), TICKERTAPE_E_INVALID_CONFIG);
        tickertape_writer_options_free(options);
    }
}

static void test_roundtrip(const char *scratch) {
    char dir[4096];
    tickertape_writer *writer;
    tickertape_writer *second = NULL;
    tickertape_reader *reader;
    tickertape_record record;
    uint64_t seq, next = 0, position = 0;
    join(dir, sizeof dir, scratch, "roundtrip");

    writer = open_writer(dir, FIXTURE_SEGMENT_SIZE);
    CHECK_STATUS(tickertape_writer_open(dir, NULL, &second), TICKERTAPE_E_LOCKED);
    CHECK(second == NULL);
    for (seq = 0; seq < 400; seq++) append_pattern(writer, seq);
    REQUIRE_OK(tickertape_writer_next_seq(writer, &next));
    CHECK(next == 400);

    reader = open_reader(dir, 0, 0);
    for (seq = 0; seq < 400; seq++) {
        CHECK_STATUS(tickertape_reader_next(reader, TICKERTAPE_NO_WAIT, &record), TICKERTAPE_OK);
        CHECK(pattern_matches(&record, seq));
    }
    record.seq = 12345;
    CHECK_STATUS(tickertape_reader_next(reader, TICKERTAPE_NO_WAIT, &record), TICKERTAPE_NONE);
    CHECK(record.seq == 12345); /* untouched */
    CHECK_STATUS(tickertape_reader_next(reader, 2000000 /* 2 ms */, &record), TICKERTAPE_NONE);
    REQUIRE_OK(tickertape_reader_position(reader, &position));
    CHECK(position == 400);
    append_pattern(writer, 400);
    CHECK_STATUS(tickertape_reader_next(reader, TICKERTAPE_WAIT_FOREVER, &record), TICKERTAPE_OK);
    CHECK(pattern_matches(&record, 400));
    REQUIRE_OK(tickertape_reader_seek(reader, 7));
    CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
    CHECK(pattern_matches(&record, 7));
    REQUIRE_OK(tickertape_reader_close(reader));

    /* Zero-copy append through a callback; a callback that aborts commits nothing. */
    {
        struct fill_ctx ctx;
        memset(&ctx, 0, sizeof ctx);
        ctx.writer = writer;
        ctx.seq = 401;
        CHECK_STATUS(tickertape_writer_append_with(writer, (uint32_t)(401 % 5 + 1), 1000000 + 401, 2000000 + 3 * 401,
                                                   pattern_len(401), fill_pattern, &ctx, &seq),
                     TICKERTAPE_OK);
        CHECK(seq == 401 && ctx.calls == 1);
        CHECK(ctx.reentrant == TICKERTAPE_E_BUSY);
        ctx.abort = 1;
        CHECK_STATUS(tickertape_writer_append_with(writer, 1, 0, 0, 10, fill_pattern, &ctx, &seq),
                     TICKERTAPE_E_ABORTED);
        CHECK_STATUS(tickertape_writer_append_with(writer, 1, 0, 0, 10, NULL, &ctx, &seq),
                     TICKERTAPE_E_INVALID_ARGUMENT);
        REQUIRE_OK(tickertape_writer_next_seq(writer, &next));
        CHECK(next == 402);
    }

    /* Batch append: argument errors append nothing; runtime errors report how many were appended. */
    {
        tickertape_new_record batch[3];
        size_t appended = 99, i;
        uint64_t first = 0;
        static uint8_t big[70000];
        uint8_t bufs[3][300];
        for (i = 0; i < 3; i++) {
            uint64_t s = 402 + i;
            pattern_fill(s, bufs[i], pattern_len(s));
            batch[i].payload = bufs[i];
            batch[i].payload_len = pattern_len(s);
            batch[i].venue_ts_ns = 1000000 + s;
            batch[i].local_ts_ns = 2000000 + 3 * s;
            batch[i].schema_id = (uint32_t)(s % 5 + 1);
            batch[i].reserved = 0;
        }
        batch[1].reserved = 9;
        CHECK_STATUS(tickertape_writer_append_batch(writer, batch, 3, &appended, &first),
                     TICKERTAPE_E_INVALID_ARGUMENT);
        CHECK(appended == 0);
        batch[1].reserved = 0;
        CHECK_STATUS(tickertape_writer_append_batch(writer, batch, 3, &appended, &first), TICKERTAPE_OK);
        CHECK(appended == 3 && first == 402);
        batch[1].payload = big;
        batch[1].payload_len = sizeof big;
        CHECK_STATUS(tickertape_writer_append_batch(writer, batch, 3, &appended, &first),
                     TICKERTAPE_E_RECORD_TOO_LARGE);
        CHECK(appended == 1 && first == 405);
    }

    /* Copy-out and batch reads into caller buffers. */
    {
        tickertape_record records[8];
        uint8_t small[16];
        uint8_t buf[4096];
        size_t count = 0;
        reader = open_reader(dir, 0, 0);
        REQUIRE_OK(tickertape_reader_seek(reader, 401));
        CHECK_STATUS(tickertape_reader_next_copy(reader, 0, small, sizeof small, &record),
                     TICKERTAPE_E_BUFFER_TOO_SMALL);
        CHECK(record.seq == 401 && record.payload_len == pattern_len(401) && record.payload == NULL);
        CHECK_STATUS(tickertape_reader_next_copy(reader, 0, buf, sizeof buf, &record), TICKERTAPE_OK);
        CHECK(pattern_matches(&record, 401) && record.payload == buf);
        CHECK_STATUS(tickertape_reader_read_batch(reader, 0, records, 8, buf, sizeof buf, &count), TICKERTAPE_OK);
        CHECK(count == 4); /* 402, 403, 404 and the 405 appended before the batch failed */
        for (seq = 0; seq < 3; seq++) CHECK(pattern_matches(&records[seq], 402 + seq));
        CHECK(records[3].seq == 405);
        CHECK_STATUS(tickertape_reader_read_batch(reader, 0, records, 8, buf, sizeof buf, &count), TICKERTAPE_NONE);
        CHECK(count == 0);
        REQUIRE_OK(tickertape_reader_close(reader));
    }

    /* Durable-only readers see only checkpointed records. */
    {
        tickertape_checkpoint checkpoint;
        reader = open_reader(dir, 1, 0);
        CHECK_STATUS(tickertape_read_checkpoint(dir, &checkpoint), TICKERTAPE_NONE);
        CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_NONE);
        REQUIRE_OK(tickertape_writer_checkpoint(writer));
        REQUIRE_OK(tickertape_read_checkpoint(dir, &checkpoint));
        CHECK(checkpoint.next_seq == 406 && checkpoint.generation == 1);
        append_pattern(writer, 406);
        for (seq = 0; seq < 406; seq++) {
            CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
            CHECK(record.seq == seq);
        }
        CHECK_STATUS(tickertape_reader_next(reader, 1000000, &record), TICKERTAPE_NONE);
        REQUIRE_OK(tickertape_reader_close(reader));
    }
    REQUIRE_OK(tickertape_writer_close(writer));
}

static void test_corruption_and_inspection(const char *scratch) {
    char dir[4096], segment[4096 + 32];
    tickertape_writer *writer;
    tickertape_reader *reader;
    tickertape_record record;
    tickertape_verify_report report;
    tickertape_segment_info segments[4];
    size_t count = 0;
    uint64_t seq;
    int fd;
    uint8_t byte;
    join(dir, sizeof dir, scratch, "corrupt");

    writer = open_writer(dir, FIXTURE_SEGMENT_SIZE);
    for (seq = 0; seq < 5; seq++) {
        static const uint8_t payload[8] = {1, 2, 3, 4, 5, 6, 7, 8};
        REQUIRE_OK(tickertape_writer_append(writer, 1, 0, 0, payload, sizeof payload, NULL));
    }
    REQUIRE_OK(tickertape_writer_close(writer));

    CHECK_STATUS(tickertape_list_segments(dir, NULL, 0, &count), TICKERTAPE_OK);
    CHECK(count == 1);
    CHECK_STATUS(tickertape_list_segments(dir, segments, 4, &count), TICKERTAPE_OK);
    CHECK(segments[0].base_seq == 0 && segments[0].segment_size == FIXTURE_SEGMENT_SIZE &&
          segments[0].format_version == 1);

    /* Flip one payload byte of record 2 (48-byte records from offset 64). */
    snprintf(segment, sizeof segment, "%s/%020d.tape", dir, 0);
    fd = open(segment, O_RDWR);
    CHECK(fd >= 0);
    CHECK(pread(fd, &byte, 1, 64 + 48 * 2 + 40) == 1);
    byte ^= 0xFF;
    CHECK(pwrite(fd, &byte, 1, 64 + 48 * 2 + 40) == 1);
    close(fd);

    reader = open_reader(dir, 0, 0);
    CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
    CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
    CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_E_CORRUPT);
    CHECK(tickertape_last_error_corrupt_reason() == TICKERTAPE_CORRUPT_BAD_CHECKSUM);
    CHECK(strstr(tickertape_last_error_message(), "checksum") != NULL);
    REQUIRE_OK(tickertape_reader_position(reader, &seq));
    CHECK(seq == 2);
    REQUIRE_OK(tickertape_reader_close(reader));

    memset(&report, 0xAB, sizeof report);
    report.struct_size = 8;
    CHECK_STATUS(tickertape_verify(dir, &report), TICKERTAPE_E_INVALID_ARGUMENT);
    report.struct_size = sizeof report;
    CHECK_STATUS(tickertape_verify(dir, &report), TICKERTAPE_OK);
    CHECK(report.struct_size == sizeof report);
    CHECK(report.records == 2 && report.problems == 1);
    CHECK(report.first_problem_base_seq == 0 && report.first_problem_offset == 64 + 48 * 2);
    CHECK(report.first_problem_reason == TICKERTAPE_CORRUPT_BAD_CHECKSUM);

    /* A struct from a newer header with a field this library does not know: the library fills only its own part
     * and says so in struct_size, so the caller knows the new field was not written. */
    {
        struct newer_verify_report {
            tickertape_verify_report v1;
            uint64_t added_later;
        } newer;
        memset(&newer, 0xCD, sizeof newer);
        newer.v1.struct_size = sizeof newer;
        CHECK_STATUS(tickertape_verify(dir, &newer.v1), TICKERTAPE_OK);
        CHECK(newer.v1.struct_size == sizeof(tickertape_verify_report));
        CHECK(newer.v1.records == 2);
        CHECK(newer.added_later == 0xCDCDCDCDCDCDCDCDull);
    }
}

static int run_basic(const char *scratch) {
    test_versions_and_names();
    test_misuse(scratch);
    test_roundtrip(scratch);
    test_corruption_and_inspection(scratch);
    return failures;
}

/* ---- fixture modes ---- */

#define FIXTURE_RECORDS 1500u
#define FIXTURE_CHECKPOINT_AT 1000u

static int run_write_fixture(const char *dir) {
    tickertape_writer *writer = open_writer(dir, FIXTURE_SEGMENT_SIZE);
    uint64_t seq;
    for (seq = 0; seq < FIXTURE_RECORDS; seq++) {
        if (seq == FIXTURE_CHECKPOINT_AT) REQUIRE_OK(tickertape_writer_checkpoint(writer));
        if (seq % 2 == 0) {
            append_pattern(writer, seq);
        } else {
            struct fill_ctx ctx;
            uint64_t got = 0;
            memset(&ctx, 0, sizeof ctx);
            ctx.writer = writer;
            ctx.seq = seq;
            REQUIRE_OK(tickertape_writer_append_with(writer, (uint32_t)(seq % 5 + 1), 1000000 + seq,
                                                     2000000 + 3 * seq, pattern_len(seq), fill_pattern, &ctx, &got));
            CHECK(got == seq);
        }
    }
    REQUIRE_OK(tickertape_writer_close(writer)); /* records after the checkpoint stay visible, not durable */
    return failures;
}

static int run_read_fixture(const char *dir, uint64_t count, uint64_t durable_next) {
    int mapped;
    uint64_t seq;
    tickertape_record record;
    tickertape_checkpoint checkpoint;
    tickertape_verify_report report;
    size_t segments = 0;

    REQUIRE_OK(tickertape_read_checkpoint(dir, &checkpoint));
    CHECK(checkpoint.next_seq == durable_next);
    REQUIRE_OK(tickertape_list_segments(dir, NULL, 0, &segments));
    CHECK(segments >= 2); /* sequence boundaries across a roll are covered */
    memset(&report, 0, sizeof report);
    report.struct_size = sizeof report;
    REQUIRE_OK(tickertape_verify(dir, &report));
    CHECK(report.problems == 0 && report.records == count && report.first_seq == 0 && report.last_seq == count - 1);

    for (mapped = 0; mapped <= 1; mapped++) {
        tickertape_reader *reader = open_reader(dir, 0, mapped);
        for (seq = 0; seq < count; seq++) {
            CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
            if (!pattern_matches(&record, seq)) failures++;
        }
        CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_NONE);
        REQUIRE_OK(tickertape_reader_close(reader));
    }

    /* Batches across segment boundaries, with a buffer small enough to force hold-backs. */
    {
        tickertape_reader *reader = open_reader(dir, 0, 0);
        tickertape_record records[64];
        uint8_t buf[2048];
        size_t n = 0, i;
        uint64_t expected = 0;
        tickertape_status status;
        while ((status = tickertape_reader_read_batch(reader, 0, records, 64, buf, sizeof buf, &n)) == TICKERTAPE_OK) {
            for (i = 0; i < n; i++) {
                if (!pattern_matches(&records[i], expected)) failures++;
                expected++;
            }
        }
        CHECK(status == TICKERTAPE_NONE);
        CHECK(expected == count);
        REQUIRE_OK(tickertape_reader_close(reader));
    }

    /* Durable-only: exactly the records below the checkpoint. */
    {
        tickertape_reader *reader = open_reader(dir, 1, 1);
        for (seq = 0; seq < durable_next; seq++) {
            CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_OK);
            if (!pattern_matches(&record, seq)) failures++;
        }
        CHECK_STATUS(tickertape_reader_next(reader, 0, &record), TICKERTAPE_NONE);
        REQUIRE_OK(tickertape_reader_position(reader, &seq));
        CHECK(seq == durable_next);
        REQUIRE_OK(tickertape_reader_close(reader));
    }
    return failures;
}

int main(int argc, char **argv) {
    int result;
    if (!library_is_compatible()) {
        fprintf(stderr, "the library (%s) is not compatible with this header (%d.%d.%d)\n", tickertape_version(),
                TICKERTAPE_VERSION_MAJOR, TICKERTAPE_VERSION_MINOR, TICKERTAPE_VERSION_PATCH);
        return 1;
    }
    if (argc == 3 && strcmp(argv[1], "basic") == 0) {
        result = run_basic(argv[2]);
    } else if (argc == 3 && strcmp(argv[1], "write-fixture") == 0) {
        result = run_write_fixture(argv[2]);
    } else if (argc == 5 && strcmp(argv[1], "read-fixture") == 0) {
        result = run_read_fixture(argv[2], strtoull(argv[3], NULL, 10), strtoull(argv[4], NULL, 10));
    } else {
        fprintf(stderr, "usage: %s basic|write-fixture|read-fixture <dir> [count durable_next]\n", argv[0]);
        return 2;
    }
    if (result != 0) {
        fprintf(stderr, "%d check(s) failed\n", result);
        return 1;
    }
    printf("ok\n");
    return 0;
}
