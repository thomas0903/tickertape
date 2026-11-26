package io.github.thomas0903.tickertape;

/**
 * Why the checkpoint marker cannot be used; anything but {@link #USABLE} means a writer refuses the journal
 * (corruption). Each value's {@link #cName()} is the C enumerator ({@code TICKERTAPE_CHECKPOINT_*}, ADR 0032).
 */
public enum CheckpointProblem {
    /** No problem was found, or there is no marker. */
    USABLE(0),
    /** The marker's segment does not exist. */
    MISSING_SEGMENT(1),
    /** The marker's segment has an invalid header. */
    INVALID_SEGMENT_HEADER(2),
    /** The marker's offset is misaligned or outside its segment's record area. */
    INVALID_OFFSET(3),
    /**
     * The records of the marker's segment do not reach the marker's position: the record right before it is missing,
     * damaged, ends elsewhere or has another sequence number. Since 1.1.
     */
    DANGLING(4),
    /** A problem the C ABI does not name. */
    OTHER_PROBLEM(255);

    private final int code;

    CheckpointProblem(int code) {
        this.code = code;
    }

    /** The C value. */
    public int code() {
        return code;
    }

    /** The C enumerator's name, such as {@code TICKERTAPE_CHECKPOINT_USABLE}. */
    public String cName() {
        return "TICKERTAPE_CHECKPOINT_" + name();
    }

    /** The value for a C code; {@link #OTHER_PROBLEM} for a code this binding does not name. */
    public static CheckpointProblem of(int code) {
        for (CheckpointProblem v : values()) {
            if (v.code == code) {
                return v;
            }
        }
        return OTHER_PROBLEM;
    }
}
