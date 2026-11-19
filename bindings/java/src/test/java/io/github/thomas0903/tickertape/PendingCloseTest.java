package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * A close that waits for a call in progress on another thread (ADR 0037 -&gt; Threads): from the moment it starts,
 * the handle refuses new calls and reports itself closed; the call in progress hands the handle to the closer when it
 * leaves, with no moment at which another call could enter; and the closer waits without spinning, also when its
 * thread is interrupted, whose interrupt status it keeps. A call is held in progress deterministically with
 * {@link RecordView#withPayload}, which keeps the reader's guard until its action returns.
 */
class PendingCloseTest {
    @TempDir
    Path tmp;

    private static volatile byte[] garbage;

    private Path journal(int records) {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < records; i++) {
                w.append(1, i, i, new byte[] {(byte) i});
            }
        }
        return dir;
    }

    private static void await(CountDownLatch latch) {
        try {
            assertTrue(latch.await(10, TimeUnit.SECONDS));
        } catch (InterruptedException e) {
            throw new AssertionError(e);
        }
    }

    @Test
    void aPendingCloseRefusesNewCallsAndTakesOverWhenTheCallLeaves() throws Exception {
        Reader reader = Reader.open(journal(10));
        CountDownLatch inside = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        AtomicReference<String> afterAction = new AtomicReference<>();
        AtomicReference<Throwable> failure = new AtomicReference<>();
        Thread caller = Thread.ofPlatform().start(() -> {
            try {
                RecordView v = reader.tryNext();
                v.withPayload(segment -> {
                    inside.countDown();
                    await(release);
                    return null;
                });
                // The close is waiting: the guard goes straight to the closer, so this call must not get in.
                try {
                    RecordView next = reader.tryNext();
                    afterAction.set("a new call entered after the action, while the close waited: "
                            + (next == null ? "null" : "seq " + next.seq()));
                } catch (IllegalStateException e) {
                    afterAction.set(e.getMessage());
                }
            } catch (Throwable t) {
                failure.set(t);
            }
        });
        await(inside);
        assertTrue(!reader.isClosed());
        Thread closer = Thread.ofPlatform().start(reader::close);
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        while (!reader.isClosed()) {
            if (System.nanoTime() > deadline) {
                fail("isClosed() stayed false while close() waited for the call in progress");
            }
            Thread.onSpinWait();
        }
        assertTrue(closer.isAlive(), "close waits for the call in progress");
        IllegalStateException refused = assertThrows(IllegalStateException.class, reader::tryNext);
        assertTrue(refused.getMessage().contains("closed"), refused.getMessage());
        release.countDown();
        caller.join(10_000);
        closer.join(10_000);
        assertTrue(!caller.isAlive() && !closer.isAlive());
        if (failure.get() != null) {
            throw new AssertionError(failure.get());
        }
        assertTrue(afterAction.get().contains("closed"), afterAction.get());
    }

    @Test
    void aCallLoopCannotStarveTheClose() throws Exception {
        Reader reader = Reader.open(journal(100));
        AtomicLong calls = new AtomicLong();
        AtomicReference<Throwable> failure = new AtomicReference<>();
        AtomicBoolean endedByClose = new AtomicBoolean();
        Thread caller = Thread.ofPlatform().start(() -> {
            long n = 0;
            try {
                for (;; n++) {
                    if (reader.tryNext() == null) {
                        reader.seek(0);
                    }
                }
            } catch (IllegalStateException e) {
                endedByClose.set(e.getMessage().contains("closed"));
            } catch (Throwable t) {
                failure.set(t);
            } finally {
                calls.set(n);
            }
        });
        Thread.sleep(200); // let the loop get hot
        long start = System.nanoTime();
        reader.close();
        long ms = TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - start);
        caller.join(10_000);
        assertTrue(!caller.isAlive());
        if (failure.get() != null) {
            throw new AssertionError(failure.get());
        }
        assertTrue(endedByClose.get());
        assertTrue(calls.get() > 0);
        assertTrue(ms < 2_000, "close() took " + ms + " ms against a thread calling in a loop");
    }

    @Test
    void anInterruptedCloserWaitsWithoutSpinningAndStaysInterrupted() throws Exception {
        ThreadMXBean threads = ManagementFactory.getThreadMXBean();
        assertTrue(threads.isCurrentThreadCpuTimeSupported());
        Reader reader = Reader.open(journal(1));
        CountDownLatch inside = new CountDownLatch(1);
        long holdMs = 400;
        Thread caller = Thread.ofPlatform().start(() -> {
            RecordView v = reader.tryNext();
            v.withPayload(segment -> {
                inside.countDown();
                try {
                    Thread.sleep(holdMs);
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                }
                return null;
            });
        });
        await(inside);
        AtomicLong cpuNs = new AtomicLong();
        AtomicLong wallNs = new AtomicLong();
        AtomicBoolean stillInterrupted = new AtomicBoolean();
        Thread closer = Thread.ofPlatform().start(() -> {
            Thread.currentThread().interrupt();
            long cpu = threads.getCurrentThreadCpuTime();
            long wall = System.nanoTime();
            reader.close();
            wallNs.set(System.nanoTime() - wall);
            cpuNs.set(threads.getCurrentThreadCpuTime() - cpu);
            stillInterrupted.set(Thread.currentThread().isInterrupted());
        });
        closer.join(10_000);
        caller.join(10_000);
        assertTrue(!closer.isAlive() && !caller.isAlive());
        assertTrue(stillInterrupted.get(), "close keeps the closing thread's interrupt status");
        assertTrue(wallNs.get() >= TimeUnit.MILLISECONDS.toNanos(holdMs / 2), "close waited for the call");
        assertTrue(cpuNs.get() < wallNs.get() / 4, "the interrupted closer used "
                + TimeUnit.NANOSECONDS.toMillis(cpuNs.get()) + " ms of CPU in " + TimeUnit.NANOSECONDS.toMillis(wallNs.get())
                + " ms of waiting");
        assertTrue(reader.isClosed());
    }

    @Test
    void aSecondCloserWaitsForTheFirst() throws Exception {
        Reader reader = Reader.open(journal(1));
        CountDownLatch inside = new CountDownLatch(1);
        CountDownLatch release = new CountDownLatch(1);
        Thread caller = Thread.ofPlatform().start(() -> reader.tryNext().withPayload(segment -> {
            inside.countDown();
            await(release);
            return null;
        }));
        await(inside);
        Thread first = Thread.ofPlatform().start(reader::close);
        while (!reader.isClosed()) {
            Thread.onSpinWait();
        }
        Thread second = Thread.ofPlatform().start(reader::close);
        second.join(200);
        assertTrue(second.isAlive(), "the second close waits until the reader is freed");
        release.countDown();
        for (Thread t : new Thread[] {caller, first, second}) {
            t.join(10_000);
            assertTrue(!t.isAlive());
        }
    }

    /**
     * {@code close()} keeps its object reachable until it returns, so the Cleaner can never claim the clean-up while
     * an explicit close runs (its {@code clean()} would then return before the handle is freed, and an immediate
     * reopen would meet the old lock). Whether the JIT would otherwise let the object go during the window cannot be
     * forced from a test: this stresses it, with a collector running constantly, and checks what must hold every
     * time (the journal can be opened again at once). It cannot fail deterministically without the fence.
     */
    @Test
    void closeReleasesTheJournalBeforeItReturnsUnderCollectorPressure() throws Exception {
        Path dir = tmp.resolve("w");
        AtomicBoolean stop = new AtomicBoolean();
        // Allocation keeps young collections (which find unreachable writers and readers) running all the time;
        // System.gc() in a loop would make every close's handshake wait for full collections instead.
        Thread gc = Thread.ofPlatform().daemon(true).start(() -> {
            while (!stop.get()) {
                garbage = new byte[16 * 1024];
            }
        });
        try {
            for (int i = 0; i < 1000; i++) {
                Writer.open(dir, TestSupport.small()).close();
                Reader reader = Reader.open(dir);
                reader.close();
            }
            try (Writer w = Writer.open(dir, TestSupport.small())) {
                assertNotNull(w);
                assertEquals(0, w.nextSeq());
            }
        } finally {
            stop.set(true);
            gc.join();
        }
    }
}
