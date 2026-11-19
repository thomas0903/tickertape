package io.github.thomas0903.tickertape;

/** A configuration value is out of range (for example a segment size that is not a multiple of 64 KiB). Status {@link Status#INVALID_CONFIG}. */
public final class InvalidConfigException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    InvalidConfigException(String message, Throwable cause) {
        super(Status.INVALID_CONFIG, Status.INVALID_CONFIG.code(), message, cause);
    }
}
