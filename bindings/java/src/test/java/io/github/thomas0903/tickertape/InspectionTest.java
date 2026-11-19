package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.lang.reflect.Modifier;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.List;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Verification, segment listing, the checkpoint marker and retention. */
class InspectionTest {
    @TempDir
    Path tmp;

    private Path journal(int bigRecords, boolean checkpoint) {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < bigRecords; i++) {
                w.append(1, 0, 0, new byte[60_000]);
            }
            if (checkpoint) {
                w.checkpoint();
            }
            w.append(1, 0, 0, new byte[10]);
        }
        return dir;
    }

    @Test
    void verifyListAndCheckpoint() {
        Path dir = journal(4, true);
        VerifyReport report = Journal.verify(dir);
        assertTrue(report.isIntact());
        assertEquals(5, report.records());
        assertEquals(0, report.firstSeq());
        assertEquals(4, report.lastSeq());
        assertEquals(4, report.segments());
        assertEquals(CorruptReason.NONE, report.firstProblemReason());

        List<SegmentInfo> segments = Journal.listSegments(dir);
        assertEquals(4, segments.size());
        for (int i = 0; i < 4; i++) {
            SegmentInfo s = segments.get(i);
            assertEquals(i, s.baseSeq());
            assertEquals(65_536, s.segmentSize());
            assertEquals(1, s.formatVersion());
            assertTrue(s.createdUnixNs() > 0);
            assertTrue(Files.exists(dir.resolve(s.fileName())), s.fileName());
        }
        assertEquals("00000000000000000003.tape", segments.get(3).fileName());

        Checkpoint c = Journal.readCheckpoint(dir).orElseThrow();
        assertEquals(1, c.generation());
        assertEquals(4, c.nextSeq());
        assertEquals(3, c.segmentBaseSeq());
    }

    @Test
    void noCheckpointMarker() {
        Path dir = journal(1, false);
        assertTrue(Journal.readCheckpoint(dir).isEmpty());
    }

    @Test
    void applyRetentionPlansDeletesAndMoves() throws IOException {
        Path dir = journal(5, true);
        RetentionReport plan = Journal.applyRetention(dir, Retention.delete().maxSegments(2), true);
        assertTrue(plan.removedSegments() > 0, plan.toString());
        assertEquals(plan.removedSegments() * 65_536, plan.removedBytes());
        assertEquals(0, plan.alreadyRemovedSegments());
        assertEquals(5, Journal.listSegments(dir).size(), "a dry run changes nothing");

        Path archive = Files.createDirectory(tmp.resolve("archive"));
        RetentionReport moved = Journal.applyRetention(dir, Retention.moveTo(archive).maxSegments(4), false);
        assertEquals(1, moved.removedSegments());
        assertTrue(Files.exists(archive.resolve("00000000000000000000.tape")));

        RetentionReport deleted = Journal.applyRetention(dir, Retention.delete().maxBytes(0)
                .maxAge(Duration.ofDays(3650)), false);
        assertTrue(deleted.removedSegments() >= 1, deleted.toString());
        assertEquals(Journal.listSegments(dir).get(0).baseSeq(), 1 + deleted.removedSegments());
    }

    @Test
    void retentionValues() {
        Retention r = Retention.moveTo(tmp).maxSegments(3).maxBytes(10).maxAge(Duration.ofSeconds(1));
        assertEquals(tmp, r.moveToDirectory());
        assertEquals(r, Retention.moveTo(tmp).maxSegments(3).maxBytes(10).maxAge(Duration.ofSeconds(1)));
        assertTrue(r.toString().contains("maxSegments(3)"), r.toString());
        assertEquals(null, Retention.delete().moveToDirectory());
    }

    /**
     * The reports come from C structs that grow (they carry {@code struct_size}), so they are final classes that can
     * gain accessors, not records whose canonical constructor and pattern would change (ADR 0037 -> Value types). The
     * frozen structs stay records.
     */
    @Test
    void growableReportsAreClassesAndFrozenValuesAreRecords() throws Exception {
        for (Class<?> c : List.of(VerifyReport.class, RetentionReport.class)) {
            assertFalse(c.isRecord(), c + " must not be a record");
            assertTrue(Modifier.isFinal(c.getModifiers()), c + " must be final");
            assertEquals(0, c.getConstructors().length, c + " must have no public constructor");
        }
        for (Class<?> c : List.of(Checkpoint.class, SegmentInfo.class, RecordCopy.class)) {
            assertTrue(c.isRecord(), c + " is a frozen value");
        }
        Path dir = journal(2, true);
        VerifyReport a = Journal.verify(dir);
        VerifyReport b = Journal.verify(dir);
        assertEquals(a, b);
        assertEquals(a.hashCode(), b.hashCode());
        assertTrue(a.toString().startsWith("VerifyReport[segments=" + a.segments() + ", records=" + a.records()),
                a.toString());
        RetentionReport plan = Journal.applyRetention(dir, Retention.delete().maxSegments(1), true);
        assertEquals(plan, Journal.applyRetention(dir, Retention.delete().maxSegments(1), true));
        assertTrue(plan.toString().contains("removedSegments=" + plan.removedSegments()), plan.toString());
    }
}
