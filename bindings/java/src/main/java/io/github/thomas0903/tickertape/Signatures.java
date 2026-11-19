package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.StructLayout;
import java.lang.foreign.ValueLayout;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;

/**
 * The C declarations this binding uses, as FFM descriptors and layouts. Pure data: nothing here touches the library,
 * so tests can compare it with the header without loading anything.
 *
 * <p>C types map as follows on the supported (LP64) platforms: {@code uint32_t}, {@code int32_t} and
 * {@code tickertape_status} to {@code JAVA_INT}; {@code uint64_t}, {@code int64_t} and {@code size_t} to
 * {@code JAVA_LONG}; every pointer to {@code ADDRESS}. {@code NativeLibrary} refuses a platform where pointers or
 * {@code size_t} are not 8 bytes.
 */
final class Signatures {
    private Signatures() {}

    /** {@code size_t}: 8 bytes on every supported platform (checked at load). */
    static final ValueLayout.OfLong SIZE_T = JAVA_LONG;

    /** {@code tickertape_record}, 48 bytes, frozen for format version 1. */
    static final StructLayout RECORD = MemoryLayout.structLayout(
            JAVA_LONG.withName("seq"),
            JAVA_LONG.withName("venue_ts_ns"),
            JAVA_LONG.withName("local_ts_ns"),
            ADDRESS.withName("payload"),
            SIZE_T.withName("payload_len"),
            JAVA_INT.withName("schema_id"),
            JAVA_INT.withName("reserved")).withName("tickertape_record");

    /** {@code tickertape_new_record}, 40 bytes, frozen for format version 1. */
    static final StructLayout NEW_RECORD = MemoryLayout.structLayout(
            ADDRESS.withName("payload"),
            SIZE_T.withName("payload_len"),
            JAVA_LONG.withName("venue_ts_ns"),
            JAVA_LONG.withName("local_ts_ns"),
            JAVA_INT.withName("schema_id"),
            JAVA_INT.withName("reserved")).withName("tickertape_new_record");

    /** {@code tickertape_segment_info}, 32 bytes, frozen for format version 1. */
    static final StructLayout SEGMENT_INFO = MemoryLayout.structLayout(
            JAVA_LONG.withName("base_seq"),
            JAVA_LONG.withName("segment_size"),
            JAVA_LONG.withName("created_unix_ns"),
            JAVA_INT.withName("format_version"),
            JAVA_INT.withName("reserved")).withName("tickertape_segment_info");

    /** {@code tickertape_checkpoint}, 32 bytes, frozen for format version 1. */
    static final StructLayout CHECKPOINT = MemoryLayout.structLayout(
            JAVA_LONG.withName("generation"),
            JAVA_LONG.withName("next_seq"),
            JAVA_LONG.withName("segment_base_seq"),
            JAVA_LONG.withName("offset")).withName("tickertape_checkpoint");

    /** {@code tickertape_verify_report}, an extensible struct; 72 bytes in this ABI version. */
    static final StructLayout VERIFY_REPORT = MemoryLayout.structLayout(
            JAVA_LONG.withName("struct_size"),
            JAVA_LONG.withName("segments"),
            JAVA_LONG.withName("records"),
            JAVA_LONG.withName("first_seq"),
            JAVA_LONG.withName("last_seq"),
            JAVA_LONG.withName("problems"),
            JAVA_LONG.withName("first_problem_base_seq"),
            JAVA_LONG.withName("first_problem_offset"),
            JAVA_INT.withName("first_problem_reason"),
            JAVA_INT.withName("reserved")).withName("tickertape_verify_report");

    /** {@code tickertape_retention_report}, an extensible struct; 32 bytes in this ABI version. */
    static final StructLayout RETENTION_REPORT = MemoryLayout.structLayout(
            JAVA_LONG.withName("struct_size"),
            JAVA_LONG.withName("removed_segments"),
            JAVA_LONG.withName("removed_bytes"),
            JAVA_LONG.withName("already_removed_segments")).withName("tickertape_retention_report");

    /** Every struct, by its C name (for the layout test). */
    static final Map<String, StructLayout> STRUCTS = Map.of(
            "tickertape_record", RECORD,
            "tickertape_new_record", NEW_RECORD,
            "tickertape_segment_info", SEGMENT_INFO,
            "tickertape_checkpoint", CHECKPOINT,
            "tickertape_verify_report", VERIFY_REPORT,
            "tickertape_retention_report", RETENTION_REPORT);

