package io.github.thomas0903.tickertape;

/**
 * How the newest segment ends, as a {@link JournalStatus} saw it. Each value's {@link #cName()} is the C enumerator
 * ({@code TICKERTAPE_HEAD_*}, ADR 0032). {@link #UNKNOWN} stands for a value from a newer library that this binding
 * does not name; it is not in the C header.
 */
public enum HeadEnd {
    /** At a slot with no record yet: the normal state. */
    OPEN(0),
    /** At a seal whose successor did not exist when the sample checked: a writer died during a roll. */
    SEALED(1),
    /** At bytes that failed validation even after re-reading ({@link JournalStatus#headCorruptReason}): corruption. */
    INVALID(2),
    /** A value from a newer library that this binding does not name. */
    UNKNOWN(-1);

    private final int code;

    HeadEnd(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_HEAD_OPEN}. */
    public String cName() {
        return "TICKERTAPE_HEAD_" + name();
    }

    /** The value for a C code; {@link #UNKNOWN} for a code this binding does not name. */
    public static HeadEnd of(int code) {
        for (HeadEnd v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return UNKNOWN;
    }
}
