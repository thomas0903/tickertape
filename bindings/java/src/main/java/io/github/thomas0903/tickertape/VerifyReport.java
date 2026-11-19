package io.github.thomas0903.tickertape;

import java.util.Objects;

/**
 * The result of {@link Journal#verify}: a summary of every segment, record and seal, and the chain between them.
 * Unsigned values are returned with the same bits in a {@code long}.
 *
 * <p>A final class, not a record: the C struct it comes from ({@code tickertape_verify_report}) is designed to grow,
 * and a later version may add accessors here without breaking source or binary compatibility (ADR 0037 &rarr; Value
 * types). Instances are created only by the binding; {@link #equals}, {@link #hashCode} and {@link #toString} compare
 * and show every value.
 */
public final class VerifyReport {
    private final long segments;
    private final long records;
    private final long firstSeq;
    private final long lastSeq;
    private final long problems;
    private final long firstProblemBaseSeq;
    private final long firstProblemOffset;
    private final CorruptReason firstProblemReason;

    VerifyReport(long segments, long records, long firstSeq, long lastSeq, long problems, long firstProblemBaseSeq,
            long firstProblemOffset, CorruptReason firstProblemReason) {
        this.segments = segments;
        this.records = records;
        this.firstSeq = firstSeq;
        this.lastSeq = lastSeq;
        this.problems = problems;
        this.firstProblemBaseSeq = firstProblemBaseSeq;
        this.firstProblemOffset = firstProblemOffset;
        this.firstProblemReason = Objects.requireNonNull(firstProblemReason);
    }

    /** The number of segments checked. */
    public long segments() {
        return segments;
    }

    /** The number of valid records found. */
    public long records() {
        return records;
    }

    /** The sequence number of the first valid record (meaningful only if {@link #records()} is above 0). */
    public long firstSeq() {
        return firstSeq;
    }

    /** The sequence number of the last valid record (meaningful only if {@link #records()} is above 0). */
    public long lastSeq() {
        return lastSeq;
    }

    /** The number of problems found; 0 means the journal is intact. */
    public long problems() {
        return problems;
    }

    /**
     * The base sequence number of the segment with the first problem, from its file name, or -1 ({@code UINT64_MAX})
     * if the name does not parse (meaningful only if {@link #problems()} is above 0).
     */
    public long firstProblemBaseSeq() {
        return firstProblemBaseSeq;
    }

    /** The byte offset of the first problem in that segment (meaningful only if {@link #problems()} is above 0). */
    public long firstProblemOffset() {
        return firstProblemOffset;
    }

    /** Why the first problem failed validation ({@link CorruptReason#NONE} if there is none). */
    public CorruptReason firstProblemReason() {
        return firstProblemReason;
    }

    /** Whether no problem was found. */
    public boolean isIntact() {
        return problems == 0;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof VerifyReport r && segments == r.segments && records == r.records && firstSeq == r.firstSeq
                && lastSeq == r.lastSeq && problems == r.problems && firstProblemBaseSeq == r.firstProblemBaseSeq
                && firstProblemOffset == r.firstProblemOffset && firstProblemReason == r.firstProblemReason;
    }

    @Override
    public int hashCode() {
        return Objects.hash(segments, records, firstSeq, lastSeq, problems, firstProblemBaseSeq, firstProblemOffset,
                firstProblemReason);
    }

    @Override
    public String toString() {
        return "VerifyReport[segments=" + Long.toUnsignedString(segments) + ", records=" + Long.toUnsignedString(records)
                + ", firstSeq=" + Long.toUnsignedString(firstSeq) + ", lastSeq=" + Long.toUnsignedString(lastSeq)
                + ", problems=" + Long.toUnsignedString(problems) + ", firstProblemBaseSeq="
                + Long.toUnsignedString(firstProblemBaseSeq) + ", firstProblemOffset="
                + Long.toUnsignedString(firstProblemOffset) + ", firstProblemReason=" + firstProblemReason + "]";
    }
}
