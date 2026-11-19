package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** The threading rules: what is allowed works, and what the C ABI leaves undefined is refused, never a crash. */
class ThreadingTest {
    @TempDir
    Path tmp;

    @Test
    void concurrentUseOfOneReaderIsRefused() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small()); Reader r = Reader.open(dir,
                ReaderOptions.defaults().waitSlice(Duration.ofMillis(50)))) {
            CountDownLatch waiting = new CountDownLatch(1);
            AtomicReference<Object> other = new AtomicReference<>();
            Thread t = Thread.ofPlatform().start(() -> {
                waiting.countDown();
                try {
                    other.set(r.next(2, TimeUnit.SECONDS));
                } catch (Throwable e) {
                    other.set(e);
                }
            });
            waiting.await();
            Thread.sleep(100); // t is inside next()
            IllegalStateException e = assertThrows(IllegalStateException.class, r::tryNext);
            assertTrue(e.getMessage().contains("in use by another thread"), e.getMessage());
            assertThrows(IllegalStateException.class, r::position);
            w.append(1, 0, 0, new byte[] {1});
            t.join(5_000);
            assertTrue(other.get() instanceof RecordView, String.valueOf(other.get()));
        }
    }

    @Test
    void concurrentUseOfOneWriterIsRefused() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            CountDownLatch inFill = new CountDownLatch(1);
            CountDownLatch release = new CountDownLatch(1);
            Thread t = Thread.ofPlatform().start(() -> w.appendWith(1, 0, 0, 1, p -> {
                inFill.countDown();
                release.await();
            }));
            inFill.await();
            assertThrows(IllegalStateException.class, () -> w.append(1, 0, 0, new byte[1]));
            assertThrows(IllegalStateException.class, w::checkpoint);
            // close() from another thread waits for the append to finish.
            Thread closer = Thread.ofPlatform().start(w::close);
            Thread.sleep(50);
            assertTrue(closer.isAlive(), "close waits for the call in progress");
            release.countDown();
            t.join(5_000);
            closer.join(5_000);
            assertFalse(closer.isAlive());
            assertTrue(w.isClosed());
        }
        try (Reader r = Reader.open(dir)) {
            assertEquals(0, r.tryNext().seq(), "the append that was in progress completed");
        }
    }

    @Test
    void handlesMoveBetweenThreadsBetweenCalls() throws Exception {
        Path dir = tmp.resolve("j");
        ExecutorService pool = Executors.newFixedThreadPool(4);
        try (Writer w = Writer.open(dir, TestSupport.small()); Reader r = Reader.open(dir)) {
            for (int i = 0; i < 200; i++) {
                int n = i;
                pool.submit(() -> w.append(1, 0, 0, new byte[] {(byte) n})).get();
            }
            for (long seq = 0; seq < 200; seq++) {
                long expected = seq;
                long got = pool.submit(() -> r.tryNext().seq()).get();
                assertEquals(expected, got);
            }
        } finally {
            pool.shutdownNow();
        }
    }

    @Test
    void separateReadersOnSeparateThreadsFollowOneWriter() throws Exception {
        Path dir = tmp.resolve("j");
        int count = 5000;
        int readers = 4;
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.nextSeq();
            ExecutorService pool = Executors.newFixedThreadPool(readers);
            List<Future<Long>> sums = new ArrayList<>();
            for (int i = 0; i < readers; i++) {
                boolean mapped = i % 2 == 0;
                sums.add(pool.submit(() -> {
                    long sum = 0;
                    try (Reader r = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir)) {
                        for (int n = 0; n < count; n++) {
                            RecordView v = r.next(10, TimeUnit.SECONDS);
                            assertEquals(n, v.seq());
                            sum += v.getByte(0);
                        }
                    }
                    return sum;
                }));
            }
            long expected = 0;
            for (int i = 0; i < count; i++) {
                w.append(1, 0, 0, new byte[] {(byte) i, 0, 0, 0});
                expected += (byte) i;
            }
            for (Future<Long> f : sums) {
                assertEquals(expected, f.get(60, TimeUnit.SECONDS));
            }
            pool.shutdown();
        }
    }

    @Test
    void inspectionRunsWhileAWriterAppends() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            Thread t = Thread.ofPlatform().start(() -> {
                for (int i = 0; i < 3000; i++) {
                    w.append(1, 0, 0, new byte[100]);
                    if (i % 500 == 0) {
                        w.checkpoint();
                    }
                }
            });
            int scans = 0;
            while (t.isAlive()) {
                // A scan of a live journal observes it while it changes (docs/cli.md -> verify), so a roll during
                // the scan can show up as a problem; the point here is that it runs alongside the writer.
                Journal.verify(dir);
                Journal.listSegments(dir);
                Journal.readCheckpoint(dir);
                scans++;
            }
            t.join();
            assertTrue(scans > 0);
            assertTrue(Journal.verify(dir).isIntact(), Journal.verify(dir).toString());
        }
    }

    @Test
    void everyConcurrentCloseReturnsAfterTheJournalIsReleased() throws Exception {
        Path dir = tmp.resolve("j");
        // With the default options the writer has a helper thread, so its C close takes a while (it joins it).
        try (Writer w = Writer.open(dir, WriterOptions.defaults().segmentSize(65_536))) {
            w.append(1, 0, 0, new byte[1]);
        }
        Object reopen = new Object();
        for (int iteration = 0; iteration < 100; iteration++) {
            Writer w = Writer.open(dir, WriterOptions.defaults().segmentSize(65_536));
            CountDownLatch start = new CountDownLatch(1);
            AtomicReference<Throwable> failure = new AtomicReference<>();
            List<Thread> closers = new ArrayList<>();
            for (int k = 0; k < 2; k++) {
                closers.add(Thread.ofPlatform().start(() -> {
                    try {
                        start.await();
                        w.close();
                        assertTrue(w.isClosed());
                        // Whichever close returns first or second, the journal is free when it does.
                        synchronized (reopen) {
                            Writer.open(dir, TestSupport.small()).close();
                        }
                    } catch (Throwable t) {
                        failure.compareAndSet(null, t);
                    }
                }));
            }
            start.countDown();
            for (Thread t : closers) {
                t.join();
            }
            if (failure.get() != null) {
                throw new AssertionError("iteration " + iteration, failure.get());
            }
        }
    }
}
