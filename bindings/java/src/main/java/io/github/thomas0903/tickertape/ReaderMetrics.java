package io.github.thomas0903.tickertape;

import java.util.Arrays;
import java.util.Optional;

/**
 * A reader's cursor and its lag behind a {@link JournalStatus}, from {@link Reader#metrics(JournalStatus)} (ADR
 * 0032). Unsigned values are returned with the same bits in a {@code long}.
 *
 * <p>A final class, not a record: the C struct it comes from ({@code tickertape_reader_metrics}) is designed to grow
 * (ADR 0037 &rarr; Value types). {@link #equals}, {@link #hashCode} and {@link #toString} use every value.
 */
public final class ReaderMetrics {
    private final long nextSeq;
    private final Position slot;
    private final boolean seekPending;
    private final boolean durableOnly;
    private final boolean heldRecord;
    private final Optional<Lag> lag;
    private final LagUnknown lagUnknown;

    ReaderMetrics(MetricsValues.Struct s) {
        nextSeq = s.u64("next_seq");
        slot = new Position(s.u64("slot_seq"), s.u64("segment_base_seq"), s.u64("offset"));
        seekPending = s.flag("seek_pending");
        durableOnly = s.flag("durable_only");
        heldRecord = s.flag("held_record");
        lag = s.u64("lag_records") == MetricsValues.UNKNOWN
                ? Optional.empty()
                : Optional.of(new Lag(s.u64("lag_records"), s.optional("lag_bytes")));
        lagUnknown = LagUnknown.of(s.i32("lag_unknown"));
    }

    /** The sequence number of the next record the reader returns (as {@link Reader#position()}). */
    public long nextSeq() {
        return nextSeq;
    }

    /**
     * The slot the reader reads next. Its sequence number equals {@link #nextSeq()} unless a seek is pending or a
     * record is held ({@link #isRecordHeld()}), when it is one more.
     */
    public Position slot() {
        return slot;
    }

    /** True while the target of a seek is still ahead of the slot. */
    public boolean isSeekPending() {
        return seekPending;
    }

    /** True for a durable-only reader: its lag is measured against the checkpoint marker. */
    public boolean isDurableOnly() {
        return durableOnly;
    }

    /** True if the reader holds a record that did not fit a batch buffer; it counts in the lag. */
    public boolean isRecordHeld() {
        return heldRecord;
    }

    /** The lag as of the sample, or empty with the reason in {@link #lagUnknown()}. */
    public Optional<Lag> lag() {
        return lag;
    }

    /** Why {@link #lag()} is empty ({@link LagUnknown#KNOWN} if it is not). */
    public LagUnknown lagUnknown() {
        return lagUnknown;
    }

    private Object[] values() {
        return new Object[] {nextSeq, slot, seekPending, durableOnly, heldRecord, lag, lagUnknown};
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof ReaderMetrics m && Arrays.equals(values(), m.values());
    }

    @Override
    public int hashCode() {
        return Arrays.hashCode(values());
    }

    @Override
    public String toString() {
        return "ReaderMetrics[nextSeq=" + nextSeq + ", slot=" + slot + ", seekPending=" + seekPending
                + ", durableOnly=" + durableOnly + ", heldRecord=" + heldRecord + ", lag=" + lag + ", lagUnknown="
                + lagUnknown + "]";
    }
}
