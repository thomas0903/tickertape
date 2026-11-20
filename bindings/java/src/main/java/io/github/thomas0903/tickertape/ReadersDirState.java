package io.github.thomas0903.tickertape;

/**
 * Whether the beacon directory could be listed. Each value's {@link #cName()} is the C enumerator ({@code
 * TICKERTAPE_READERS_DIR_*}, ADR 0032). {@link #UNKNOWN} stands for a value from a newer library that this binding
 * does not name; it is not in the C header.
 */
public enum ReadersDirState {
    /** Listed (possibly empty). */
    LISTED(0),
    /** It does not exist: no reader has created a beacon there. */
    MISSING(1),
    /** It exists but could not be listed; no beacons are reported. */
    UNUSABLE(2),
    /** A value from a newer library that this binding does not name. */
    UNKNOWN(-1);

    private final int code;

    ReadersDirState(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_READERS_DIR_LISTED}. */
    public String cName() {
        return "TICKERTAPE_READERS_DIR_" + name();
    }

    /** The value for a C code; {@link #UNKNOWN} for a code this binding does not name. */
    public static ReadersDirState of(int code) {
        for (ReadersDirState v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return UNKNOWN;
    }
}
