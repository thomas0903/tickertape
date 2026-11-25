package io.github.thomas0903.tickertape;

import java.lang.foreign.MemorySegment;
import java.time.Duration;
import java.util.Objects;

/**
 * Options for {@link Reader#open(java.nio.file.Path, ReaderOptions)}. Immutable; each method returns a new value.
 *
 * <pre>{@code
 * ReaderOptions options = ReaderOptions.defaults().durableOnly(true);
 * }</pre>
 */
public final class ReaderOptions {
    static final long DEFAULT_WAIT_SLICE_NS = 10_000_000L;
    private static final ReaderOptions DEFAULTS =
            new ReaderOptions(false, false, true, false, 100, 10, 10_000, 1_000_000, DEFAULT_WAIT_SLICE_NS);

    private final boolean durableOnly;
    private final boolean seekIndex;
    private final boolean notify;
    private final boolean busySpin;
    private final int spins;
    private final int yields;
    private final long minSleepNs;
    private final long maxSleepNs;
    private final long waitSliceNs;

    private ReaderOptions(boolean durableOnly, boolean seekIndex, boolean notify, boolean busySpin, int spins,
            int yields, long minSleepNs, long maxSleepNs, long waitSliceNs) {
        this.durableOnly = durableOnly;
        this.seekIndex = seekIndex;
        this.notify = notify;
        this.busySpin = busySpin;
        this.spins = spins;
        this.yields = yields;
        this.minSleepNs = minSleepNs;
        this.maxSleepNs = maxSleepNs;
        this.waitSliceNs = waitSliceNs;
    }

    /**
     * Not durable-only, notify on, the library's default back-off (100 spins, 10 yields, then sleeps from 10 &micro;s
     * doubling to 1 ms), and a 10 ms wait slice.
     */
    public static ReaderOptions defaults() {
        return DEFAULTS;
    }

    /**
     * Whether the reader returns only <b>durable</b> records: those covered by a checkpoint (ADR 0010, crash contract
     * C13). By default it returns every <b>visible</b> record, including records a power loss can still erase. A
     * durable-only reader treats records after the checkpoint as not written yet, and returns nothing without a
     * valid checkpoint marker.
     */
    public ReaderOptions durableOnly(boolean enabled) {
        return new ReaderOptions(enabled, seekIndex, notify, busySpin, spins, yields, minSleepNs, maxSleepNs,
                waitSliceNs);
    }

    /**
     * Whether {@link Reader#seek(long)} may start at a record named by a seek index, one of the optional files that
     * {@code tickertape index} writes next to the journal (ADR 0045, crash contract C15). Off by default. Records before
     * that one are then not read, so damage there goes unnoticed; without an index the seek is unchanged, and it never
     * fails because of one. Do not enable this where someone who may not write the segments can write
     * {@code seek-index/}.
     *
     * @since 1.1
     */
    public ReaderOptions seekIndex(boolean enabled) {
        return new ReaderOptions(durableOnly, enabled, notify, busySpin, spins, yields, minSleepNs, maxSleepNs,
                waitSliceNs);
    }

    /** Whether a waiting reader blocks on the writer's wake-up region, so a new record wakes it at once. */
    public ReaderOptions notify(boolean enabled) {
        return new ReaderOptions(durableOnly, seekIndex, enabled, busySpin, spins, yields, minSleepNs, maxSleepNs,
                waitSliceNs);
    }

    /**
     * How a waiting reader polls: {@code spins} busy polls, then {@code yields} polls that yield the CPU, then sleeps
     * from {@code minSleep} doubling to {@code maxSleep}.
     */
    public ReaderOptions backoff(int spins, int yields, Duration minSleep, Duration maxSleep) {
        if (spins < 0 || yields < 0) {
            throw new IllegalArgumentException("spins and yields must not be negative");
        }
        long min = nanos(minSleep, "minSleep");
        long max = nanos(maxSleep, "maxSleep");
        if (min > max) {
            throw new IllegalArgumentException("minSleep must not exceed maxSleep");
        }
        return new ReaderOptions(durableOnly, seekIndex, notify, false, spins, yields, min, max, waitSliceNs);
    }

