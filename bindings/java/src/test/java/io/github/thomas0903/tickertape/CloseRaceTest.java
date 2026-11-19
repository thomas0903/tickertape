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
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.io.TempDir;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;

/**
 * Closing a reader from another thread while a consumer reads its current view (ADR 0037 -> Ownership and lifetimes).
 * The view is read outside any call into the library, so the guard cannot wait for it; the reader's shared arena does:
 * closing it ends every access first, so the consumer either completes a pass over the payload with the right bytes or
 * gets {@link IllegalStateException}. Without that, the mapped reader's consumer touches an unmapped page (SIGSEGV, the
 * test JVM dies) and the safe reader's reads freed heap memory (a wrong sum).
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
        Path dir = tmp.resolve(name);
        byte[] payload = new byte[PAYLOAD];
        for (int i = 0; i < PAYLOAD; i++) {
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

    @ParameterizedTest(name = "mapped={0}")
    @ValueSource(booleans = {false, true})
    void closeFromAnotherThreadWhileAViewIsReadNeverReadsFreedMemory(boolean mapped) throws Exception {
        Path dir = journal("j", false);
        // Read right after each close, so that the memory the closed reader freed is likely reused with other bytes.
        Path other = journal("other", true);
        long expected = expectedSum();
        int completedAfterClose = 0;
        for (int iteration = 0; iteration < ITERATIONS; iteration++) {
            Reader reader = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir);
            CountDownLatch ready = new CountDownLatch(1);
            AtomicReference<Throwable> failure = new AtomicReference<>();
            AtomicReference<RecordView> view = new AtomicReference<>();
            long[] passes = new long[1];
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
                            failure.set(new AssertionError("pass " + passes[0] + " read " + sum + " instead of "
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
                        passes[0]++;
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
            // Close at a random point of a pass (a pass takes some tens of microseconds).
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
            if (passes[0] > 0) {
                completedAfterClose++;
            }
            // After close, every access throws, on this thread too, and the reader refuses calls.
            RecordView stale = view.get();
            assertThrows(IllegalStateException.class, () -> stale.getByte(0));
            assertThrows(IllegalStateException.class, () -> stale.copyPayloadTo(new byte[PAYLOAD], 0));
            assertThrows(IllegalStateException.class, reader::tryNext);
        }
        assertTrue(completedAfterClose > 0, "the race was exercised: some consumers completed passes before the close");
    }
}
