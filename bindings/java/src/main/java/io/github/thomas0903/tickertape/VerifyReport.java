package io.github.thomas0903.tickertape;

/**
 * The result of {@link Journal#verify}: a summary of every segment, record and seal, and the chain between them.
 * Unsigned values are returned with the same bits in a {@code long}.
 *
 * @param segments the number of segments checked
 * @param records the number of valid records found
 * @param firstSeq the sequence number of the first valid record (meaningful only if {@code records} is above 0)
 * @param lastSeq the sequence number of the last valid record (meaningful only if {@code records} is above 0)
 * @param problems the number of problems found; 0 means the journal is intact
 * @param firstProblemBaseSeq the base sequence number of the segment with the first problem, from its file name, or
 *     -1 ({@code UINT64_MAX}) if the name does not parse (meaningful only if {@code problems} is above 0)
 * @param firstProblemOffset the byte offset of the first problem in that segment
 * @param firstProblemReason why the first problem failed validation ({@link CorruptReason#NONE} if there is none)
 */
public record VerifyReport(long segments, long records, long firstSeq, long lastSeq, long problems,
        long firstProblemBaseSeq, long firstProblemOffset, CorruptReason firstProblemReason) {
    /** Whether no problem was found. */
    public boolean isIntact() {
        return problems == 0;
    }
}
