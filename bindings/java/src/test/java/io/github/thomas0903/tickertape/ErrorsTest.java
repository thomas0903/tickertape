package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.io.RandomAccessFile;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Errors produced for real through the library, with their details. {@code HeaderTest} checks the mapping of every
 * status code; this checks that the details (message, OS error, corruption reason) arrive from the failing call.
 */
class ErrorsTest {
    private static final int ENOENT = 2;

    @TempDir
    Path tmp;

    @Test
    void missingDirectoryIsIoWithEnoent() {
        JournalIoException e = assertThrows(JournalIoException.class, () -> Reader.open(tmp.resolve("missing")));
        assertEquals(Status.IO, e.status());
        assertEquals(ENOENT, e.osErrorCode());
        assertTrue(e.getMessage().contains("missing"), e.getMessage());
        JournalIoException v = assertThrows(JournalIoException.class, () -> Journal.verify(tmp.resolve("missing")));
        assertEquals(ENOENT, v.osErrorCode());
    }

    @Test
    void emptyDirectoryIsNotFound() throws IOException {
        Path dir = Files.createDirectory(tmp.resolve("empty"));
        assertThrows(JournalNotFoundException.class, () -> Reader.open(dir));
        assertThrows(JournalNotFoundException.class, () -> Journal.listSegments(dir));
    }

    @Test
    void secondWriterIsLocked() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            JournalLockedException e = assertThrows(JournalLockedException.class,
                    () -> Writer.open(dir, TestSupport.small()));
            assertEquals(Status.LOCKED, e.status());
            assertEquals(0, w.nextSeq());
        }
    }

    @Test
    void invalidConfigAndArguments() {
        Path dir = tmp.resolve("j");
        assertThrows(InvalidConfigException.class, () -> Writer.open(dir, WriterOptions.defaults().segmentSize(1000)));
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            assertThrows(RecordTooLargeException.class, () -> w.append(1, 0, 0, new byte[70_000]));
            assertThrows(RecordTooLargeException.class, () -> w.appendWith(1, 0, 0, 70_000, p -> {}));
            assertThrows(IndexOutOfBoundsException.class, () -> w.append(1, 0, 0, new byte[4], 2, 3));
            assertThrows(IllegalArgumentException.class, () -> w.appendWith(1, 0, 0, -1, p -> {}));
            assertThrows(NullPointerException.class, () -> w.appendWith(1, 0, 0, 1, null));
            w.checkpoint();
        }
        assertThrows(InvalidConfigException.class, () -> Writer.open(tmp.resolve("k"),
                WriterOptions.defaults().segmentSize(1L << 41)));
    }

    @Test
    void batchPartialFailureSaysWhatWasAppended() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[1]);
            AppendBatch batch = new AppendBatch()
                    .add(1, 0, 0, new byte[10])
                    .add(2, 0, 0, new byte[10])
                    .add(3, 0, 0, new byte[70_000])
                    .add(4, 0, 0, new byte[10]);
            BatchAppendException e = assertThrows(BatchAppendException.class, () -> w.appendBatch(batch));
            assertEquals(2, e.appended());
            assertEquals(1, e.firstSeq());
            assertEquals(Status.RECORD_TOO_LARGE, e.status());
            assertInstanceOf(RecordTooLargeException.class, e.getCause());
            assertEquals(3, w.nextSeq(), "the records before the failing one stay appended");
            assertEquals(3, w.appendBatch(new AppendBatch().add(5, 0, 0, new byte[1])));
        }
    }

    @Test
    void corruptionNamesTheReasonAndTheReaderStaysPut() throws IOException {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < 10; i++) {
                w.append(1, 0, 0, new byte[100]);
            }
        }
        // Flip a payload byte of a record in the middle of the first segment.
        Path segment = dir.resolve(Journal.listSegments(dir).get(0).fileName());
        long offset = findPayloadOf(segment, 5);
        assertTrue(Files.size(segment) > offset);
        try (RandomAccessFile f = new RandomAccessFile(segment.toFile(), "rw")) {
            f.seek(offset);
            f.write(0x5A);
        }
        try (Reader r = Reader.open(dir)) {
            r.seek(5);
            CorruptJournalException e = assertThrows(CorruptJournalException.class, r::tryNext);
            assertEquals(CorruptReason.BAD_CHECKSUM, e.reason());
            assertEquals(Status.CORRUPT, e.status());
            assertEquals(5, r.position(), "the cursor did not move");
            assertThrows(CorruptJournalException.class, r::tryNext, "the same position fails again");
        }
        VerifyReport report = Journal.verify(dir);
        assertEquals(1, report.problems());
        assertEquals(CorruptReason.BAD_CHECKSUM, report.firstProblemReason());
        assertEquals(0, report.firstProblemBaseSeq());
        assertTrue(!report.isIntact());
    }

    /**
     * The offset of a byte inside record {@code seq}'s payload ({@code docs/format.md}): records start at offset 64,
     * each a 40-byte header plus the payload, padded to 8 bytes; these 100-byte payloads make 144-byte records.
     */
    private static long findPayloadOf(Path segment, int seq) {
        return 64 + 144L * seq + 40 + 10;
    }

    @Test
    void seekBeforeTheOldestSegmentIsUnavailable() {
        Path dir = tmp.resolve("j");
        Retention keepOne = Retention.delete().maxSegments(0);
        try (Writer w = Writer.open(dir, TestSupport.small().retention(keepOne))) {
            for (int i = 0; i < 5; i++) {
                w.append(1, 0, 0, new byte[60_000]);
            }
            w.checkpoint();
            assertTrue(w.lastRetentionError().isEmpty());
        }
        List<SegmentInfo> segments = Journal.listSegments(dir);
        assertTrue(segments.get(0).baseSeq() > 0, "retention removed the oldest segments: " + segments);
        try (Reader r = Reader.open(dir)) {
            SequenceUnavailableException e = assertThrows(SequenceUnavailableException.class, () -> r.seek(0));
            assertEquals(Status.SEQ_UNAVAILABLE, e.status());
            r.seek(segments.get(0).baseSeq());
            assertEquals(segments.get(0).baseSeq(), r.tryNext().seq());
        }
    }

    @Test
    void retentionErrorsAreReportedNotThrown() throws IOException {
        Path dir = tmp.resolve("j");
        Path archive = Files.createDirectory(tmp.resolve("archive"));
        Retention move = Retention.moveTo(archive).maxSegments(0);
        try (Writer w = Writer.open(dir, TestSupport.small().retention(move))) {
            w.append(1, 0, 0, new byte[60_000]);
            w.append(1, 0, 0, new byte[60_000]);
            w.append(1, 0, 0, new byte[60_000]);
            // A file of the same name in the archive makes the move fail.
            Files.writeString(archive.resolve(Journal.listSegments(dir).get(0).fileName()), "in the way");
            w.checkpoint();
            TickertapeException e = w.lastRetentionError().orElseThrow();
            assertInstanceOf(JournalIoException.class, e);
            assertEquals(Status.IO, e.status());
        }
    }

    @Test
    void theLastErrorDetailsComeFromTheFailingCall() {
        // Two different failures in a row on one thread: each exception carries its own details.
        JournalIoException io = assertThrows(JournalIoException.class, () -> Reader.open(tmp.resolve("a")));
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            JournalLockedException locked = assertThrows(JournalLockedException.class,
                    () -> Writer.open(dir, TestSupport.small()));
            assertTrue(!locked.getMessage().equals(io.getMessage()));
            assertNull(locked.getCause());
            assertEquals(0, w.nextSeq());
        }
    }
}
