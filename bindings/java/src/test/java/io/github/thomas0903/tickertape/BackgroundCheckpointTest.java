package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Path;
import java.util.List;
import java.util.Optional;
import java.util.OptionalLong;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Background checkpoints (ADR 0038) against a live journal: the request returns its frontier at once, appends
 * continue, the progress reports the pending frontier and then the marker, a durable-only reader sees the marker move,
 * and close waits for a checkpoint in flight. The binding's test build has no fault injection: failures on the
 * background path are tested through the C ABI in the library's Rust tests.
 */
class BackgroundCheckpointTest {
    @TempDir
    Path tmp;

    private static void append(Writer w, long from, long to) {
        for (long seq = from; seq < to; seq++) {
            assertEquals(seq, w.append(1, 0, 0, new byte[200]));
        }
    }

    private static CheckpointProgress waitFinished(Writer w, long frontier) throws InterruptedException {
        long deadline = System.nanoTime() + 30_000_000_000L;
        CheckpointProgress progress = w.checkpointProgress();
        while (progress.pending().isPresent()) {
            assertEquals(frontier, progress.pending().getAsLong());
            assertTrue(System.nanoTime() < deadline, "the background checkpoint did not finish");
            Thread.sleep(1);
            progress = w.checkpointProgress();
        }
        return progress;
    }

    @Test
    void requestProgressAndDurableOnlyReader() throws Exception {
        Path dir = tmp.resolve("j");
        WriterOptions options = TestSupport.small().earlyWriteback(true).helperCpus(HelperCpus.process());
        try (Writer w = Writer.open(dir, options);
                Reader durable = Reader.open(dir, ReaderOptions.defaults().durableOnly(true))) {
            CheckpointProgress fresh = w.checkpointProgress();
            assertTrue(fresh.durable().isEmpty());
            assertEquals(OptionalLong.empty(), fresh.pending());
            append(w, 0, 400);
            assertNull(durable.tryNext());

            assertEquals(400, w.requestCheckpoint());
            append(w, 400, 800); // rolls while the checkpoint may still run
            CheckpointProgress done = waitFinished(w, 400);
            Checkpoint marker = done.durable().orElseThrow();
            assertEquals(400, marker.nextSeq());
            assertEquals(marker, Journal.readCheckpoint(dir).orElseThrow());
            for (long seq = 0; seq < 400; seq++) {
                RecordView r = durable.tryNext();
                assertNotNull(r, "record " + seq);
                assertEquals(seq, r.seq());
            }
            assertNull(durable.tryNext());

            // Requests made in a row coalesce or queue; each covers what was appended before it.
            assertEquals(800, w.requestCheckpoint());
            append(w, 800, 810);
            assertEquals(810, w.requestCheckpoint());
            assertEquals(810, waitFinished(w, 810).durable().orElseThrow().nextSeq());

            assertEquals(810, w.requestCheckpoint());
            w.checkpoint(); // waits for it, then checkpoints the same frontier again
            assertEquals(810, w.checkpointProgress().durable().orElseThrow().nextSeq());
            assertTrue(w.checkpointProgress().pending().isEmpty());

            append(w, 810, 820);
            assertEquals(820, w.requestCheckpoint());
        } // close waits for the checkpoint in flight
        assertEquals(820, Journal.readCheckpoint(dir).orElseThrow().nextSeq());
    }

    @Test
    void closedWriterRefusesAndOptionsAreValues() {
        Path dir = tmp.resolve("closed");
        Writer w = Writer.open(dir, TestSupport.small().helperCpus(HelperCpus.list(0)));
        w.close();
        assertThrows(IllegalStateException.class, w::requestCheckpoint);
        assertThrows(IllegalStateException.class, w::checkpointProgress);

        WriterOptions defaults = WriterOptions.defaults();
        assertTrue(defaults.isEarlyWritebackEnabled());
        assertEquals(HelperCpus.process(), defaults.helperCpus());
        WriterOptions custom = defaults.earlyWriteback(false).helperCpus(HelperCpus.list(3, 1));
        assertEquals(List.of(3, 1), custom.helperCpus().cpus());
        assertEquals(custom, defaults.earlyWriteback(false).helperCpus(HelperCpus.list(3, 1)));
        assertTrue(custom.toString().contains("earlyWriteback=false"), custom.toString());
        assertTrue(defaults.spareSegment().isEmpty());
        assertEquals(Optional.of(true), custom.spareSegment(true).spareSegment());
        assertEquals(custom.spareSegment(false), custom.spareSegment(true).spareSegment(false));
        assertTrue(!custom.equals(custom.spareSegment(false)));
        assertTrue(custom.spareSegment(true).toString().contains("spareSegment=true"));
        Writer.open(tmp.resolve("spare"), TestSupport.small().spareSegment(true)).close();
        assertEquals(List.of(), HelperCpus.inherited().cpus());
        assertThrows(IllegalArgumentException.class, () -> HelperCpus.list(-1));
        assertThrows(NullPointerException.class, () -> defaults.helperCpus(null));
        // Each placement reaches the library (which accepts an empty list: the helpers then use the process's CPUs).
        for (HelperCpus cpus : List.of(HelperCpus.process(), HelperCpus.inherited(), HelperCpus.list())) {
            try (Writer opened = Writer.open(dir, TestSupport.small().helperCpus(cpus))) {
                assertEquals(0, opened.requestCheckpoint());
            }
        }
    }
}
