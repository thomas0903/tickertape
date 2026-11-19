package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.Optional;

/**
 * Operations on a journal directory that need no writer or reader: verification, segment listing, the checkpoint
 * marker and retention. Each takes no lock and is safe while a writer is active. Callable from any thread.
 */
public final class Journal {
    private Journal() {}

    private static long off(StructLayout layout, String name) {
        return layout.byteOffset(PathElement.groupElement(name));
    }

    /**
     * Checks every segment in {@code dir}: every header, record and seal, and the chain between segments. Corruption
     * is reported in the result ({@link VerifyReport#problems()}), not thrown. Uncommitted bytes after the end of the
     * newest segment are not a problem. The newest segment may grow during the scan. A state an active writer passes
     * through (a segment roll in progress) is scanned again for up to 630 ms, so only a writer stalled in it for longer
     * can make the scan report a problem that is not there (a {@link CorruptReason#BROKEN_CHAIN}); verify again to tell.
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws JournalNotFoundException if {@code dir} has no segments
     * @throws JournalIoException if reading fails (with {@code ENOENT} if {@code dir} does not exist)
     */
    public static VerifyReport verify(Path dir) {
        Objects.requireNonNull(dir, "dir");
        NativeLibrary.lookup();
        StructLayout l = Signatures.VERIFY_REPORT;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment report = arena.allocate(l);
            report.set(JAVA_LONG, off(l, "struct_size"), l.byteSize());
            Errors.check(Native.verify(Paths.toC(dir, arena), report));
            return new VerifyReport(report.get(JAVA_LONG, off(l, "segments")), report.get(JAVA_LONG, off(l, "records")),
                    report.get(JAVA_LONG, off(l, "first_seq")), report.get(JAVA_LONG, off(l, "last_seq")),
                    report.get(JAVA_LONG, off(l, "problems")), report.get(JAVA_LONG, off(l, "first_problem_base_seq")),
                    report.get(JAVA_LONG, off(l, "first_problem_offset")),
                    report.get(JAVA_INT, off(l, "first_problem_reason")));
        }
    }

    /**
     * Lists the segments in {@code dir}, oldest first. The list can change while a writer is active.
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws JournalNotFoundException if there are no segments
     * @throws CorruptJournalException if a segment header is invalid
     * @throws UnsupportedFormatVersionException if a segment has an unsupported format version
     * @throws JournalIoException if reading fails
     */
    public static List<SegmentInfo> listSegments(Path dir) {
        Objects.requireNonNull(dir, "dir");
        NativeLibrary.lookup();
        StructLayout l = Signatures.SEGMENT_INFO;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment path = Paths.toC(dir, arena);
            MemorySegment count = arena.allocate(JAVA_LONG);
            Errors.check(Native.listSegments(path, MemorySegment.NULL, 0, count));
            for (;;) {
                long capacity = count.get(JAVA_LONG, 0) + 4; // room for segments created meanwhile
                MemorySegment infos = arena.allocate(l, capacity);
                int status = Native.listSegments(path, infos, capacity, count);
                if (status == Status.BUFFER_TOO_SMALL.code()) {
                    continue;
                }
                Errors.check(status);
                int n = (int) count.get(JAVA_LONG, 0);
                List<SegmentInfo> out = new ArrayList<>(n);
                for (int i = 0; i < n; i++) {
                    long base = i * l.byteSize();
                    out.add(new SegmentInfo(infos.get(JAVA_LONG, base + off(l, "base_seq")),
                            infos.get(JAVA_LONG, base + off(l, "segment_size")),
                            infos.get(JAVA_LONG, base + off(l, "created_unix_ns")),
                            infos.get(JAVA_INT, base + off(l, "format_version"))));
                }
                return List.copyOf(out);
            }
        }
    }

    /**
     * Reads the checkpoint marker of the journal in {@code dir}, or returns empty if the marker file is missing or
     * neither slot is valid. This observes the marker; it does not validate the position it describes.
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws JournalIoException if the marker exists but cannot be read
     */
    public static Optional<Checkpoint> readCheckpoint(Path dir) {
        Objects.requireNonNull(dir, "dir");
        NativeLibrary.lookup();
        StructLayout l = Signatures.CHECKPOINT;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(l);
            int status = Native.readCheckpoint(Paths.toC(dir, arena), out);
            Errors.check(status);
            if (status == Status.NONE.code()) {
                return Optional.empty();
            }
            return Optional.of(new Checkpoint(out.get(JAVA_LONG, off(l, "generation")),
                    out.get(JAVA_LONG, off(l, "next_seq")), out.get(JAVA_LONG, off(l, "segment_base_seq")),
                    out.get(JAVA_LONG, off(l, "offset"))));
        }
    }

    /**
     * Applies {@code retention} to the journal in {@code dir} once (ADR 0016) and reports what it did; with
     * {@code dryRun}, only plans and changes nothing. Only a sealed prefix strictly before the checkpoint segment is
     * eligible, removed oldest first; without a valid checkpoint marker nothing is. Stops at the first error; removals
     * before it stay in effect.
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws JournalNotFoundException if there are no segments
     * @throws CorruptJournalException if a segment header is invalid
     * @throws JournalIoException if a removal fails (including an existing destination file for a move)
     */
    public static RetentionReport applyRetention(Path dir, Retention retention, boolean dryRun) {
        Objects.requireNonNull(dir, "dir");
        Objects.requireNonNull(retention, "retention");
        NativeLibrary.lookup();
        StructLayout l = Signatures.RETENTION_REPORT;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment report = arena.allocate(l);
            report.set(JAVA_LONG, off(l, "struct_size"), l.byteSize());
            MemorySegment path = Paths.toC(dir, arena);
            MemorySegment r = retention.toNative(arena);
            int status;
            TickertapeException error = null;
            try {
                status = Native.retentionApply(path, r, dryRun ? 1 : 0, report);
                if (status < 0) {
                    error = Errors.capture(status);
                }
            } finally {
                Native.retentionFree(r);
            }
            if (error != null) {
                throw error;
            }
            return new RetentionReport(report.get(JAVA_LONG, off(l, "removed_segments")),
                    report.get(JAVA_LONG, off(l, "removed_bytes")),
                    report.get(JAVA_LONG, off(l, "already_removed_segments")));
        }
    }
}