    /** {@code tickertape_fill_fn}: {@code int32_t (*)(void *user_data, uint8_t *buf, size_t len)}. */
    static final FunctionDescriptor FILL_FN = FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, SIZE_T);

    /**
     * The C functions this binding calls, with their descriptors. Every function in the header is either here or in
     * {@code docs/decisions/0037-java-binding.md}'s list of functions deliberately not wrapped; a test checks both
     * against the header.
     */
    static final Map<String, FunctionDescriptor> FUNCTIONS;

    /** The header functions that the binding deliberately does not call (ADR 0037 gives the reasons). */
    static final Set<String> NOT_WRAPPED = Set.of(
            "tickertape_reader_next_copy",
            "tickertape_status_name",
            "tickertape_corrupt_reason_name");

    static {
        Map<String, FunctionDescriptor> f = new LinkedHashMap<>();
        // Library and errors.
        f.put("tickertape_abi_version", FunctionDescriptor.of(JAVA_INT));
        f.put("tickertape_version_number", FunctionDescriptor.of(JAVA_INT));
        f.put("tickertape_version", FunctionDescriptor.of(ADDRESS));
        f.put("tickertape_realtime_ns", FunctionDescriptor.of(JAVA_LONG));
        f.put("tickertape_last_error_code", FunctionDescriptor.of(JAVA_INT));
        f.put("tickertape_last_error_message", FunctionDescriptor.of(ADDRESS));
        f.put("tickertape_last_error_os_code", FunctionDescriptor.of(JAVA_INT));
        f.put("tickertape_last_error_corrupt_reason", FunctionDescriptor.of(JAVA_INT));
        // Inspection.
        f.put("tickertape_verify", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        f.put("tickertape_list_segments", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, SIZE_T, ADDRESS));
        f.put("tickertape_read_checkpoint", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        f.put("tickertape_retention_apply", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, JAVA_INT, ADDRESS));
        // Writer options.
        f.put("tickertape_writer_options_new", FunctionDescriptor.of(ADDRESS));
        f.put("tickertape_writer_options_free", FunctionDescriptor.ofVoid(ADDRESS));
        f.put("tickertape_writer_options_set_segment_size", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG));
        f.put("tickertape_writer_options_set_recovery_policy", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT));
        f.put("tickertape_writer_options_set_notify", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT));
        f.put("tickertape_writer_options_set_prefault", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT));
        f.put("tickertape_writer_options_set_retention", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        // Reader options.
        f.put("tickertape_reader_options_new", FunctionDescriptor.of(ADDRESS));
        f.put("tickertape_reader_options_free", FunctionDescriptor.ofVoid(ADDRESS));
        f.put("tickertape_reader_options_set_durable_only", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT));
        f.put("tickertape_reader_options_set_notify", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT));
        f.put("tickertape_reader_options_set_backoff",
                FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT, JAVA_INT, JAVA_LONG, JAVA_LONG));
        f.put("tickertape_reader_options_set_busy_spin", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        // Retention settings.
        f.put("tickertape_retention_new", FunctionDescriptor.of(ADDRESS));
        f.put("tickertape_retention_free", FunctionDescriptor.ofVoid(ADDRESS));
        f.put("tickertape_retention_set_max_segments", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG));
        f.put("tickertape_retention_set_max_bytes", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG));
        f.put("tickertape_retention_set_max_age_ns", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG));
        f.put("tickertape_retention_set_delete", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        f.put("tickertape_retention_set_move_to", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        // Reader.
        f.put("tickertape_reader_open", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, ADDRESS));
        f.put("tickertape_reader_open_mapped_unsafe", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, ADDRESS));
        f.put("tickertape_reader_close", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        f.put("tickertape_reader_seek", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG));
        f.put("tickertape_reader_next", FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG, ADDRESS));
        f.put("tickertape_reader_read_batch",
                FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_LONG, ADDRESS, SIZE_T, ADDRESS, SIZE_T, ADDRESS));
        f.put("tickertape_reader_position", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        // Writer.
        f.put("tickertape_writer_open", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, ADDRESS));
        f.put("tickertape_writer_close", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        f.put("tickertape_writer_append",
                FunctionDescriptor.of(JAVA_INT, ADDRESS, JAVA_INT, JAVA_LONG, JAVA_LONG, ADDRESS, SIZE_T, ADDRESS));
        f.put("tickertape_writer_append_with", FunctionDescriptor.of(
                JAVA_INT, ADDRESS, JAVA_INT, JAVA_LONG, JAVA_LONG, SIZE_T, ADDRESS, ADDRESS, ADDRESS));
        f.put("tickertape_writer_append_batch",
                FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS, SIZE_T, ADDRESS, ADDRESS));
        f.put("tickertape_writer_checkpoint", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        f.put("tickertape_writer_next_seq", FunctionDescriptor.of(JAVA_INT, ADDRESS, ADDRESS));
        f.put("tickertape_writer_last_retention_error", FunctionDescriptor.of(JAVA_INT, ADDRESS));
        FUNCTIONS = Collections.unmodifiableMap(f);
    }
}
