package io.github.thomas0903.tickertape;

/**
 * Why journal bytes failed validation ({@code tickertape_corrupt_reason}), from {@link CorruptJournalException#reason()}
 * and {@link VerifyReport#firstProblemReason()}. See {@code docs/format.md} for the rules.
 *
 * <p><b>Constants may be added in minor releases</b> (with the C reasons they stand for), so a {@code switch} over a
 * reason needs a {@code default} branch; until a binding knows a new reason, it reports it as {@link #UNKNOWN}.
 */
public enum CorruptReason {
    /** No corruption. */
    NONE(0),
    /** The segment header's magic bytes are wrong. */
    BAD_MAGIC(1),
    /** The segment header failed its checksum or one of its fields is out of range. */
    BAD_SEGMENT_HEADER(2),
    /** The segment file is shorter or longer than its header says. */
    SIZE_MISMATCH(3),
    /** The file name and the header's base sequence number disagree. */
    BASE_SEQ_MISMATCH(4),
    /** A slot is misaligned, before the record area, or outside the segment. */
    BAD_OFFSET(5),
    /** A record has no representable next sequence number. */
    SEQUENCE_EXHAUSTED(6),
    /** A record's length is out of range. */
    BAD_LENGTH(7),
    /** A record's CRC32C does not match. */
    BAD_CHECKSUM(8),
    /** A record has an unexpected sequence number. */
    BAD_SEQUENCE(9),
    /** A reserved field is not zero. */
    BAD_FLAGS(10),
    /**
     * A seal is invalid (its checksum does not match the expected next sequence number, or it lies where a seal
     * cannot), or writer recovery found non-zero bytes beyond the place of a missing seal in front of a successor
     * segment that opens.
     */
    BAD_SEAL(11),
    /** A sealed segment's successor is missing while later segments exist, or does not start where the seal says. */
    BROKEN_CHAIN(12),
    /** The checkpoint marker points to a missing segment or an impossible offset. */
    DANGLING_CHECKPOINT(13),
    /**
     * A commit word reads 0, and recovery or verify found evidence of committed records after it (usually storage
     * damage; {@code docs/crash-contract.md} C1 and C11).
     */
    EMPTY_COMMIT_WORD(14),
    /**
     * A commit word reads 0, and the search of the bytes after it stopped at its work bound: inconclusive, and
     * treated like {@link #EMPTY_COMMIT_WORD}.
     */
    UNCHECKED_TAIL(15),
    /** A reason this binding has no specific constant for ({@code TICKERTAPE_CORRUPT_UNKNOWN}, or a newer one). */
    UNKNOWN(255);

    private final int code;

    CorruptReason(int code) {
        this.code = code;
    }

    /** The C value of this reason. */
    public int code() {
        return code;
    }

    /** The C name, such as {@code TICKERTAPE_CORRUPT_BAD_CHECKSUM}. */
    public String cName() {
        return "TICKERTAPE_CORRUPT_" + name();
    }

    /** The reason with C value {@code code}, or {@link #UNKNOWN}. */
    public static CorruptReason of(int code) {
        for (CorruptReason r : values()) {
            if (r.code == code) {
                return r;
            }
        }
        return UNKNOWN;
    }
}
