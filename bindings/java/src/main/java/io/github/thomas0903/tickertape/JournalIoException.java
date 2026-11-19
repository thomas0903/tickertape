package io.github.thomas0903.tickertape;

/**
 * An operating-system call failed. {@link #osErrorCode()} is its {@code errno} (0 if the library gave none). A
 * failed clean-up of an aborted fill, and every append the writer refuses after it, is this exception even when the
 * OS error is {@code ENOSPC}: freeing space does not make that writer accept appends again. Status
 * {@link Status#IO}; the subclass {@link StorageFullException} is {@link Status#STORAGE_FULL}.
 */
public sealed class JournalIoException extends TickertapeException permits StorageFullException {
    private static final long serialVersionUID = 1L;

    private final int osErrorCode;

    JournalIoException(String message, int osErrorCode, Throwable cause) {
        this(Status.IO, message, osErrorCode, cause);
    }

    JournalIoException(Status status, String message, int osErrorCode, Throwable cause) {
        super(status, status.code(), message, cause);
        this.osErrorCode = osErrorCode;
    }

    /** The operating system's error number ({@code errno}), or 0 if there is none. */
    public int osErrorCode() {
        return osErrorCode;
    }
}
