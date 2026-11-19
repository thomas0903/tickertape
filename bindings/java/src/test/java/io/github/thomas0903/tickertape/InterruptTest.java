package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.file.Path;
import java.time.Duration;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Blocking reads respond to {@link Thread#interrupt()} and to {@link Reader#close()} from another thread within a
 * bounded time, and never lose a record: an exception from a read means nothing was consumed.
 */
class InterruptTest {
    @TempDir
    Path tmp;

    private Path emptyJournal() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.nextSeq();
        }
        return dir;
    }

    private static long awaitInterruptLatency(Reader r, boolean forever, boolean batch) throws Exception {
        AtomicReference<Throwable> thrown = new AtomicReference<>();
        AtomicReference<Long> caughtAt = new AtomicReference<>();
        CountDownLatch started = new CountDownLatch(1);
        Thread t = Thread.ofPlatform().start(() -> {
            started.countDown();
            try {
                if (batch) {
                    r.readBatch(new ReadBatch(4, 64), 1, TimeUnit.HOURS);
                } else if (forever) {
                    r.next();
                } else {
                    r.next(1, TimeUnit.HOURS);
                }
            } catch (Throwable e) {
                caughtAt.set(System.nanoTime());
                thrown.set(e);
            }
        });
        started.await();
        Thread.sleep(50);
        long interruptedAt = System.nanoTime();
        t.interrupt();
        t.join(5_000);
        assertFalse(t.isAlive(), "the read did not respond to the interrupt");
        assertInstanceOf(InterruptedException.class, thrown.get());
        return caughtAt.get() - interruptedAt;
    }

    @Test
    void anInterruptEndsABlockedReadWithinOneSlice() throws Exception {
        Path dir = emptyJournal();
        ReaderOptions options = ReaderOptions.defaults().waitSlice(Duration.ofMillis(20));
        try (Reader r = Reader.open(dir, options)) {
            for (boolean forever : new boolean[] {false, true}) {
                long latency = awaitInterruptLatency(r, forever, false);
                // One 20 ms slice plus scheduling; generous for loaded CI machines.
                assertTrue(latency < 1_000_000_000L, "took " + latency / 1_000_000 + " ms");
            }
            long latency = awaitInterruptLatency(r, false, true);
            assertTrue(latency < 1_000_000_000L, "batch read took " + latency / 1_000_000 + " ms");
            assertEquals(null, r.tryNext(), "the reader is usable after an interrupted wait");
        }
    }

    @Test
    void anAlreadyInterruptedThreadThrowsWithoutConsuming() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small()); Reader r = Reader.open(dir)) {
            w.append(1, 0, 0, new byte[] {1});
            Thread.currentThread().interrupt();
            assertThrows(InterruptedException.class, () -> r.next(1, TimeUnit.SECONDS));
            assertFalse(Thread.currentThread().isInterrupted(), "InterruptedException clears the status");
            Thread.currentThread().interrupt();
            assertThrows(InterruptedException.class, () -> r.readBatch(new ReadBatch(1, 8), 1, TimeUnit.SECONDS));
            Thread.currentThread().interrupt();
            assertNotNull(r.tryNext(), "tryNext never waits, so it ignores the interrupt status");
            assertTrue(Thread.interrupted());
            assertEquals(1, r.position());
        }
        try (Reader r = Reader.open(dir)) {
            Thread.currentThread().interrupt();
            assertThrows(InterruptedException.class, r::next);
            assertEquals(0, r.next(1, TimeUnit.SECONDS).seq(), "the interrupted read consumed nothing");
        }
    }

    /**
     * A writer appends while a follower is interrupted again and again during its waits. Every record must reach the
     * follower exactly once and in order: an interrupted read consumed nothing, and a read that got a record returned
     * it even when the interrupt arrived during that slice.
     */
    @Test
    void interruptsNeverLoseOrDuplicateRecords() throws Exception {
        Path dir = emptyJournal();
        int count = 20_000;
        AtomicInteger interrupts = new AtomicInteger();
        AtomicInteger received = new AtomicInteger();
        CompletableFuture<Void> done = new CompletableFuture<>();
        ReaderOptions options = ReaderOptions.defaults().waitSlice(Duration.ofMillis(1));
        Thread follower = Thread.ofPlatform().start(() -> {
            try (Reader r = Reader.open(dir, options)) {
                long expected = 0;
                while (expected < count) {
                    try {
                        RecordView v = r.next(10, TimeUnit.SECONDS);
                        if (v == null) {
                            throw new AssertionError("timed out at " + expected);
                        }
                        assertEquals(expected, v.seq(), "lost or duplicated a record");
                        assertEquals((byte) expected, v.getByte(0));
                        expected++;
                        received.incrementAndGet();
                    } catch (InterruptedException e) {
                        interrupts.incrementAndGet();
                    }
                }
                Thread.interrupted();
                done.complete(null);
            } catch (Throwable e) {
                done.completeExceptionally(e);
            }
        });
        Thread interrupter = Thread.ofPlatform().start(() -> {
            while (!done.isDone()) {
                follower.interrupt();
                java.util.concurrent.locks.LockSupport.parkNanos(200_000);
            }
        });
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < count; i++) {
                w.append(1, 0, 0, new byte[] {(byte) i});
                if (i % 50 == 0) {
                    java.util.concurrent.locks.LockSupport.parkNanos(100_000);
                }
            }
        }
        done.get(60, TimeUnit.SECONDS);
        interrupter.join();
        assertEquals(count, received.get());
        assertTrue(interrupts.get() > 10, "the test must actually interrupt waits: " + interrupts.get());
    }

    @Test
    void closeFromAnotherThreadEndsABlockedRead() throws Exception {
        Path dir = emptyJournal();
        Reader r = Reader.open(dir, ReaderOptions.defaults().waitSlice(Duration.ofMillis(20)));
        AtomicReference<Throwable> thrown = new AtomicReference<>();
        CountDownLatch started = new CountDownLatch(1);
        Thread t = Thread.ofPlatform().start(() -> {
            started.countDown();
            try {
                r.next();
            } catch (Throwable e) {
                thrown.set(e);
            }
        });
        started.await();
        Thread.sleep(50);
        long start = System.nanoTime();
        r.close();
        long took = System.nanoTime() - start;
        t.join(5_000);
        assertFalse(t.isAlive());
        assertInstanceOf(IllegalStateException.class, thrown.get());
        assertTrue(thrown.get().getMessage().contains("closed"), thrown.get().getMessage());
        assertTrue(r.isClosed());
        assertTrue(took < 1_000_000_000L, "close waited " + took / 1_000_000 + " ms");
    }

    @Test
    void virtualThreadsCanReadAndBeInterrupted() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[] {5});
            AtomicReference<Object> result = new AtomicReference<>();
            Thread vt = Thread.ofVirtual().start(() -> {
                try (Reader r = Reader.open(dir, ReaderOptions.defaults().waitSlice(Duration.ofMillis(5)))) {
                    result.set(r.next(1, TimeUnit.SECONDS).getByte(0));
                    r.next(1, TimeUnit.HOURS);
                    result.set("no interrupt");
                } catch (InterruptedException e) {
                    result.set(e);
                }
            });
            long deadline = System.nanoTime() + 5_000_000_000L;
            while (!(result.get() instanceof Byte) && System.nanoTime() < deadline) {
                Thread.sleep(5);
            }
            assertEquals((byte) 5, result.get());
            Thread.sleep(50); // now waiting for a second record
            vt.interrupt();
            vt.join(5_000);
            assertFalse(vt.isAlive());
            assertInstanceOf(InterruptedException.class, result.get());
        }
    }
}
