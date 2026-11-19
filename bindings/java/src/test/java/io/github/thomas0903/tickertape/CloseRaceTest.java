package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.io.TempDir;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.CsvSource;
import org.junit.jupiter.params.provider.ValueSource;

/**
 * Closing a reader from another thread while a consumer reads its current view (ADR 0037 -> Ownership and lifetimes).
 * The view is read outside any call into the library, so the guard cannot wait for it; the reader's shared arena does:
 * closing it ends every access first, so the consumer either completes a pass over the payload with the right bytes or
 * gets {@link IllegalStateException}. Without that, the mapped reader's consumer touches an unmapped page (SIGSEGV, the
 * test JVM dies) and the safe reader's reads freed heap memory (a wrong sum).
 *
 * <p>The pass is compiled before the timed iterations (a cold pass takes milliseconds, longer than the random delay
 * before the close), and every other iteration closes only after the consumer has completed a pass, so the test's own
 * precondition (the race was exercised) holds on a loaded machine too.
 */
class CloseRaceTest {
    private static final int ITERATIONS = Integer.getInteger("tickertape.test.raceIterations", 200);
    private static final int PAYLOAD = 16 * 1024;

    @TempDir
    Path tmp;

    private static byte patternByte(int i) {
        return (byte) (i * 31 + 7);
    }

    private Path journal(String name, boolean inverted) {
        return journal(name, inverted, PAYLOAD);
    }

