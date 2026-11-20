package io.github.thomas0903.tickertape;

import java.util.Objects;
import java.util.Optional;
import java.util.OptionalLong;

/**
 * Where a writer's checkpoints stand, from {@link Writer#checkpointProgress()}. A final class rather than a record,
 * because its C struct ({@code tickertape_checkpoint_progress}) can gain fields in later versions.
 */
public final class CheckpointProgress {
    private final Optional<Checkpoint> durable;
    private final OptionalLong pending;

    CheckpointProgress(Optional<Checkpoint> durable, OptionalLong pending) {
        this.durable = Objects.requireNonNull(durable, "durable");
        this.pending = Objects.requireNonNull(pending, "pending");
    }

    /**
     * The checkpoint marker this writer last made durable, or read when it opened; empty if the journal has none yet.
     * Every record with a sequence number below its {@link Checkpoint#nextSeq()} is durable.
     */
    public Optional<Checkpoint> durable() {
        return durable;
    }

    /**
     * The sequence number the newest requested background checkpoint covers up to (what
     * {@link Writer#requestCheckpoint()} returned for it), while it has not finished; empty when every requested
     * checkpoint has finished.
     */
    public OptionalLong pending() {
        return pending;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof CheckpointProgress p && durable.equals(p.durable) && pending.equals(p.pending);
    }

    @Override
    public int hashCode() {
        return Objects.hash(durable, pending);
    }

    @Override
    public String toString() {
        return "CheckpointProgress[durable=" + durable + ", pending=" + pending + "]";
    }
}
