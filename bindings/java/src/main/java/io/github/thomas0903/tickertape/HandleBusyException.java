package io.github.thomas0903.tickertape;

/** The library found a handle already inside a call on this thread. The binding's own guard reports such calls as {@link IllegalStateException} before they reach the library, so this means a bug in the binding. Status {@link Status#BUSY}. */
public final class HandleBusyException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    HandleBusyException(String message, Throwable cause) {
        super(Status.BUSY, Status.BUSY.code(), message, cause);
    }
}
