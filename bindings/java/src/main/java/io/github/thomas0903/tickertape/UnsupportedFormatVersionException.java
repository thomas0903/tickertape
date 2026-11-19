package io.github.thomas0903.tickertape;

/** A segment was written by an incompatible format version. Status {@link Status#UNSUPPORTED_VERSION}. */
public final class UnsupportedFormatVersionException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    UnsupportedFormatVersionException(String message, Throwable cause) {
        super(Status.UNSUPPORTED_VERSION, Status.UNSUPPORTED_VERSION.code(), message, cause);
    }
}
