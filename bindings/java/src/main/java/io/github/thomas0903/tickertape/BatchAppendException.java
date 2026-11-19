package io.github.thomas0903.tickertape;

/**
 * {@link Writer#appendBatch} failed after appending {@link #appended()} records of the batch. Those records (sequence
 * numbers {@link #firstSeq()} onwards) are appended and stay appended (visible, not durable); the record at index
 * {@code appended()} failed and the rest were not tried. Retry from that index, not from the start.
 * {@link #getCause()} is the mapped error of the failing record, and {@link #status()} is its status.
 */
public final class BatchAppendException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    private final long appended;
    private final long firstSeq;

    BatchAppendException(TickertapeException cause, long appended, long firstSeq) {
        super(cause.status(), cause.code(), "batch append failed at record " + appended + ": " + cause.getMessage(), cause);
        this.appended = appended;
        this.firstSeq = firstSeq;
    }

    /** The number of records appended before the failure; also the index of the record that failed. */
    public long appended() {
        return appended;
    }

    /** The sequence number of the batch's first record (meaningful when {@link #appended()} is above 0). */
    public long firstSeq() {
        return firstSeq;
    }
}
