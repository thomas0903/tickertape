package io.github.thomas0903.tickertape;

/**
 * An operating-system call failed because the filesystem is full ({@code ENOSPC}, {@code EDQUOT}). An append that
 * fails this way leaves the journal consistent and can be retried once space is available. Status
 * {@link Status#STORAGE_FULL}.
 */
public final class StorageFullException extends JournalIoException {
    private static final long serialVersionUID = 1L;

    StorageFullException(String message, int osErrorCode, Throwable cause) {
        super(Status.STORAGE_FULL, message, osErrorCode, cause);
    }
}