    private Path journal(String name, boolean inverted, int length) {
        Path dir = tmp.resolve(name);
        byte[] payload = new byte[length];
        for (int i = 0; i < length; i++) {
            payload[i] = (byte) (inverted ? ~patternByte(i) : patternByte(i));
        }
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, payload);
        }
        return dir;
    }

    private static long expectedSum() {
        long sum = 0;
        for (int i = 0; i < PAYLOAD; i++) {
            sum += patternByte(i);
        }
        return sum;
    }

    /** One pass over the payload through every kind of accessor; returns the byte sum. */
    private static long pass(RecordView v, byte[] array, ByteBuffer direct, MemorySegment segment) {
        long sum = 0;
        for (int i = 0; i < PAYLOAD; i++) {
            sum += v.getByte(i);
        }
        long longs = 0;
        for (int i = 0; i + 8 <= PAYLOAD; i += 8) {
            longs ^= v.get(ValueLayout.JAVA_LONG_UNALIGNED, i);
        }
        v.copyPayloadTo(array, 0);
        direct.clear();
        v.copyPayloadTo(direct);
        v.copyPayloadTo(segment, 0);
        sink ^= longs;
        return sum;
    }

    private static volatile long sink;

    /** Compiles {@link #pass} before the timed iterations, so that a pass takes microseconds, not milliseconds. */
    private static void warmUp(Path dir, boolean mapped) {
        byte[] array = new byte[PAYLOAD];
        ByteBuffer direct = ByteBuffer.allocateDirect(PAYLOAD);
        try (Reader reader = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir); Arena arena = Arena.ofConfined()) {
            MemorySegment segment = arena.allocate(PAYLOAD);
            RecordView v = reader.tryNext();
            long expected = expectedSum();
            for (int i = 0; i < 300; i++) {
                if (pass(v, array, direct, segment) != expected) {
                    fail("warm-up pass read the wrong bytes");
                }
            }
        }
    }

    @ParameterizedTest(name = "mapped={0}")
    @ValueSource(booleans = {false, true})
    void closeFromAnotherThreadWhileAViewIsReadNeverReadsFreedMemory(boolean mapped) throws Exception {
        Path dir = journal("j", false);
        // Read right after each close, so that the memory the closed reader freed is likely reused with other bytes.
        Path other = journal("other", true);
        long expected = expectedSum();
        warmUp(dir, mapped);
        int completedBeforeClose = 0;
        for (int iteration = 0; iteration < ITERATIONS; iteration++) {
            Reader reader = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir);
            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> failure = new AtomicReference<>();
            AtomicReference<RecordView> view = new AtomicReference<>();
            AtomicLong passes = new AtomicLong();
            Thread consumer = Thread.ofPlatform().start(() -> {
                byte[] array = new byte[PAYLOAD];
                ByteBuffer direct = ByteBuffer.allocateDirect(PAYLOAD);
                try (Arena arena = Arena.ofConfined()) {
                    MemorySegment segment = arena.allocate(PAYLOAD);
                    RecordView v = reader.tryNext();
                    assertNotNull(v);
                    view.set(v);
                    ready.countDown();
                    for (;;) {
                        long sum = pass(v, array, direct, segment);
                        if (sum != expected) {
                            failure.set(new AssertionError("pass " + passes.get() + " read " + sum + " instead of "
                                    + expected + ": the view read freed memory"));
                            return;
                        }
                        for (int i = 0; i < PAYLOAD; i += 997) {
                            if (array[i] != patternByte(i) || direct.get(i) != patternByte(i)
                                    || segment.get(ValueLayout.JAVA_BYTE, i) != patternByte(i)) {
                                failure.set(new AssertionError("a copy read freed memory at " + i));
                                return;
                            }
                        }
                        passes.incrementAndGet();
                    }
                } catch (IllegalStateException expectedAfterClose) {
                    // the reader was closed: the only acceptable way for the loop to end
                } catch (Throwable t) {
                    failure.set(t);
                } finally {
                    ready.countDown();
                }
            });
            assertTrue(ready.await(10, TimeUnit.SECONDS));
            if (iteration % 2 == 0) {
                // Every other iteration: close only once a pass has completed, so the race is exercised however slow
                // the machine; the others close at any point, also during the first pass.
                long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
                while (passes.get() == 0 && consumer.isAlive() && System.nanoTime() < deadline) {
                    Thread.onSpinWait();
                }
            }
            // Close at a random point of a pass (a compiled pass takes some tens of microseconds).
            long spin = ThreadLocalRandom.current().nextLong(200_000);
            long until = System.nanoTime() + spin;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
            reader.close();
            Reader reuse = mapped ? Reader.openMappedUnsafe(other) : Reader.open(other);
            reuse.tryNext();
            consumer.join(10_000);
            reuse.close();
            if (consumer.isAlive()) {
                fail("the consumer did not notice the close");
            }
            if (failure.get() != null) {
                throw new AssertionError("iteration " + iteration, failure.get());
            }
            if (passes.get() > 0) {
                completedBeforeClose++;
            }
            // After close, every access throws, on this thread too, and the reader refuses calls.
            RecordView stale = view.get();
            assertThrows(IllegalStateException.class, () -> stale.getByte(0));
            assertThrows(IllegalStateException.class, () -> stale.copyPayloadTo(new byte[PAYLOAD], 0));
            assertThrows(IllegalStateException.class, reader::tryNext);
        }
        assertTrue(completedBeforeClose >= ITERATIONS / 2,
                "the race was exercised: consumers completed passes before the close in " + completedBeforeClose
                        + " of " + ITERATIONS + " iterations");
    }

    /** What the consumer of {@link #metadataAndEmptyPayloadsSeeACloseFromAnotherThread} reads in its loop. */
    enum Reads {
        /** The metadata accessors only (a record with a payload, which this loop never touches). */
        METADATA,
        /** Copies of an empty payload, which touch no memory: into an array. */
        EMPTY_TO_ARRAY,
        /** Into a segment. */
        EMPTY_TO_SEGMENT,
        /** Into a direct buffer. */
        EMPTY_TO_BUFFER,
        /** {@link RecordView#isValid()} only. */
        IS_VALID
    }

    /**
     * Accessors that touch no payload memory see a close from another thread too: the arena's handshake cannot stop
     * them, only the view's generation can. The consumer spins in a loop that reads nothing else, which the JIT compiles
     * (OSR) during the first wait; with a generation read as a plain field, the compiled loop keeps returning values
     * long after {@code close()} returned (it was seen spinning for more than a second). Each iteration therefore gives
     * the consumer two seconds to notice; it needs microseconds.
     */
    @ParameterizedTest(name = "mapped={0}, reads={1}")
    @CsvSource({"false, METADATA", "true, METADATA", "false, EMPTY_TO_ARRAY", "true, EMPTY_TO_ARRAY",
            "false, EMPTY_TO_SEGMENT", "true, EMPTY_TO_SEGMENT", "false, EMPTY_TO_BUFFER", "true, EMPTY_TO_BUFFER",
            "false, IS_VALID", "true, IS_VALID"})
    void metadataAndEmptyPayloadsSeeACloseFromAnotherThread(boolean mapped, Reads reads) throws Exception {
        Path dir = journal("j", false, reads.name().startsWith("EMPTY") ? 0 : 16);
        int iterations = Math.max(5, ITERATIONS / 20);
        for (int iteration = 0; iteration < iterations; iteration++) {
            Reader reader = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir);
            RecordView v = reader.tryNext();
            assertNotNull(v);
            AtomicReference<Throwable> failure = new AtomicReference<>();
            AtomicLong loops = new AtomicLong();
            // A daemon: if it never sees the close, it must not keep the test JVM alive.
            Thread consumer = Thread.ofPlatform().daemon(true).start(() -> {
                long n = 0;
                long sum = 0;
                byte[] array = new byte[1];
                ByteBuffer direct = ByteBuffer.allocateDirect(1);
                MemorySegment segment = MemorySegment.ofArray(new byte[1]);
                try {
                    switch (reads) {
                        case METADATA -> {
                            for (;; n++) {
                                sum += v.seq() + v.schemaId() + v.venueTsNs() + v.localTsNs() + v.payloadLength();
                            }
                        }
                        case EMPTY_TO_ARRAY -> {
                            for (;; n++) {
                                v.copyPayloadTo(array, 0);
                            }
                        }
                        case EMPTY_TO_SEGMENT -> {
                            for (;; n++) {
                                v.copyPayloadTo(segment, 0);
                            }
                        }
                        case EMPTY_TO_BUFFER -> {
                            for (;; n++) {
                                v.copyPayloadTo(direct);
                            }
                        }
                        case IS_VALID -> {
                            while (v.isValid()) {
                                n++;
                            }
                        }
                    }
                } catch (IllegalStateException expected) {
                    // the close was seen
                } catch (Throwable t) {
                    failure.set(t);
                } finally {
                    loops.set(n);
                    sink ^= sum;
                }
            });
            Thread.sleep(iteration == 0 ? 300 : 20); // the first wait lets the JIT compile the loop
            reader.close();
            long closed = System.nanoTime();
            consumer.join(2_000);
            if (consumer.isAlive()) {
                fail("iteration " + iteration + ": the consumer still reads the view "
                        + TimeUnit.NANOSECONDS.toMillis(System.nanoTime() - closed) + " ms after close() returned");
            }
            if (failure.get() != null) {
                throw new AssertionError("iteration " + iteration, failure.get());
            }
            assertTrue(loops.get() > 0, "the consumer read the view before the close");
            assertTrue(!v.isValid());
            assertThrows(IllegalStateException.class, v::seq);
            assertThrows(IllegalStateException.class, v::payloadLength);
            assertThrows(IllegalStateException.class, () -> v.copyPayloadTo(new byte[1], 0));
        }
    }
}
