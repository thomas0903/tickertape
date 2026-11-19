package io.github.thomas0903.tickertape;

/** The requested sequence number is older than the oldest segment (retention removed it). Seek to the oldest segment's {@link SegmentInfo#baseSeq()} from {@link Journal#listSegments} to continue there. Status {@link Status#SEQ_UNAVAILABLE}. */
public final class SequenceUnavailableException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    SequenceUnavailableException(String message, Throwable cause) {
        super(Status.SEQ_UNAVAILABLE, Status.SEQ_UNAVAILABLE.code(), message, cause);
    }
}