    /** Makes a waiting reader spin without ever yielding or sleeping: lowest latency, one core kept busy. */
    public ReaderOptions busySpin() {
        return new ReaderOptions(durableOnly, seekIndex, notify, true, spins, yields, minSleepNs, maxSleepNs,
                waitSliceNs);
    }

    /**
     * The longest single wait in the library during a blocking read (default 10 ms). Between slices the binding checks
     * for {@link Thread#interrupt()} and for {@link Reader#close()} from another thread, so this bounds how long a
     * blocked read takes to notice either. It does not change when a record is returned: a record that arrives
     * during a slice is returned at once.
     */
    public ReaderOptions waitSlice(Duration slice) {
        long ns = nanos(slice, "slice");
        if (ns == 0) {
            throw new IllegalArgumentException("the wait slice must be positive");
        }
        return new ReaderOptions(durableOnly, seekIndex, notify, busySpin, spins, yields, minSleepNs, maxSleepNs, ns);
    }

    /** Whether the reader returns only durable records (see {@link #durableOnly(boolean)}). */
    public boolean isDurableOnly() {
        return durableOnly;
    }

    /**
     * Whether seeks may use a seek index (see {@link #seekIndex(boolean)}).
     *
     * @since 1.1
     */
    public boolean isSeekIndexEnabled() {
        return seekIndex;
    }

    /** Whether a waiting reader blocks on the writer's wake-up region (see {@link #notify(boolean)}). */
    public boolean isNotifyEnabled() {
        return notify;
    }

    /** The longest single wait in the library during a blocking read (see {@link #waitSlice(Duration)}). */
    public Duration waitSlice() {
        return Duration.ofNanos(waitSliceNs);
    }

    long waitSliceNanos() {
        return waitSliceNs;
    }

    private static long nanos(Duration d, String name) {
        Objects.requireNonNull(d, name);
        if (d.isNegative()) {
            throw new IllegalArgumentException(name + " must not be negative: " + d);
        }
        try {
            return d.toNanos();
        } catch (ArithmeticException e) {
            return Long.MAX_VALUE;
        }
    }

    /** Native reader options; free with {@code tickertape_reader_options_free}. */
    MemorySegment toNative() {
        MemorySegment o = Native.readerOptionsNew();
        try {
            Errors.check(Native.readerOptionsSetDurableOnly(o, durableOnly ? 1 : 0));
            Errors.check(Native.readerOptionsSetSeekIndex(o, seekIndex ? 1 : 0));
            Errors.check(Native.readerOptionsSetNotify(o, notify ? 1 : 0));
            if (busySpin) {
                Errors.check(Native.readerOptionsSetBusySpin(o));
            } else {
                Errors.check(Native.readerOptionsSetBackoff(o, spins, yields, minSleepNs, maxSleepNs));
            }
            return o;
        } catch (Throwable e) { // an Error too: the native struct must not leak
            Native.readerOptionsFree(o);
            throw e;
        }
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof ReaderOptions r && durableOnly == r.durableOnly && seekIndex == r.seekIndex && notify == r.notify
                && busySpin == r.busySpin && spins == r.spins && yields == r.yields && minSleepNs == r.minSleepNs
                && maxSleepNs == r.maxSleepNs && waitSliceNs == r.waitSliceNs;
    }

    @Override
    public int hashCode() {
        return Objects.hash(durableOnly, seekIndex, notify, busySpin, spins, yields, minSleepNs, maxSleepNs, waitSliceNs);
    }

    @Override
    public String toString() {
        return "ReaderOptions[durableOnly=" + durableOnly + ", seekIndex=" + seekIndex + ", notify=" + notify
                + (busySpin ? ", busySpin" : ", backoff=" + spins + "/" + yields + "/" + minSleepNs + "ns/" + maxSleepNs + "ns")
                + ", waitSlice=" + Duration.ofNanos(waitSliceNs) + "]";
    }
}
