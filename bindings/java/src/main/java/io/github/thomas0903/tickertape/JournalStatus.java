package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.ref.Cleaner;
import java.lang.ref.Reference;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalLong;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.function.Function;

/**
 * One read-only sample of a journal's operational state: what {@code tickertape status} prints (ADR 0032;
 * {@code docs/operations.md}). Taking it needs no writer and disturbs none. Its cost grows with what was appended
 * since the last checkpoint ({@code docs/java-api.md} lists the files it reads).
 *
 * <p>The values are read once, by {@link #sample}, and never change; unknown values are empty, never guessed.
 * Unsigned values are returned with the same bits in a {@code long}. The sample also keeps the library's copy, which
 * {@link Reader#metrics(JournalStatus)} measures a reader's lag against; {@link #close()} frees it (a {@link Cleaner}
 * frees an unreachable sample too). The values stay readable after the close. Every method may be called from any
 * thread, concurrently.
 */
public final class JournalStatus implements AutoCloseable {
    private static final Cleaner CLEANER = Cleaner.create();

    /** Frees the library's sample; holds no reference to the owner. */
    private record Free(MemorySegment ptr) implements Runnable {
        @Override
        public void run() {
            Native.journalStatusFree(ptr);
        }
    }

    /** Readers of {@link #ptr} hold the read lock; {@link #close()} holds the write lock. */
    private final ReentrantReadWriteLock lock = new ReentrantReadWriteLock();
    private MemorySegment ptr;
    private final Cleaner.Cleanable cleanable;

    private final long sampledUnixNs;
    private final Position head;
    private final long headSegmentSize;
    private final HeadEnd headEnd;
    private final CorruptReason headCorruptReason;
    private final OptionalLong headLastLocalTsNs;
    private final boolean headVerified;
    private final boolean headChainBroken;
    private final long segmentCount;
    private final long firstSeq;
    private final long segmentsTotalBytes;
    private final OptionalLong oldestSegmentCreatedUnixNs;
    private final long segmentsBeforeCheckpoint;
    private final Optional<Checkpoint> checkpoint;
    private final CheckpointProblem checkpointProblem;
    private final OptionalLong checkpointModifiedUnixNs;
    private final Optional<Duration> checkpointEstimatedAge;
    private final AgeUnknown checkpointAgeUnknown;
    private final OptionalLong exposureRecords;
    private final OptionalLong exposureBytes;
    private final Optional<Duration> exposureEstimatedAge;
    private final OptionalLong exposureOldestLocalTsNs;
    private final boolean exposureVerified;
    private final long storageAvailableBytes;
    private final long storageTotalBytes;
    private final long storageNextSegmentSize;
    private final long storageEstimatedRollsLeft;
    private final boolean storageSpareSegment;
    private final OptionalLong writerPid;
    private final Optional<Boolean> writerAlive;
    private final ReadersDirState readersDirState;
    private final List<BeaconStatus> beacons;

    private JournalStatus(MemorySegment ptr, MetricsValues.Struct r, List<BeaconStatus> beacons) {
        this.ptr = ptr;
        sampledUnixNs = r.u64("sampled_unix_ns");
        head = new Position(r.u64("head_next_seq"), r.u64("head_segment_base_seq"), r.u64("head_offset"));
        headSegmentSize = r.u64("head_segment_size");
        headEnd = HeadEnd.of(r.i32("head_end"));
        headCorruptReason = CorruptReason.of(r.i32("head_corrupt_reason"));
        headLastLocalTsNs = r.optional("head_last_local_ts_ns");
        headVerified = r.flag("head_verified");
        headChainBroken = r.flag("head_chain_broken");
        segmentCount = r.u64("segment_count");
        firstSeq = r.u64("first_seq");
        segmentsTotalBytes = r.u64("segments_total_bytes");
        oldestSegmentCreatedUnixNs = r.optional("oldest_segment_created_unix_ns");
        segmentsBeforeCheckpoint = r.u64("segments_before_checkpoint");
        checkpoint = r.flag("has_checkpoint")
                ? Optional.of(new Checkpoint(r.u64("checkpoint_generation"), r.u64("checkpoint_next_seq"),
                        r.u64("checkpoint_segment_base_seq"), r.u64("checkpoint_offset")))
                : Optional.empty();
        checkpointProblem = CheckpointProblem.of(r.i32("checkpoint_problem"));
        checkpointModifiedUnixNs = r.optional("checkpoint_modified_unix_ns");
        checkpointEstimatedAge = r.duration("checkpoint_estimated_age_ns");
        checkpointAgeUnknown = AgeUnknown.of(r.i32("checkpoint_age_unknown"));
        exposureRecords = r.optional("exposure_records");
        exposureBytes = r.optional("exposure_bytes");
        exposureEstimatedAge = r.duration("exposure_estimated_age_ns");
        exposureOldestLocalTsNs = r.optional("exposure_oldest_local_ts_ns");
        exposureVerified = r.flag("exposure_verified");
        storageAvailableBytes = r.u64("storage_available_bytes");
        storageTotalBytes = r.u64("storage_total_bytes");
        storageNextSegmentSize = r.u64("storage_next_segment_size");
        storageEstimatedRollsLeft = r.u64("storage_estimated_rolls_left");
        storageSpareSegment = r.flag("storage_spare_segment");
        writerPid = r.optional("writer_pid");
        long alive = r.u64("writer_alive");
        writerAlive = alive == MetricsValues.UNKNOWN ? Optional.empty() : Optional.of(alive != 0);
        readersDirState = ReadersDirState.of(r.i32("readers_dir_state"));
        this.beacons = Collections.unmodifiableList(beacons);
        this.cleanable = CLEANER.register(this, new Free(ptr)); // last: nothing above can throw after it
    }

