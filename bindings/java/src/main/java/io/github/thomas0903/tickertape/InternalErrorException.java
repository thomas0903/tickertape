package io.github.thomas0903.tickertape;

/** An internal error (a bug in tickertape) was caught. The handle involved is broken: later calls on it throw this too; close it. Status {@link Status#PANIC}. */
public final class InternalErrorException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    InternalErrorException(String message, Throwable cause) {
        super(Status.PANIC, Status.PANIC.code(), message, cause);
    }
}
