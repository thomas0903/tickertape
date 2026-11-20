package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.JAVA_INT;

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
    private static final WriterOptions DEFAULTS =
            new WriterOptions(128L << 20, RecoveryPolicy.QUARANTINE, true, true, true, HelperCpus.process(), null);

    private final long segmentSize;
    private final RecoveryPolicy recoveryPolicy;
    private final boolean notify;
    private final boolean prefault;
    private final boolean earlyWriteback;
    private final HelperCpus helperCpus;
    private final Retention retention;

    private WriterOptions(long segmentSize, RecoveryPolicy recoveryPolicy, boolean notify, boolean prefault,
            boolean earlyWriteback, HelperCpus helperCpus, Retention retention) {
        this.segmentSize = segmentSize;
        this.recoveryPolicy = recoveryPolicy;
        this.notify = notify;
        this.prefault = prefault;
        this.earlyWriteback = earlyWriteback;
        this.helperCpus = helperCpus;
        this.retention = retention;
    }

    /**
     * 128 MiB segments, {@link RecoveryPolicy#QUARANTINE}, notify on, prefault on, early write-back on,
     * {@link HelperCpus#process()}, no retention.
     */
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
        return new WriterOptions(bytes, recoveryPolicy, notify, prefault, earlyWriteback, helperCpus, retention);
    }

    /** What to do with a corrupt committed tail on open. */
    public WriterOptions recoveryPolicy(RecoveryPolicy policy) {
        return new WriterOptions(segmentSize, Objects.requireNonNull(policy, "policy"), notify, prefault, earlyWriteback,
                helperCpus, retention);
    }

    /** Whether the writer wakes blocked readers through its shared-memory wake-up region (ADRs 0017, 0021). */
    public WriterOptions notify(boolean enabled) {
        return new WriterOptions(segmentSize, recoveryPolicy, enabled, prefault, earlyWriteback, helperCpus, retention);
    }

    /**
     * Whether the writer prefaults pages (and on macOS prepares the next segment) in a background thread it starts.
     * {@code false} gives a writer that starts no threads, except the checkpoint thread that
     * {@link Writer#requestCheckpoint()} starts, and turns off early write-back.
     */
    public WriterOptions prefault(boolean enabled) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, enabled, earlyWriteback, helperCpus, retention);
    }

    /**
     * <b>Linux:</b> whether the writer starts the kernel's write-back of the pages behind it as it goes, so that a
     * checkpoint finds little left to write (ADR 0038 &rarr; Early write-back; needs {@link #prefault(boolean)}). It
     * makes nothing durable earlier: only a checkpoint does. If it fails with an I/O error, no later checkpoint of the
     * writer succeeds: the error is thrown once, by the next checkpoint, {@link Writer#requestCheckpoint()},
     * {@link Writer#checkpointProgress()} or {@link Writer#close()}. <b>macOS:</b> no effect. Default: on.
     */
    public WriterOptions earlyWriteback(boolean enabled) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, prefault, enabled, helperCpus, retention);
    }

    /** Where the writer's helper threads run (Linux; see {@link HelperCpus}). Default: {@link HelperCpus#process()}. */
    public WriterOptions helperCpus(HelperCpus cpus) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, prefault, earlyWriteback,
                Objects.requireNonNull(cpus, "cpus"), retention);
    }

    /**
     * Applies {@code retention} after open and after every successful checkpoint ({@code null} turns it off). Failures
     * never fail open, appends or checkpoints; see {@link Writer#lastRetentionError()}. A move destination must
     * already exist.
     */
    public WriterOptions retention(Retention retention) {
        return new WriterOptions(segmentSize, recoveryPolicy, notify, prefault, earlyWriteback, helperCpus, retention);
    }

    /** The size of new segment files in bytes. */
    public long segmentSize() {
        return segmentSize;
    }

    /** What to do with a corrupt committed tail on open. */
    public RecoveryPolicy recoveryPolicy() {
        return recoveryPolicy;
    }

    /** Whether the writer wakes blocked readers through its wake-up region (see {@link #notify(boolean)}). */
    public boolean isNotifyEnabled() {
        return notify;
    }

    /** Whether the writer prefaults pages in a background thread (see {@link #prefault(boolean)}). */
    public boolean isPrefaultEnabled() {
        return prefault;
    }

    /** Whether early write-back is on (see {@link #earlyWriteback(boolean)}). */
    public boolean isEarlyWritebackEnabled() {
        return earlyWriteback;
    }

    /** Where the writer's helper threads run. */
    public HelperCpus helperCpus() {
        return helperCpus;
    }

    /** The retention settings, or {@code null} if retention is off. */
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
            Errors.check(Native.writerOptionsSetEarlyWriteback(o, earlyWriteback ? 1 : 0));
            int[] cpus = helperCpus.cpuArray();
            MemorySegment list = cpus.length == 0 ? MemorySegment.NULL : arena.allocateFrom(JAVA_INT, cpus);
            Errors.check(Native.writerOptionsSetHelperCpus(o, helperCpus.placement, list, cpus.length));
            if (retention != null) {
                MemorySegment r = retention.toNative(arena);
                try {
                    Errors.check(Native.writerOptionsSetRetention(o, r));
                } finally {
                    Native.retentionFree(r);
                }
            }
            return o;
        } catch (Throwable e) { // an Error too: the native struct must not leak
            Native.writerOptionsFree(o);
            throw e;
        }
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof WriterOptions w && segmentSize == w.segmentSize && recoveryPolicy == w.recoveryPolicy
                && notify == w.notify && prefault == w.prefault && earlyWriteback == w.earlyWriteback
                && helperCpus.equals(w.helperCpus) && Objects.equals(retention, w.retention);
    }

    @Override
    public int hashCode() {
        return Objects.hash(segmentSize, recoveryPolicy, notify, prefault, earlyWriteback, helperCpus, retention);
    }

    @Override
    public String toString() {
        return "WriterOptions[segmentSize=" + segmentSize + ", recoveryPolicy=" + recoveryPolicy + ", notify=" + notify
                + ", prefault=" + prefault + ", earlyWriteback=" + earlyWriteback + ", helperCpus=" + helperCpus
                + ", retention=" + retention + "]";
    }
}