    /** Samples the journal in {@code dir}, reading reader beacons from {@code dir/readers}. */
    public static JournalStatus sample(Path dir) {
        return sample(dir, null);
    }

    /**
     * Samples the journal in {@code dir}, reading reader beacons from {@code readersDir} ({@code dir/readers} if it is
     * null). A beacon that cannot be read, or a beacon directory that cannot be listed, is reported in the sample and
     * never fails it.
     *
     * @throws IllegalArgumentException if a path is not on the default file system
     * @throws JournalNotFoundException if {@code dir} has no segments
     * @throws CorruptJournalException if the newest segment's header is invalid, or there is no segment but the
     *     checkpoint marker names one
     * @throws UnsupportedFormatVersionException if the newest segment has an unsupported format version
     * @throws JournalIoException if reading fails
     */
    @SuppressWarnings("restricted") // reads the beacon names (at most 64 bytes and a NUL) the library returns
    public static JournalStatus sample(Path dir, Path readersDir) {
        Objects.requireNonNull(dir, "dir");
        NativeLibrary.lookup();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ADDRESS);
            MemorySegment readers = readersDir == null ? MemorySegment.NULL : Paths.toC(readersDir, arena);
            Errors.check(Native.journalStatusSample(Paths.toC(dir, arena), readers, out));
            MemorySegment ptr = out.get(ADDRESS, 0);
            try {
                MemorySegment report = MetricsValues.sized(arena, Signatures.JOURNAL_STATUS_REPORT);
                Errors.check(Native.journalStatusGetReport(ptr, report));
                MetricsValues.Struct r = new MetricsValues.Struct(report, Signatures.JOURNAL_STATUS_REPORT);
                long count = r.u64("beacon_count");
                List<BeaconStatus> beacons = new ArrayList<>();
                MemorySegment beacon = MetricsValues.sized(arena, Signatures.BEACON_STATUS);
                for (long i = 0; i < count; i++) {
                    beacon.set(JAVA_LONG, 0, Signatures.BEACON_STATUS.byteSize());
                    Errors.check(Native.journalStatusGetBeacon(ptr, i, beacon));
                    String name = Native.journalStatusBeaconName(ptr, i).reinterpret(65).getString(0);
                    beacons.add(new BeaconStatus(name, new MetricsValues.Struct(beacon, Signatures.BEACON_STATUS)));
                }
                return new JournalStatus(ptr, r, beacons);
            } catch (RuntimeException | Error e) {
                Native.journalStatusFree(ptr);
                throw e;
            }
        }
    }

    /** Runs {@code body} with the library's sample, which stays valid until it returns. */
    <T> T withHandle(Function<MemorySegment, T> body) {
        lock.readLock().lock();
        try {
            if (ptr == null) {
                throw new IllegalStateException("the JournalStatus is closed");
            }
            return body.apply(ptr);
        } finally {
            lock.readLock().unlock();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Frees the library's copy of the sample. Idempotent; waits for {@link Reader#metrics(JournalStatus)} calls on
     * other threads that use it. The values stay readable; {@link Reader#metrics(JournalStatus)} then throws
     * {@link IllegalStateException}.
     */
    @Override
    public void close() {
        lock.writeLock().lock();
        try {
            if (ptr != null) {
                ptr = null;
                cleanable.clean();
            }
        } finally {
            lock.writeLock().unlock();
            Reference.reachabilityFence(this);
        }
    }

    /** The observer's wall clock when the sample was taken, ns since the Unix epoch; ages are measured from it. */
    public long sampledUnixNs() {
        return sampledUnixNs;
    }

    /** The head: where the next record will go, as far as the newest segment shows. */
    public Position head() {
        return head;
    }

    /** The size of the newest segment. */
    public long headSegmentSize() {
        return headSegmentSize;
    }

    /** What the sample found at the head. */
    public HeadEnd headEnd() {
        return headEnd;
    }

    /** Why the head is at invalid bytes ({@link HeadEnd#INVALID}); {@link CorruptReason#NONE} otherwise. */
    public CorruptReason headCorruptReason() {
        return headCorruptReason;
    }

    /** The {@code local_ts_ns} of the last record the scan passed in the newest segment, if set. */
    public OptionalLong headLastLocalTsNs() {
        return headLastLocalTsNs;
    }

    /**
     * True if the sample validated every record from the checkpoint marker's position to the head. False with no
     * corruption reported does not mean that a writer would open the journal.
     */
    public boolean isHeadVerified() {
        return headVerified;
    }

    /** True if the segment listing alone proves a segment missing: a writer refuses the journal (corruption). */
    public boolean isHeadChainBroken() {
        return headChainBroken;
    }

    /** The number of segment files. */
    public long segmentCount() {
        return segmentCount;
    }

    /** The base sequence number of the oldest segment. */
    public long firstSeq() {
        return firstSeq;
    }

    /** The sum of the segment files' (preallocated) sizes. */
    public long segmentsTotalBytes() {
        return segmentsTotalBytes;
    }

    /** The creation time from the oldest segment's header (the writer's clock), if it could be read. */
    public OptionalLong oldestSegmentCreatedUnixNs() {
        return oldestSegmentCreatedUnixNs;
    }

    /** The segments older than the checkpoint marker's segment (0 without a marker). */
    public long segmentsBeforeCheckpoint() {
        return segmentsBeforeCheckpoint;
    }

    /** The current checkpoint marker, if there is a valid one. */
    public Optional<Checkpoint> checkpoint() {
        return checkpoint;
    }

    /** Why the marker cannot be used, if it cannot: a writer refuses the journal (corruption). */
    public CheckpointProblem checkpointProblem() {
        return checkpointProblem;
    }

    /** The marker file's modification time, ns since the Unix epoch, if the filesystem reports one. */
    public OptionalLong checkpointModifiedUnixNs() {
        return checkpointModifiedUnixNs;
    }

    /** The estimated time since the last checkpoint, or empty with the reason in {@link #checkpointAgeUnknown()}. */
    public Optional<Duration> checkpointEstimatedAge() {
        return checkpointEstimatedAge;
    }

    /** Why {@link #checkpointEstimatedAge()} is empty ({@link AgeUnknown#KNOWN} if it is not, or without a marker). */
    public AgeUnknown checkpointAgeUnknown() {
        return checkpointAgeUnknown;
    }

    /** The committed records a power loss now could remove, if known. */
    public OptionalLong exposureRecords() {
        return exposureRecords;
    }

    /** The segment-file footprint between the marker's position and the head, if known. */
    public OptionalLong exposureBytes() {
        return exposureBytes;
    }

    /** An upper bound on how long the uncovered records have been exposed (zero if there are none), if known. */
    public Optional<Duration> exposureEstimatedAge() {
        return exposureEstimatedAge;
    }

    /** The {@code local_ts_ns} of the oldest uncovered record, if known and set. */
    public OptionalLong exposureOldestLocalTsNs() {
        return exposureOldestLocalTsNs;
    }

    /** True if the exposure counts come from records the sample validated. */
    public boolean isExposureVerified() {
        return exposureVerified;
    }

    /** The bytes available to unprivileged users on the journal's filesystem. */
    public long storageAvailableBytes() {
        return storageAvailableBytes;
    }

    /** The size of the journal's filesystem. */
    public long storageTotalBytes() {
        return storageTotalBytes;
    }

    /** The size assumed for the segments still to come. */
    public long storageNextSegmentSize() {
        return storageNextSegmentSize;
    }

    /** The estimated number of further segment rolls that fit (optimistic on btrfs). */
    public long storageEstimatedRollsLeft() {
        return storageEstimatedRollsLeft;
    }

    /** True if a fully allocated prepared next segment exists (not counted in the rolls left). */
    public boolean isSpareSegmentPresent() {
        return storageSpareSegment;
    }

    /** The PID the most recent writer recorded in {@code writer.lock} (informational; it stays after it exits). */
    public OptionalLong writerPid() {
        return writerPid;
    }

    /** Whether a process with {@link #writerPid()} exists (a PID can be reused), if known. */
    public Optional<Boolean> writerAlive() {
        return writerAlive;
    }

    /** Whether the beacon directory could be listed. */
    public ReadersDirState readersDirState() {
        return readersDirState;
    }

    /** The reader beacons, sorted by name (unmodifiable). */
    public List<BeaconStatus> beacons() {
        return beacons;
    }

    @Override
    public String toString() {
        return "JournalStatus[head=" + head + ", headEnd=" + headEnd + ", checkpoint=" + checkpoint + ", exposureRecords="
                + exposureRecords + ", storageAvailableBytes=" + storageAvailableBytes + ", beacons=" + beacons.size()
                + "]";
    }
}
