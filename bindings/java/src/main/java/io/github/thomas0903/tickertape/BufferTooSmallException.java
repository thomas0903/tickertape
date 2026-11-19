package io.github.thomas0903.tickertape;

/**
 * A caller-provided buffer is too small. From {@link Reader#readBatch}: the next record's payload does not fit in
 * the {@link ReadBatch}'s buffer; {@link #requiredBytes()} is the size it needs, and the record stays next (nothing
 * was consumed). Status {@link Status#BUFFER_TOO_SMALL}.
 */
public final class BufferTooSmallException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    private final long requiredBytes;

    BufferTooSmallException(String message, long requiredBytes, Throwable cause) {
        super(Status.BUFFER_TOO_SMALL, Status.BUFFER_TOO_SMALL.code(), message, cause);
        this.requiredBytes = requiredBytes;
    }

    /** The number of bytes needed, or -1 if the library did not say. */
    public long requiredBytes() {
        return requiredBytes;
    }
}
