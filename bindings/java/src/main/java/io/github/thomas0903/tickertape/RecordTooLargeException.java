package io.github.thomas0903.tickertape;

/** The payload does not fit in one segment. Status {@link Status#RECORD_TOO_LARGE}. */
public final class RecordTooLargeException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    RecordTooLargeException(String message, Throwable cause) {
        super(Status.RECORD_TOO_LARGE, Status.RECORD_TOO_LARGE.code(), message, cause);
    }
}
