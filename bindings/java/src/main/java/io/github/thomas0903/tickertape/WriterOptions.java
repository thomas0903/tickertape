package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.util.Objects;

/**
 * Options for {@link Writer#open(java.nio.file.Path, WriterOptions)}. Immutable; each method returns a new value.
 *
 * <pre>{@code
 * WriterOptions options = WriterOptions.defaults()
 *         .segmentSize(64L << 20)
 *         .retention(Retention.delete().maxSegments(16));
 * }</pre>
 */
public final class WriterOptions {
    private static final WriterOptions DEFAULTS = new WriterOptions(128L << 20, RecoveryPolicy.QUARANTINE, true, true, null);

    private final long segmentSize;
    private final RecoveryPolicy recoveryPolicy;
    private final boolean notify;
    private final boolean prefault;
    private final Retention retention;

    private WriterOptions(long segmentSize, RecoveryPolicy recoveryPolicy, boolean notify, boolean prefault,
            Retention retention) {
        this.segmentSize = segmentSize;
        this.recoveryPolicy = recoveryPolicy;
        this.notify = notify;
        this.prefault = prefault;
        this.retention = retention;
    }

    /** 128 MiB segments, {@link RecoveryPolicy#QUARANTINE}, notify on, prefault on, no retention. */
    public static WriterOptions defaults() {
        return DEFAULTS;
    }

    /**
     * The size of new segment files in bytes: a multiple of 65,536 from 65,536 to 2^40 (checked by
     * {@link Writer#open}, which throws {@link InvalidConfigException}). Existing segments keep their size.
     */
    public WriterOptions segmentSize(long bytes) {
        if (bytes <= 0) {
            throw new IllegalArgumentException("segment size must be positive: " + bytes);
        }
        return new WriterOptions(bytes, recoveryPolicy, notify, prefault, retention);
    }

    /** What to do with a corrupt committed tail on open. */
    public WriterOptions recoveryPolicy(RecoveryPolicy policy) {
        return new WriterOptions(segmentSize, Objects.requireNonNull(policy, "policy"), notify, prefault, retention);
    }

    /** Whether the writer wakes blocked readers through its shared-memory wake-up region (ADRs 0017, 0021). */
    public WriterOptions notify(boolean enabled) {
        return new WriterOptions(segmentSize, recoveryPolicy, enabled, prefault, retention);
    }

    /**
     * Whether the writer prefaults pages (and on macOS prepares the next segment) in a background thread it starts.
     * {@code false} gives a writer that starts no threads.
     */
    public WriterOptions prefault(boolean enabled) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, enabled, retention);
    }

    /**
     * Applies {@code retention} after open and after every successful checkpoint ({@code null} turns it off). Failures
     * never fail open, appends or checkpoints; see {@link Writer#lastRetentionError()}. A move destination must
     * already exist.
     */
    public WriterOptions retention(Retention retention) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, prefault, retention);
    }

    public long segmentSize() {
        return segmentSize;
    }

    public RecoveryPolicy recoveryPolicy() {
        return recoveryPolicy;
    }

    public boolean notifyEnabled() {
        return notify;
    }

    public boolean prefaultEnabled() {
        return prefault;
    }

    public Retention retention() {
        return retention;
    }

    /** Native writer options; free with {@code tickertape_writer_options_free}. */
    MemorySegment toNative(Arena arena) {
        MemorySegment o = Native.writerOptionsNew();
        try {
            Errors.check(Native.writerOptionsSetSegmentSize(o, segmentSize));
            Errors.check(Native.writerOptionsSetRecoveryPolicy(o, recoveryPolicy.code));
            Errors.check(Native.writerOptionsSetNotify(o, notify ? 1 : 0));
            Errors.check(Native.writerOptionsSetPrefault(o, prefault ? 1 : 0));
            if (retention != null) {
                MemorySegment r = retention.toNative(arena);
                try {
                    Errors.check(Native.writerOptionsSetRetention(o, r));
                } finally {
                    Native.retentionFree(r);
                }
            }
            return o;
        } catch (RuntimeException e) {
            Native.writerOptionsFree(o);
            throw e;
        }
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof WriterOptions w && segmentSize == w.segmentSize && recoveryPolicy == w.recoveryPolicy
                && notify == w.notify && prefault == w.prefault && Objects.equals(retention, w.retention);
    }

    @Override
    public int hashCode() {
        return Objects.hash(segmentSize, recoveryPolicy, notify, prefault, retention);
    }

    @Override
    public String toString() {
        return "WriterOptions[segmentSize=" + segmentSize + ", recoveryPolicy=" + recoveryPolicy + ", notify=" + notify
                + ", prefault=" + prefault + ", retention=" + retention + "]";
    }
}
