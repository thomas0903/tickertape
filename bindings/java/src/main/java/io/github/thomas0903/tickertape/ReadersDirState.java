package io.github.thomas0903.tickertape;

/**
 * Whether the beacon directory could be listed. Each value's {@link #cName()} is the C enumerator ({@code
 * TICKERTAPE_READERS_DIR_*}, ADR 0032). {@link #OTHER} is a state the C ABI does not name (a newer
 * library); this binding reports a code it does not know as {@link #OTHER} too.
 */
public enum ReadersDirState {
    /** Listed (possibly empty). */
    LISTED(0),
    /** It does not exist: no reader has created a beacon there. */
    MISSING(1),
    /** It exists but could not be listed; no beacons are reported. */
    UNUSABLE(2),
    /** A state the C ABI does not name ({@code TICKERTAPE_READERS_DIR_OTHER}). */
    OTHER(255);

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

    /** The value for a C code; {@link #OTHER} for a code this binding does not name. */
    public static ReadersDirState of(int code) {
        for (ReadersDirState v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return OTHER;
    }
}
