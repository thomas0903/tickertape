package io.github.thomas0903.tickertape;

/**
 * A {@link PayloadFiller} threw, so {@link Writer#appendWith} aborted the append: nothing was appended, and the
 * writer zeroed the reserved bytes and synced that to storage. {@link #getCause()} is what the filler threw. (If
 * the clean-up itself failed, {@code appendWith} throws a {@link JournalIoException} instead, with the filler's
 * exception as its cause.) Status {@link Status#ABORTED}.
 */
public final class FillAbortedException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    FillAbortedException(String message, Throwable cause) {
        super(Status.ABORTED, Status.ABORTED.code(), message, cause);
    }
}
