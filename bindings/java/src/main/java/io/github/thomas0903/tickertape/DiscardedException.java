package io.github.thomas0903.tickertape;

/** Writer recovery or retention replaced or removed the segment at the reader's position. Records from here on may belong to a discarded history: call {@link Reader#seek} (for example to {@link Reader#position()}) to continue on the current chain. Status {@link Status#DISCARDED}. */
public final class DiscardedException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    DiscardedException(String message, Throwable cause) {
        super(Status.DISCARDED, Status.DISCARDED.code(), message, cause);
    }
}
