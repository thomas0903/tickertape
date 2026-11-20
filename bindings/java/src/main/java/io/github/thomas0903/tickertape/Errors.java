package io.github.thomas0903.tickertape;

/**
 * Maps C status codes to exceptions.
 *
 * <p>The C ABI keeps the details of the last error (message, OS error, corruption reason) per operating-system
 * thread. {@link #capture} reads them right after the failing call, in the same sequence of downcalls with no Java
 * code in between that can block. A virtual thread is only unmounted from its carrier when it blocks (or yields), and
 * a downcall pins it for its duration, so the sequence runs on one OS thread. As a check, the thread's last error code
 * must equal the status the call returned; if it does not, the exception says the details are unavailable rather
 * than reporting another call's details.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
final class Errors {
    private Errors() {}

    /** The exception for {@code status} (negative), with the details the library recorded for this thread. */
    static TickertapeException capture(int status) {
        return capture(status, -1, null);
    }

    /** As {@link #capture(int)}, with the size a {@link BufferTooSmallException} reports and a cause. */
    static TickertapeException capture(int status, long requiredBytes, Throwable cause) {
        int last = Native.lastErrorCode();
        if (last != status) {
            return create(status, "error details unavailable: the call returned " + status + " but this thread's last"
                    + " error is " + last, 0, 0, requiredBytes, cause);
        }
        String message = Native.lastErrorMessage().reinterpret(Long.MAX_VALUE).getString(0);
        int os = Native.lastErrorOsCode();
        int reason = Native.lastErrorCorruptReason();
        return create(status, message, os, reason, requiredBytes, cause);
    }

    /** The exception class for each status; pure, so a test can check every code in the header. */
    static TickertapeException create(int status, String message, int osErrorCode, int corruptReason, long requiredBytes,
            Throwable cause) {
        Status s = Status.of(status);
        return switch (s) {
            case INVALID_ARGUMENT -> new InvalidArgumentException(message, cause);
            case IO -> new JournalIOException(message, osErrorCode, cause);
            case STORAGE_FULL -> new StorageFullException(message, osErrorCode, cause);
            case LOCKED -> new LockedException(message, cause);
            case CORRUPT -> new CorruptJournalException(message, corruptReason, cause);
            case UNSUPPORTED_VERSION -> new UnsupportedVersionException(message, cause);
            case NOT_FOUND -> new JournalNotFoundException(message, cause);
            case SEQ_UNAVAILABLE -> new SeqUnavailableException(message, cause);
            case RECORD_TOO_LARGE -> new RecordTooLargeException(message, cause);
            case DURABILITY_LOST -> new DurabilityLostException(message, cause);
            case SEQUENCE_EXHAUSTED -> new SequenceExhaustedException(message, cause);
            case DISCARDED -> new DiscardedException(message, cause);
            case INVALID_CONFIG -> new InvalidConfigException(message, cause);
            case BUFFER_TOO_SMALL -> new BufferTooSmallException(message, requiredBytes, cause);
            case ABORTED -> new FillAbortedException(message, cause);
            case BUSY -> new HandleBusyException(message, cause);
            case PANIC -> new InternalErrorException(message, cause);
            case BEACON_IN_USE -> new BeaconInUseException(message, cause);
            case UNKNOWN -> new TickertapeException(Status.UNKNOWN, status, message, cause);
            case OK, NONE -> throw new IllegalArgumentException("not an error: " + status);
        };
    }

    /** Throws the captured exception for a negative status from a call that has no output to undo. */
    static void check(int status) {
        if (status < 0) {
            throw capture(status);
        }
    }
}
