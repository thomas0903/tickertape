package io.github.thomas0903.tickertape;

/**
 * The journal's checkpoint marker, from {@link Journal#readCheckpoint} ({@code docs/format.md} &rarr; Checkpoint
 * marker).
 *
 * @param generation incremented on every checkpoint
 * @param nextSeq every record with a smaller sequence number was durable when the marker was written
 * @param segmentBaseSeq the segment that contains the position of {@code nextSeq}
 * @param offset the offset of {@code nextSeq}'s record (or of a seal) in that segment
 */
public record Checkpoint(long generation, long nextSeq, long segmentBaseSeq, long offset) {}
