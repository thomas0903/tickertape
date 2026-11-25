package io.github.thomas0903.tickertape;

import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.invoke.MethodHandle;

/**
 * Downcall handles for the C functions in {@link Signatures#FUNCTIONS}, one {@code static final} handle each (so the
 * JIT compiler can inline the call) and a typed wrapper around each. Generated from {@code Signatures} by hand-kept
 * code; {@code HeaderTest} checks that the descriptors match the header.
 *
 * <p>Initialising this class requires a loaded, checked library: callers run {@link NativeLibrary#lookup()} first,
 * which reports a missing or incompatible library as an {@link UnsatisfiedLinkError} (cached), so this class's
 * initialisation itself cannot fail.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
final class Native {
    private Native() {}

    private static final Linker LINKER = Linker.nativeLinker();
    private static final SymbolLookup SYMBOLS = NativeLibrary.lookup();

    private static MethodHandle h(String name) {
        return LINKER.downcallHandle(SYMBOLS.find(name).orElseThrow(), Signatures.FUNCTIONS.get(name));
    }

    /** Rethrows what a downcall threw: only unchecked exceptions are possible, but invokeExact declares Throwable. */
    private static RuntimeException rethrow(Throwable t) {
        if (t instanceof RuntimeException r) {
            throw r;
        }
        if (t instanceof Error e) {
            throw e;
        }
        throw new IllegalStateException(t);
    }

    private static final MethodHandle ABI_VERSION = h("tickertape_abi_version");
    private static final MethodHandle VERSION_NUMBER = h("tickertape_version_number");
    private static final MethodHandle VERSION = h("tickertape_version");
    private static final MethodHandle REALTIME_NS = h("tickertape_realtime_ns");
    private static final MethodHandle LAST_ERROR_CODE = h("tickertape_last_error_code");
    private static final MethodHandle LAST_ERROR_MESSAGE = h("tickertape_last_error_message");
    private static final MethodHandle LAST_ERROR_OS_CODE = h("tickertape_last_error_os_code");
    private static final MethodHandle LAST_ERROR_CORRUPT_REASON = h("tickertape_last_error_corrupt_reason");
    private static final MethodHandle JOURNAL_STATUS_SAMPLE = h("tickertape_journal_status_sample");
    private static final MethodHandle JOURNAL_STATUS_FREE = h("tickertape_journal_status_free");
    private static final MethodHandle JOURNAL_STATUS_GET_REPORT = h("tickertape_journal_status_get_report");
    private static final MethodHandle JOURNAL_STATUS_GET_BEACON = h("tickertape_journal_status_get_beacon");
    private static final MethodHandle JOURNAL_STATUS_BEACON_NAME = h("tickertape_journal_status_beacon_name");
    private static final MethodHandle WRITER_GET_METRICS = h("tickertape_writer_get_metrics");
    private static final MethodHandle READER_GET_METRICS = h("tickertape_reader_get_metrics");
    private static final MethodHandle VERIFY = h("tickertape_verify");
    private static final MethodHandle LIST_SEGMENTS = h("tickertape_list_segments");
    private static final MethodHandle READ_CHECKPOINT = h("tickertape_read_checkpoint");
    private static final MethodHandle RETENTION_APPLY = h("tickertape_retention_apply");
    private static final MethodHandle WRITER_OPTIONS_NEW = h("tickertape_writer_options_new");
    private static final MethodHandle WRITER_OPTIONS_FREE = h("tickertape_writer_options_free");
    private static final MethodHandle WRITER_OPTIONS_SET_SEGMENT_SIZE = h("tickertape_writer_options_set_segment_size");
    private static final MethodHandle WRITER_OPTIONS_SET_RECOVERY_POLICY = h("tickertape_writer_options_set_recovery_policy");
    private static final MethodHandle WRITER_OPTIONS_SET_NOTIFY = h("tickertape_writer_options_set_notify");
    private static final MethodHandle WRITER_OPTIONS_SET_PREFAULT = h("tickertape_writer_options_set_prefault");
    private static final MethodHandle WRITER_OPTIONS_SET_EARLY_WRITEBACK = h("tickertape_writer_options_set_early_writeback");
    private static final MethodHandle WRITER_OPTIONS_SET_HELPER_CPUS = h("tickertape_writer_options_set_helper_cpus");
    private static final MethodHandle WRITER_OPTIONS_SET_RETENTION = h("tickertape_writer_options_set_retention");
    private static final MethodHandle READER_OPTIONS_NEW = h("tickertape_reader_options_new");
    private static final MethodHandle READER_OPTIONS_FREE = h("tickertape_reader_options_free");
    private static final MethodHandle READER_OPTIONS_SET_DURABLE_ONLY = h("tickertape_reader_options_set_durable_only");
    private static final MethodHandle READER_OPTIONS_SET_SEEK_INDEX = h("tickertape_reader_options_set_seek_index");
    private static final MethodHandle READER_OPTIONS_SET_NOTIFY = h("tickertape_reader_options_set_notify");
    private static final MethodHandle READER_OPTIONS_SET_BACKOFF = h("tickertape_reader_options_set_backoff");
    private static final MethodHandle READER_OPTIONS_SET_BUSY_SPIN = h("tickertape_reader_options_set_busy_spin");
    private static final MethodHandle RETENTION_NEW = h("tickertape_retention_new");
    private static final MethodHandle RETENTION_FREE = h("tickertape_retention_free");
    private static final MethodHandle RETENTION_SET_MAX_SEGMENTS = h("tickertape_retention_set_max_segments");
    private static final MethodHandle RETENTION_SET_MAX_BYTES = h("tickertape_retention_set_max_bytes");
    private static final MethodHandle RETENTION_SET_MAX_AGE_NS = h("tickertape_retention_set_max_age_ns");
    private static final MethodHandle RETENTION_SET_DELETE = h("tickertape_retention_set_delete");
    private static final MethodHandle RETENTION_SET_MOVE_TO = h("tickertape_retention_set_move_to");
    private static final MethodHandle READER_OPEN = h("tickertape_reader_open");
    private static final MethodHandle READER_OPEN_MAPPED_UNSAFE = h("tickertape_reader_open_mapped_unsafe");
    private static final MethodHandle READER_CLOSE = h("tickertape_reader_close");
    private static final MethodHandle READER_SEEK = h("tickertape_reader_seek");
    private static final MethodHandle READER_SEEK_TO_END = h("tickertape_reader_seek_to_end");
    private static final MethodHandle READER_NEXT = h("tickertape_reader_next");
    private static final MethodHandle READER_READ_BATCH = h("tickertape_reader_read_batch");
    private static final MethodHandle READER_POSITION = h("tickertape_reader_position");
    private static final MethodHandle WRITER_OPEN = h("tickertape_writer_open");
    private static final MethodHandle WRITER_CLOSE = h("tickertape_writer_close");
    private static final MethodHandle WRITER_APPEND = h("tickertape_writer_append");
    private static final MethodHandle WRITER_APPEND_WITH = h("tickertape_writer_append_with");
    private static final MethodHandle WRITER_APPEND_BATCH = h("tickertape_writer_append_batch");
    private static final MethodHandle WRITER_CHECKPOINT = h("tickertape_writer_checkpoint");
    private static final MethodHandle WRITER_REQUEST_CHECKPOINT = h("tickertape_writer_request_checkpoint");
    private static final MethodHandle WRITER_CHECKPOINT_PROGRESS = h("tickertape_writer_checkpoint_progress");
    private static final MethodHandle WRITER_NEXT_SEQ = h("tickertape_writer_next_seq");
    private static final MethodHandle WRITER_LAST_RETENTION_ERROR = h("tickertape_writer_last_retention_error");

    static int abiVersion() {
        try {
            return (int) ABI_VERSION.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int versionNumber() {
        try {
            return (int) VERSION_NUMBER.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment version() {
        try {
            return (MemorySegment) VERSION.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static long realtimeNs() {
        try {
            return (long) REALTIME_NS.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int lastErrorCode() {
        try {
            return (int) LAST_ERROR_CODE.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment lastErrorMessage() {
        try {
            return (MemorySegment) LAST_ERROR_MESSAGE.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int lastErrorOsCode() {
        try {
            return (int) LAST_ERROR_OS_CODE.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int lastErrorCorruptReason() {
        try {
            return (int) LAST_ERROR_CORRUPT_REASON.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int verify(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) VERIFY.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int listSegments(MemorySegment a0, MemorySegment a1, long a2, MemorySegment a3) {
        try {
            return (int) LIST_SEGMENTS.invokeExact(a0, a1, a2, a3);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readCheckpoint(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) READ_CHECKPOINT.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionApply(MemorySegment a0, MemorySegment a1, int a2, MemorySegment a3) {
        try {
            return (int) RETENTION_APPLY.invokeExact(a0, a1, a2, a3);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment writerOptionsNew() {
        try {
            return (MemorySegment) WRITER_OPTIONS_NEW.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static void writerOptionsFree(MemorySegment a0) {
        try {
            WRITER_OPTIONS_FREE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetSegmentSize(MemorySegment a0, long a1) {
        try {
            return (int) WRITER_OPTIONS_SET_SEGMENT_SIZE.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetRecoveryPolicy(MemorySegment a0, int a1) {
        try {
            return (int) WRITER_OPTIONS_SET_RECOVERY_POLICY.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetNotify(MemorySegment a0, int a1) {
        try {
            return (int) WRITER_OPTIONS_SET_NOTIFY.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetPrefault(MemorySegment a0, int a1) {
        try {
            return (int) WRITER_OPTIONS_SET_PREFAULT.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetEarlyWriteback(MemorySegment a0, int a1) {
        try {
            return (int) WRITER_OPTIONS_SET_EARLY_WRITEBACK.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetHelperCpus(MemorySegment a0, int a1, MemorySegment a2, long a3) {
        try {
            return (int) WRITER_OPTIONS_SET_HELPER_CPUS.invokeExact(a0, a1, a2, a3);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOptionsSetRetention(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) WRITER_OPTIONS_SET_RETENTION.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment readerOptionsNew() {
        try {
            return (MemorySegment) READER_OPTIONS_NEW.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static void readerOptionsFree(MemorySegment a0) {
        try {
            READER_OPTIONS_FREE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOptionsSetDurableOnly(MemorySegment a0, int a1) {
        try {
            return (int) READER_OPTIONS_SET_DURABLE_ONLY.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOptionsSetSeekIndex(MemorySegment a0, int a1) {
        try {
            return (int) READER_OPTIONS_SET_SEEK_INDEX.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOptionsSetNotify(MemorySegment a0, int a1) {
        try {
            return (int) READER_OPTIONS_SET_NOTIFY.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOptionsSetBackoff(MemorySegment a0, int a1, int a2, long a3, long a4) {
        try {
            return (int) READER_OPTIONS_SET_BACKOFF.invokeExact(a0, a1, a2, a3, a4);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOptionsSetBusySpin(MemorySegment a0) {
        try {
            return (int) READER_OPTIONS_SET_BUSY_SPIN.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment retentionNew() {
        try {
            return (MemorySegment) RETENTION_NEW.invokeExact();
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static void retentionFree(MemorySegment a0) {
        try {
            RETENTION_FREE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionSetMaxSegments(MemorySegment a0, long a1) {
        try {
            return (int) RETENTION_SET_MAX_SEGMENTS.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionSetMaxBytes(MemorySegment a0, long a1) {
        try {
            return (int) RETENTION_SET_MAX_BYTES.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionSetMaxAgeNs(MemorySegment a0, long a1) {
        try {
            return (int) RETENTION_SET_MAX_AGE_NS.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionSetDelete(MemorySegment a0) {
        try {
            return (int) RETENTION_SET_DELETE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int retentionSetMoveTo(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) RETENTION_SET_MOVE_TO.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOpen(MemorySegment a0, MemorySegment a1, MemorySegment a2) {
        try {
            return (int) READER_OPEN.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerOpenMappedUnsafe(MemorySegment a0, MemorySegment a1, MemorySegment a2) {
        try {
            return (int) READER_OPEN_MAPPED_UNSAFE.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerClose(MemorySegment a0) {
        try {
            return (int) READER_CLOSE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerSeek(MemorySegment a0, long a1) {
        try {
            return (int) READER_SEEK.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerSeekToEnd(MemorySegment a0) {
        try {
            return (int) READER_SEEK_TO_END.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerNext(MemorySegment a0, long a1, MemorySegment a2) {
        try {
            return (int) READER_NEXT.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerReadBatch(MemorySegment a0, long a1, MemorySegment a2, long a3, MemorySegment a4, long a5, MemorySegment a6) {
        try {
            return (int) READER_READ_BATCH.invokeExact(a0, a1, a2, a3, a4, a5, a6);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerPosition(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) READER_POSITION.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerOpen(MemorySegment a0, MemorySegment a1, MemorySegment a2) {
        try {
            return (int) WRITER_OPEN.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerClose(MemorySegment a0) {
        try {
            return (int) WRITER_CLOSE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerAppend(MemorySegment a0, int a1, long a2, long a3, MemorySegment a4, long a5, MemorySegment a6) {
        try {
            return (int) WRITER_APPEND.invokeExact(a0, a1, a2, a3, a4, a5, a6);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerAppendWith(MemorySegment a0, int a1, long a2, long a3, long a4, MemorySegment a5, MemorySegment a6, MemorySegment a7) {
        try {
            return (int) WRITER_APPEND_WITH.invokeExact(a0, a1, a2, a3, a4, a5, a6, a7);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerAppendBatch(MemorySegment a0, MemorySegment a1, long a2, MemorySegment a3, MemorySegment a4) {
        try {
            return (int) WRITER_APPEND_BATCH.invokeExact(a0, a1, a2, a3, a4);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerCheckpoint(MemorySegment a0) {
        try {
            return (int) WRITER_CHECKPOINT.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerRequestCheckpoint(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) WRITER_REQUEST_CHECKPOINT.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerCheckpointProgress(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) WRITER_CHECKPOINT_PROGRESS.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerNextSeq(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) WRITER_NEXT_SEQ.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerLastRetentionError(MemorySegment a0) {
        try {
            return (int) WRITER_LAST_RETENTION_ERROR.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int journalStatusSample(MemorySegment a0, MemorySegment a1, MemorySegment a2) {
        try {
            return (int) JOURNAL_STATUS_SAMPLE.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static void journalStatusFree(MemorySegment a0) {
        try {
            JOURNAL_STATUS_FREE.invokeExact(a0);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int journalStatusGetReport(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) JOURNAL_STATUS_GET_REPORT.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int journalStatusGetBeacon(MemorySegment a0, long a1, MemorySegment a2) {
        try {
            return (int) JOURNAL_STATUS_GET_BEACON.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static MemorySegment journalStatusBeaconName(MemorySegment a0, long a1) {
        try {
            return (MemorySegment) JOURNAL_STATUS_BEACON_NAME.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int writerGetMetrics(MemorySegment a0, MemorySegment a1) {
        try {
            return (int) WRITER_GET_METRICS.invokeExact(a0, a1);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }

    static int readerGetMetrics(MemorySegment a0, MemorySegment a1, MemorySegment a2) {
        try {
            return (int) READER_GET_METRICS.invokeExact(a0, a1, a2);
        } catch (Throwable t) {
            throw rethrow(t);
        }
    }
}
