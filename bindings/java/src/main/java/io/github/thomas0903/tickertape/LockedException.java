package io.github.thomas0903.tickertape;

/** Another writer, in this process or another, has the journal open; only one writer may hold a journal at a time. Status {@link Status#LOCKED}. */
public final class LockedException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    LockedException(String message, Throwable cause) {
        super(Status.LOCKED, Status.LOCKED.code(), message, cause);
    }
}
