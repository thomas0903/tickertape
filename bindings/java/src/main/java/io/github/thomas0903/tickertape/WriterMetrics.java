package io.github.thomas0903.tickertape;

import java.time.Duration;
import java.util.Optional;

/**
 * What only the writer knows, from {@link Writer#metrics()} (ADR 0032). Unsigned values are returned with the same
 * bits in a {@code long}.
 *
 * <p>A final class, not a record: the C struct it comes from ({@code tickertape_writer_metrics}) is designed to grow
 * (ADR 0037 &rarr; Value types). {@link #equals}, {@link #hashCode} and {@link #toString} use every value.
 */
public final class WriterMetrics {
    private final Position head;
    private final long segmentSize;
    private final Optional<Checkpoint> checkpoint;
    private final Optional<Duration> sinceCheckpoint;
    private final long uncheckpointedRecords;
    private final long uncheckpointedBytes;
    private final boolean poisoned;
    private final long retentionFailures;

    WriterMetrics(MetricsValues.Struct s) {
        head = new Position(s.u64("head_next_seq"), s.u64("head_segment_base_seq"), s.u64("head_offset"));
        segmentSize = s.u64("segment_size");
        checkpoint = s.flag("has_checkpoint")
                ? Optional.of(new Checkpoint(s.u64("checkpoint_generation"), s.u64("checkpoint_next_seq"),
                        s.u64("checkpoint_segment_base_seq"), s.u64("checkpoint_offset")))
                : Optional.empty();
        sinceCheckpoint = s.duration("since_checkpoint_ns");
        uncheckpointedRecords = s.u64("uncheckpointed_records");
        uncheckpointedBytes = s.u64("uncheckpointed_bytes");
        poisoned = s.flag("poisoned");
        retentionFailures = s.u64("retention_failures");
    }

    /** Where the next record will be written. */
    public Position head() {
        return head;
    }

    /** The size of the active segment. */
    public long segmentSize() {
        return segmentSize;
    }

    /** The checkpoint marker as this writer last wrote it or read it at open, if there is one. */
    public Optional<Checkpoint> checkpoint() {
        return checkpoint;
    }

    /** The time since this writer's last successful checkpoint (monotonic clock); empty before its first. */
    public Optional<Duration> sinceCheckpoint() {
        return sinceCheckpoint;
    }

    /** Records appended but not covered by the checkpoint marker. */
    public long uncheckpointedRecords() {
        return uncheckpointedRecords;
    }

    /** Bytes of records and seals appended after the marker: what a power loss could remove. */
    public long uncheckpointedBytes() {
        return uncheckpointedBytes;
    }

    /** True after a failed checkpoint: later checkpoints throw {@link DurabilityLostException}. */
    public boolean isPoisoned() {
        return poisoned;
    }

    /** Retention runs that failed since the writer opened. */
    public long retentionFailures() {
        return retentionFailures;
    }

    private Object[] values() {
        return new Object[] {head, segmentSize, checkpoint, sinceCheckpoint, uncheckpointedRecords,
            uncheckpointedBytes, poisoned, retentionFailures};
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof WriterMetrics m && java.util.Arrays.equals(values(), m.values());
    }

    @Override
    public int hashCode() {
        return java.util.Arrays.hashCode(values());
    }

    @Override
    public String toString() {
        return "WriterMetrics[head=" + head + ", segmentSize=" + segmentSize + ", checkpoint=" + checkpoint
                + ", sinceCheckpoint=" + sinceCheckpoint + ", uncheckpointedRecords=" + uncheckpointedRecords
                + ", uncheckpointedBytes=" + uncheckpointedBytes + ", poisoned=" + poisoned + ", retentionFailures="
                + retentionFailures + "]";
    }

}
