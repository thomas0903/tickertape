package io.github.thomas0903.tickertape;

/**
 * Whether a reader beacon's owner is running, and whether the beacon could be read. Each value's {@link #cName()} is
 * the C enumerator ({@code TICKERTAPE_BEACON_*}, ADR 0032). {@link #UNKNOWN} stands for a value from a newer library
 * that this binding does not name; it is not in the C header.
 */
public enum BeaconState {
    /** A process holds the beacon's lock. */
    LIVE(0),
    /** Nobody holds the lock: the owner died. Its position is history, not lag. */
    STALE(1),
    /** A valid beacon whose lock state could not be established. */
    UNCERTAIN(2),
    /** Not a valid beacon (length, magic, version or checksum). */
    INVALID(3),
    /** The file could not be opened or read. */
    UNREADABLE(4),
    /** The name was replaced during every attempt to observe it: nothing is reported. */
    CHANGING(5),
    /** A value from a newer library that this binding does not name. */
    UNKNOWN(-1);

    private final int code;

    BeaconState(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_BEACON_LIVE}. */
    public String cName() {
        return "TICKERTAPE_BEACON_" + name();
    }

    /** The value for a C code; {@link #UNKNOWN} for a code this binding does not name. */
    public static BeaconState of(int code) {
        for (BeaconState v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return UNKNOWN;
    }
}
