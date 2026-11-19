package io.github.thomas0903.tickertape;

/**
 * What a writer does on open when it finds a corrupt committed record at the end of the newest segment
 * ({@code docs/format.md} &rarr; Recovery). Uncommitted partial records are always cleaned up, whatever the policy.
 */
public enum RecoveryPolicy {
    /** Copy the invalid bytes to {@code quarantine/} in the journal directory, then continue; the default. */
    QUARANTINE(0),
    /** Fail the open with {@link CorruptJournalException} instead of quarantining or repairing anything. */
    FAIL(1);

    final int code;

    RecoveryPolicy(int code) {
        this.code = code;
    }
}
