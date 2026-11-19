package io.github.thomas0903.tickertape;

import java.util.Objects;

/**
 * What {@link Journal#applyRetention} removed (or, with {@code dryRun}, would remove).
 *
 * <p>A final class, not a record: the C struct it comes from ({@code tickertape_retention_report}) is designed to
 * grow, and a later version may add accessors here without breaking source or binary compatibility (ADR 0037 &rarr;
 * Value types). Instances are created only by the binding; {@link #equals}, {@link #hashCode} and {@link #toString}
 * compare and show every value.
 */
public final class RetentionReport {
    private final long removedSegments;
    private final long removedBytes;
    private final long alreadyRemovedSegments;

    RetentionReport(long removedSegments, long removedBytes, long alreadyRemovedSegments) {
        this.removedSegments = removedSegments;
        this.removedBytes = removedBytes;
        this.alreadyRemovedSegments = alreadyRemovedSegments;
    }

    /** Segment files removed (planned for removal, with {@code dryRun}). */
    public long removedSegments() {
        return removedSegments;
    }

    /** The sum of their full sizes. */
    public long removedBytes() {
        return removedBytes;
    }

    /** Planned files that were already gone (removed concurrently); 0 with {@code dryRun}. */
    public long alreadyRemovedSegments() {
        return alreadyRemovedSegments;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof RetentionReport r && removedSegments == r.removedSegments && removedBytes == r.removedBytes
                && alreadyRemovedSegments == r.alreadyRemovedSegments;
    }

    @Override
    public int hashCode() {
        return Objects.hash(removedSegments, removedBytes, alreadyRemovedSegments);
    }

    @Override
    public String toString() {
        return "RetentionReport[removedSegments=" + Long.toUnsignedString(removedSegments) + ", removedBytes="
                + Long.toUnsignedString(removedBytes) + ", alreadyRemovedSegments="
                + Long.toUnsignedString(alreadyRemovedSegments) + "]";
    }
}
