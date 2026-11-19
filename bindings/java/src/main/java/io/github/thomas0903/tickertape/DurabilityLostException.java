package io.github.thomas0903.tickertape;

/** An earlier checkpoint failed, or the clean-up after an aborted fill failed, so this writer can no longer guarantee durability. After a failed checkpoint appends still work. See {@code docs/crash-contract.md} &rarr; Errors are sticky. Status {@link Status#DURABILITY_LOST}. */
public final class DurabilityLostException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    DurabilityLostException(String message, Throwable cause) {
        super(Status.DURABILITY_LOST, Status.DURABILITY_LOST.code(), message, cause);
    }
}
