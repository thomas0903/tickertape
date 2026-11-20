package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.OptionalLong;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Operational metrics (ADR 0032): a status sample, the writer's metrics and a lagging reader's. */
class MetricsTest {
    @TempDir
    Path tmp;

    private static void append(Writer w, long from, long to) {
        for (long seq = from; seq < to; seq++) {
            w.append(1, 0, 2_000_000 + seq, new byte[200]);
        }
    }

    @Test
    void statusWriterAndLaggingReader() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            WriterMetrics fresh = w.metrics();
            assertEquals(0, fresh.head().nextSeq());
            assertTrue(fresh.checkpoint().isEmpty() && fresh.sinceCheckpoint().isEmpty());
            append(w, 0, 400);
            w.checkpoint();
            append(w, 400, 1000);

            WriterMetrics m = w.metrics();
            assertEquals(1000, m.head().nextSeq());
            assertTrue(m.head().segmentBaseSeq() > 0, "the journal rolled");
            assertEquals(400, m.checkpoint().orElseThrow().nextSeq());
            assertTrue(m.sinceCheckpoint().isPresent());
            assertEquals(600, m.uncheckpointedRecords());
            assertTrue(m.uncheckpointedBytes() > 0);
            assertFalse(m.isPoisoned());
            assertEquals(0, m.retentionFailures());
            assertEquals(m.head(), w.metrics().head());

            try (Reader lagging = Reader.open(dir); Reader durable = Reader.open(dir, ReaderOptions.defaults().durableOnly(true))) {
                for (int i = 0; i < 100; i++) {
                    assertTrue(lagging.tryNext() != null);
                }
                for (int i = 0; i < 30; i++) {
                    assertTrue(durable.tryNext() != null);
                }
                JournalStatus status = JournalStatus.sample(dir);
                {
                    assertEquals(1000, status.head().nextSeq());
                    assertEquals(m.head(), status.head());
                    assertEquals(HeadEnd.OPEN, status.headEnd());
                    assertEquals(CorruptReason.NONE, status.headCorruptReason());
                    assertEquals(OptionalLong.of(2_000_999), status.headLastLocalTsNs());
                    assertFalse(status.isHeadChainBroken());
                    assertTrue(status.segmentCount() > 2);
                    assertEquals(400, status.checkpoint().orElseThrow().nextSeq());
                    assertEquals(CheckpointProblem.USABLE, status.checkpointProblem());
                    assertEquals(OptionalLong.of(600), status.exposureRecords());
                    assertEquals(OptionalLong.of(2_000_400), status.exposureOldestLocalTsNs());
                    assertTrue(status.storageAvailableBytes() > 0);
                    assertEquals(65536, status.storageNextSegmentSize());
                    assertEquals(OptionalLong.of(ProcessHandle.current().pid()), status.writerPid());
                    assertEquals(java.util.Optional.of(true), status.writerAlive());
                    assertEquals(ReadersDirState.MISSING, status.readersDirState());
                    assertEquals(List.of(), status.beacons());

                    ReaderMetrics r = lagging.metrics(status);
                    assertEquals(100, r.nextSeq());
                    assertEquals(100, r.slot().nextSeq());
                    assertFalse(r.isSeekPending() || r.isDurableOnly() || r.isRecordHeld());
                    assertEquals(LagUnknown.KNOWN, r.lagUnknown());
                    assertEquals(900, r.lag().orElseThrow().records());
                    assertTrue(r.lag().orElseThrow().bytes().orElseThrow() > 0);
                    ReaderMetrics d = durable.metrics(status);
                    assertTrue(d.isDurableOnly());
                    assertEquals(370, d.lag().orElseThrow().records());

                    // Past the sample: unknown until a new one.
                    append(w, 1000, 1400);
                    while (lagging.tryNext() != null) {
                        // drain
                    }
                    ReaderMetrics ahead = lagging.metrics(status);
                    assertTrue(ahead.lag().isEmpty());
                    assertEquals(LagUnknown.AHEAD_OF_SAMPLE, ahead.lagUnknown());
                    try (JournalStatus fresher = JournalStatus.sample(dir, dir.resolve("readers"))) {
                        Lag caughtUp = lagging.metrics(fresher).lag().orElseThrow();
                        assertEquals(0, caughtUp.records());
                        assertEquals(OptionalLong.of(0), caughtUp.bytes());
                    }
                    status.close();
                    status.close(); // idempotent
                    assertEquals(1000, status.head().nextSeq(), "values stay readable after close");
                    assertThrows(IllegalStateException.class, () -> lagging.metrics(status));
                }
            }
        }
    }

    @Test
    void sampleAgreesWithTheCli() throws Exception {
        Path cli = TestSupport.cli();
        Path dir = tmp.resolve("cli");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            append(w, 0, 300);
            w.checkpoint();
            append(w, 300, 500);
            try (JournalStatus status = JournalStatus.sample(dir)) {
                TestSupport.Result out = TestSupport.runOk(List.of(cli.toString(), "status", "--json", dir.toString()), 60);
                // The test's JSON parser takes no fractions (the CLI prints ages in seconds), so read the integers.
                String json = TestSupport.lines(out.stdout()).get(0);
                assertEquals(field(json, "head", "next_seq"), status.head().nextSeq());
                assertEquals(field(json, "head", "offset"), status.head().offset());
                assertEquals(field(json, "exposure", "records"), status.exposureRecords().orElseThrow());
                assertEquals(field(json, "exposure", "bytes"), status.exposureBytes().orElseThrow());
                assertEquals(field(json, "segments", "count"), status.segmentCount());
                assertEquals(field(json, "segments", "total_bytes"), status.segmentsTotalBytes());
            }
        }
    }

    /** The integer {@code key} in the JSON object {@code object} of a {@code tickertape status --json} line. */
    private static long field(String json, String object, String key) {
        int start = json.indexOf("\"" + object + "\":{");
        assertTrue(start >= 0, object + " in " + json);
        Matcher m = Pattern.compile("\"" + key + "\":(\\d+)").matcher(json);
        assertTrue(m.find(start), key + " in " + object + " of " + json);
        return Long.parseLong(m.group(1));
    }

    @Test
    void errors() throws Exception {
        Path empty = Files.createDirectories(tmp.resolve("empty"));
        assertThrows(JournalNotFoundException.class, () -> JournalStatus.sample(empty));
        assertThrows(NullPointerException.class, () -> JournalStatus.sample(null));
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            append(w, 0, 3);
        }
        try (JournalStatus status = JournalStatus.sample(dir)) {
            assertTrue(status.checkpoint().isEmpty());
            assertEquals(AgeUnknown.KNOWN, status.checkpointAgeUnknown());
            assertEquals(OptionalLong.of(3), status.exposureRecords());
            Reader reader = Reader.open(dir);
            reader.close();
            assertThrows(IllegalStateException.class, () -> reader.metrics(status));
        }
        Writer closed = Writer.open(dir);
        closed.close();
        assertThrows(IllegalStateException.class, closed::metrics);
    }
}
