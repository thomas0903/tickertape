package io.github.thomas0903.tickertape;

/**
 * One segment file, as listed by {@link Journal#listSegments}.
 *
 * @param baseSeq the sequence number of the segment's first record
 * @param segmentSize the file size in bytes
 * @param createdUnixNs the creation time from the segment header, ns since the Unix epoch, by the writer's clock
 * @param formatVersion the format version from the segment header
 */
public record SegmentInfo(long baseSeq, long segmentSize, long createdUnixNs, int formatVersion) {
    /** The file name: {@code baseSeq} as 20 zero-padded decimal digits, then {@code .tape}. */
    public String fileName() {
        String digits = Long.toUnsignedString(baseSeq);
        return "0".repeat(Math.max(0, 20 - digits.length())) + digits + ".tape";
    }
}
