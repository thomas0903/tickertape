package io.github.thomas0903.tickertape;

/**
 * The base of every error the binding maps from the C ABI. Each error {@link Status} has its own subclass (see
 * {@link Status}); a code this binding does not know, which a newer library may return, arrives as a plain
 * {@code TickertapeException} with status {@link Status#UNKNOWN} and the raw {@link #code()}.
 *
 * <p>Unchecked, like the exceptions of Chronicle Queue and Aeron, so that appends and reads work inside lambdas and
 * streams. Every method documents what it throws. Errors that are the caller's fault and that Java can detect
 * itself (null arguments, negative lengths, a closed handle, concurrent use) are thrown as the usual
 * {@link NullPointerException}, {@link IllegalArgumentException} and {@link IllegalStateException} before the library
 * is called.
 *
 * <p>A failed read never consumed a record: after any exception from {@link Reader}, the next read returns the record
 * that was next before the failing call.
 */
public class TickertapeException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final Status status;
    private final int code;

    TickertapeException(Status status, int code, String message, Throwable cause) {
        super(message, cause);
        this.status = status;
        this.code = code;
    }

    /** The status this error maps; never {@link Status#OK} or {@link Status#NONE}. */
    public Status status() {
        return status;
    }

    /** The C status code as the library returned it (useful when {@link #status()} is {@link Status#UNKNOWN}). */
    public int code() {
        return code;
    }
}
