package io.github.thomas0903.tickertape;

/**
 * A position in the journal: the sequence number expected there, and where that is in the segment files (ADR 0032).
 * Unsigned values are returned with the same bits in a {@code long}.
 *
 * @param nextSeq the sequence number of the record at this position (or that the next record will get)
 * @param segmentBaseSeq the base sequence number of the segment the position is in
 * @param offset the byte offset in that segment
 */
public record Position(long nextSeq, long segmentBaseSeq, long offset) {}
