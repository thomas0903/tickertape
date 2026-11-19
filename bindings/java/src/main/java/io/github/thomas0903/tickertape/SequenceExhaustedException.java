package io.github.thomas0903.tickertape;

/** Sequence numbers (or checkpoint generations) are exhausted. Status {@link Status#SEQUENCE_EXHAUSTED}. */
public final class SequenceExhaustedException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    SequenceExhaustedException(String message, Throwable cause) {
        super(Status.SEQUENCE_EXHAUSTED, Status.SEQUENCE_EXHAUSTED.code(), message, cause);
    }
}
