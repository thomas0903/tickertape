package io.github.thomas0903.tickertape;

/**
 * Why a reader's lag is unknown. Each value's {@link #cName()} is the C enumerator ({@code TICKERTAPE_LAG_*}, ADR
 * 0032).
 */
public enum LagUnknown {
    /** The lag is known. */
    KNOWN(0),
    /** Retention or recovery removed the reader's segment. */
    SEGMENT_GONE(1),
    /** The segment of that name is another file (another journal, a replaced journal, a repaired segment). */
    OTHER_HISTORY(2),
    /** The reader's segment exists but its header could not be read. */
    SEGMENT_UNREADABLE(3),
    /** A durable-only reader, and the checkpoint marker cannot be used. */
    CHECKPOINT_UNUSABLE(4),
    /** The reader is in a segment newer than the sample listed: take a new sample. */
    AHEAD_OF_SAMPLE(5),
    /** A reason the C ABI does not name. */
    OTHER_REASON(255);

    private final int code;

    LagUnknown(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_LAG_KNOWN}. */
    public String cName() {
        return "TICKERTAPE_LAG_" + name();
    }

    /** The value for a C code; {@link #OTHER_REASON} for a code this binding does not name. */
    public static LagUnknown of(int code) {
        for (LagUnknown v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return OTHER_REASON;
    }
}
