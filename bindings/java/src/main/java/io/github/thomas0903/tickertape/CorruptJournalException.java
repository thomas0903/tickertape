package io.github.thomas0903.tickertape;

/**
 * Bytes in the journal failed validation; {@link #reason()} says why. A reader's position does not move, so calling
 * again retries the same position (which may succeed after a writer recovery). Status {@link Status#CORRUPT}.
 */
public final class CorruptJournalException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    private final CorruptReason reason;
    private final int reasonCode;

    CorruptJournalException(String message, int reasonCode, Throwable cause) {
        super(Status.CORRUPT, Status.CORRUPT.code(), message, cause);
        this.reason = CorruptReason.of(reasonCode);
        this.reasonCode = reasonCode;
    }

    /** Why the bytes failed validation ({@link CorruptReason#UNKNOWN} for a reason this binding does not know). */
    public CorruptReason reason() {
        return reason;
    }

    /** The C value of the reason, as the library returned it. */
    public int reasonCode() {
        return reasonCode;
    }
}
