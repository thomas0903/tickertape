package io.github.thomas0903.tickertape;

/** A value the library rejected as an argument: out of range, or an inconsistent combination (for example retention settings without an action). Arguments that Java can check itself raise {@link IllegalArgumentException} or {@link NullPointerException} instead. Status {@link Status#INVALID_ARGUMENT}. */
public final class InvalidArgumentException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    InvalidArgumentException(String message, Throwable cause) {
        super(Status.INVALID_ARGUMENT, Status.INVALID_ARGUMENT.code(), message, cause);
    }
}
