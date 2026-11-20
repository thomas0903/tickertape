package io.github.thomas0903.tickertape;

/** The directory exists but contains no journal segments yet (for example, the writer is creating the first one). A directory that does not exist is a {@link JournalIOException} with {@code ENOENT} instead; a reader that waits for its writer retries on both. Status {@link Status#NOT_FOUND}. */
public final class JournalNotFoundException extends TickertapeException {
    private static final long serialVersionUID = 1L;

    JournalNotFoundException(String message, Throwable cause) {
        super(Status.NOT_FOUND, Status.NOT_FOUND.code(), message, cause);
    }
}
