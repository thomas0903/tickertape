package io.github.thomas0903.tickertape;

/**
 * Why the checkpoint's age could not be estimated from the marker's modification time. Each value's {@link #cName()}
 * is the C enumerator ({@code TICKERTAPE_AGE_*}, ADR 0032).
 */
public enum AgeUnknown {
    /** The age is known, or there is no marker. */
    KNOWN(0),
    /** The filesystem does not report a modification time. */
    NO_MODIFICATION_TIME(1),
    /** The modification time is later than the observer's clock. */
    IN_FUTURE(2),
    /** The modification time is earlier than the creation of the marker's segment. */
    BEFORE_SEGMENT(3),
    /** The marker file does not have its full length. */
    MARKER_RESIZED(4),
    /** The marker kept changing while it was read: a writer checkpointing very often. */
    CHANGING(5),
    /** The marker lost bytes, so its time is not known to be a checkpoint's. */
    OLDER_SLOT_LOST(6),
    /** A reason the C ABI does not name. */
    OTHER_REASON(255);

    private final int code;

    AgeUnknown(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_AGE_KNOWN}. */
    public String cName() {
        return "TICKERTAPE_AGE_" + name();
    }

    /** The value for a C code; {@link #OTHER_REASON} for a code this binding does not name. */
    public static AgeUnknown of(int code) {
        for (AgeUnknown v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return OTHER_REASON;
    }
}
