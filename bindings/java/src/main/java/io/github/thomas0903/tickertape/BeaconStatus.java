package io.github.thomas0903.tickertape;

import java.time.Duration;
import java.util.Arrays;
import java.util.Optional;
import java.util.OptionalLong;

/**
 * A reader in another process, from its beacon, as a {@link JournalStatus} read it (ADR 0032 &rarr; Reader positions
 * from other processes). The {@code report*} values are what the beacon said at its last write, and are empty when its
 * contents could not be read ({@link #reportSlot()} empty). Unsigned values are returned with the same bits in a
 * {@code long}.
 *
 * <p>A final class, not a record: the C struct it comes from ({@code tickertape_beacon_status}) is designed to grow
 * (ADR 0037 &rarr; Value types). {@link #equals} and {@link #hashCode} use every value.
 */
public final class BeaconStatus {
    private final String name;
    private final BeaconState state;
    private final boolean hasReport;
    private final OptionalLong reportPid;
    private final OptionalLong reportNextSeq;
    private final Optional<Position> reportSlot;
    private final boolean reportSeekPending;
    private final boolean reportDurableOnly;
    private final OptionalLong reportUpdatedUnixNs;
    private final Optional<Duration> updateAge;
    private final Optional<Lag> lag;
    private final LagUnknown lagUnknown;
    private final OptionalLong oldestUnreadLocalTsNs;
    private final Optional<Duration> unreadAge;

    BeaconStatus(String name, MetricsValues.Struct s) {
        this.name = name;
        state = BeaconState.of(s.i32("state"));
        hasReport = s.flag("has_report");
        reportPid = s.optional("report_pid");
        reportNextSeq = s.optional("report_next_seq");
        reportSlot = hasReport
                ? Optional.of(new Position(s.u64("report_slot_seq"), s.u64("report_segment_base_seq"),
                        s.u64("report_offset")))
                : Optional.empty();
        reportSeekPending = s.flag("report_seek_pending");
        reportDurableOnly = s.flag("report_durable_only");
        reportUpdatedUnixNs = s.optional("report_updated_unix_ns");
        updateAge = s.duration("update_age_ns");
        lag = s.u64("lag_records") == MetricsValues.UNKNOWN
                ? Optional.empty()
                : Optional.of(new Lag(s.u64("lag_records"), s.optional("lag_bytes")));
        lagUnknown = LagUnknown.of(s.i32("lag_unknown"));
        oldestUnreadLocalTsNs = s.optional("oldest_unread_local_ts_ns");
        unreadAge = s.duration("unread_age_ns");
    }

    /** The beacon's name (its file name in the beacon directory). */
    public String name() {
        return name;
    }

    /** Whether its owner is running, and whether the file could be read. */
    public BeaconState state() {
        return state;
    }

    /** The owner's PID as it recorded it (informational). */
    public OptionalLong reportPid() {
        return reportPid;
    }

    /** The sequence number of the next record the reader returns. */
    public OptionalLong reportNextSeq() {
        return reportNextSeq;
    }

    /** The slot the reader reads next. */
    public Optional<Position> reportSlot() {
        return reportSlot;
    }

    /** True if a seek was pending at the last write. */
    public boolean isReportSeekPending() {
        return reportSeekPending;
    }

    /** True if the reader is durable-only. */
    public boolean isReportDurableOnly() {
        return reportDurableOnly;
    }

    /** The owner's wall-clock time of the last write, in nanoseconds since the Unix epoch. */
    public OptionalLong reportUpdatedUnixNs() {
        return reportUpdatedUnixNs;
    }

    /** The time since the last write by the observer's clock (empty if the write time is in the future). */
    public Optional<Duration> updateAge() {
        return updateAge;
    }

    /** The reader's lag as of its last write, or empty with the reason in {@link #lagUnknown()}. */
    public Optional<Lag> lag() {
        return lag;
    }

    /** Why {@link #lag()} is empty for a beacon that could be read. */
    public LagUnknown lagUnknown() {
        return lagUnknown;
    }

    /** The {@code local_ts_ns} of the first record the reader has not returned. */
    public OptionalLong oldestUnreadLocalTsNs() {
        return oldestUnreadLocalTsNs;
    }

    /**
     * The observer's clock minus {@link #oldestUnreadLocalTsNs()} (zero if it has caught up); meaningful when the
     * producer stamps {@code local_ts_ns} from this host's wall clock.
     */
    public Optional<Duration> unreadAge() {
        return unreadAge;
    }

    private Object[] values() {
        return new Object[] {name, state, hasReport, reportPid, reportNextSeq, reportSlot, reportSeekPending,
            reportDurableOnly, reportUpdatedUnixNs, updateAge, lag, lagUnknown, oldestUnreadLocalTsNs, unreadAge};
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof BeaconStatus b && Arrays.equals(values(), b.values());
    }

    @Override
    public int hashCode() {
        return Arrays.hashCode(values());
    }

    @Override
    public String toString() {
        return "BeaconStatus[name=" + name + ", state=" + state + ", reportNextSeq=" + reportNextSeq + ", lag=" + lag
                + ", lagUnknown=" + lagUnknown + ", updateAge=" + updateAge + ", unreadAge=" + unreadAge + "]";
    }
}
