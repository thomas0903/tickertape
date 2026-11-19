package io.github.thomas0903.tickertape;

/**
 * The C ABI's status codes ({@code tickertape_status_code}), each with its C value. Every error status has its own
 * subclass of {@link TickertapeException}; {@link TickertapeException#status()} returns the status, and
 * {@link #UNKNOWN} stands for a code this binding does not know (a newer library may add codes).
 */
public enum Status {
    /** Success. */
    OK(0),
    /** Success with nothing to return; the binding returns {@code null}, {@code 0} or an empty value instead. */
    NONE(1),
    /** {@link InvalidArgumentException}. */
    INVALID_ARGUMENT(-1),
    /** {@link JournalIoException}. */
    IO(-2),
    /** {@link StorageFullException}. */
    STORAGE_FULL(-3),
    /** {@link JournalLockedException}. */
    LOCKED(-4),
    /** {@link CorruptJournalException}. */
    CORRUPT(-5),
    /** {@link UnsupportedFormatVersionException}. */
    UNSUPPORTED_VERSION(-6),
    /** {@link JournalNotFoundException}. */
    NOT_FOUND(-7),
    /** {@link SequenceUnavailableException}. */
    SEQ_UNAVAILABLE(-8),
    /** {@link RecordTooLargeException}. */
    RECORD_TOO_LARGE(-9),
    /** {@link DurabilityLostException}. */
    DURABILITY_LOST(-10),
    /** {@link SequenceExhaustedException}. */
    SEQUENCE_EXHAUSTED(-11),
    /** {@link DiscardedHistoryException}. */
    DISCARDED(-12),
    /** {@link InvalidConfigException}. */
    INVALID_CONFIG(-13),
    /** {@link BufferTooSmallException}. */
    BUFFER_TOO_SMALL(-14),
    /** {@link FillAbortedException}. */
    ABORTED(-15),
    /** {@link HandleBusyException}. */
    BUSY(-16),
    /** {@link InternalErrorException}. */
    PANIC(-17),
    /** An error code this binding has no specific mapping for ({@code TICKERTAPE_E_UNKNOWN}, or a newer code). */
    UNKNOWN(-255);

    private final int code;

    Status(int code) {
        this.code = code;
    }

    /** The C value of this status. */
    public int code() {
        return code;
    }

    /** The C name, such as {@code TICKERTAPE_E_IO} (as {@code tickertape_status_name} returns it). */
    public String cName() {
        return switch (this) {
            case OK, NONE -> "TICKERTAPE_" + name();
            default -> "TICKERTAPE_E_" + name();
        };
    }

    /** The status with C value {@code code}, or {@link #UNKNOWN} for an unknown negative code. */
    public static Status of(int code) {
        for (Status s : values()) {
            if (s.code == code) {
                return s;
            }
        }
        if (code >= 0) {
            throw new IllegalArgumentException("not an error code: " + code);
        }
        return UNKNOWN;
    }
}
