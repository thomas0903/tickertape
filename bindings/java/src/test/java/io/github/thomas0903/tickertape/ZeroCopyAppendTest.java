package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertInstanceOf;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** {@link Writer#appendWith}: the upcall, aborts, re-entrancy and the payload buffer's lifetime. */
@SuppressWarnings("try") // closes the writer inside its try block on purpose
class ZeroCopyAppendTest {
    @TempDir
    Path tmp;

    @Test
    void fillWritesInPlaceWithEveryAccessor() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            long seq = w.appendWith(1, 2, 3, 40, p -> {
                assertEquals(40, p.length());
                MemorySegment before = p.segment();
                for (long i = 0; i < 40; i++) {
                    assertEquals(0, before.get(ValueLayout.JAVA_BYTE, i), "the buffer starts zeroed at " + i);
                }
                p.setByte(0, (byte) 1);
                p.set(ValueLayout.JAVA_SHORT_UNALIGNED, 1, (short) 2);
                p.set(ValueLayout.JAVA_INT_UNALIGNED, 3, 3);
                p.set(ValueLayout.JAVA_LONG_UNALIGNED, 7, 4L);
                p.set(ValueLayout.JAVA_DOUBLE_UNALIGNED, 15, 5.5);
                p.put(23, new byte[] {9, 9, 9});
                p.put(26, ByteBuffer.wrap(new byte[] {8, 8}));
                p.put(28, MemorySegment.ofArray(new byte[] {7}));
                MemorySegment seg = p.segment();
                assertEquals(40, seg.byteSize());
                seg.set(ValueLayout.JAVA_BYTE, 39, (byte) 6);
                assertThrows(IndexOutOfBoundsException.class, () -> p.setByte(40, (byte) 0));
                assertThrows(IndexOutOfBoundsException.class, () -> p.put(38, new byte[3]));
            });
            assertEquals(0, seq);
        }
        try (Reader r = Reader.open(dir)) {
            RecordView v = r.tryNext();
            assertEquals(1, v.getByte(0));
            assertEquals(2, v.get(ValueLayout.JAVA_SHORT_UNALIGNED, 1));
            assertEquals(3, v.get(ValueLayout.JAVA_INT_UNALIGNED, 3));
            assertEquals(4L, v.get(ValueLayout.JAVA_LONG_UNALIGNED, 7));
            assertEquals(5.5, v.get(ValueLayout.JAVA_DOUBLE_UNALIGNED, 15));
            assertEquals(9, v.getByte(25));
            assertEquals(8, v.getByte(27));
            assertEquals(7, v.getByte(28));
            assertEquals(0, v.getByte(29));
            assertEquals(6, v.getByte(39));
        }
    }

    @Test
    void aThrowingFillerAbortsAndAppendsNothing() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[] {1});
            IOException boom = new IOException("boom");
            FillAbortedException e = assertThrows(FillAbortedException.class, () -> w.appendWith(1, 0, 0, 10, p -> {
                p.setByte(0, (byte) 42);
                throw boom;
            }));
            assertSame(boom, e.getCause());
            assertEquals(Status.ABORTED, e.status());
            // An Error from the filler is rethrown as it is, after the abort.
            StackOverflowError so = new StackOverflowError("deep");
            assertSame(so, assertThrows(StackOverflowError.class, () -> w.appendWith(1, 0, 0, 10, p -> {
                throw so;
            })));
            assertEquals(1, w.nextSeq(), "nothing was appended");
            assertEquals(1, w.append(1, 0, 0, new byte[] {2}), "the writer still works");
        }
        try (Reader r = Reader.open(dir)) {
            assertEquals(1, r.tryNext().getByte(0));
            assertEquals(2, r.tryNext().getByte(0));
            assertNull(r.tryNext());
        }
    }

    @Test
    void theBufferIsInvalidAfterTheFillerReturns() {
        Path dir = tmp.resolve("j");
        AtomicReference<PayloadBuffer> kept = new AtomicReference<>();
        AtomicReference<MemorySegment> keptSegment = new AtomicReference<>();
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.appendWith(1, 0, 0, 8, p -> {
                kept.set(p);
                keptSegment.set(p.segment());
            });
            assertThrows(IllegalStateException.class, () -> kept.get().setByte(0, (byte) 1));
            assertThrows(IllegalStateException.class, () -> kept.get().length());
            assertThrows(IllegalStateException.class, () -> keptSegment.get().get(ValueLayout.JAVA_BYTE, 0),
                    "the segment's scope closed when the filler returned");
            assertTrue(kept.get().toString().contains("no longer valid"));
            // The same buffer object serves the next append.
            w.appendWith(1, 0, 0, 8, p -> assertSame(kept.get(), p));
        }
    }

    @Test
    void theBufferRefusesEveryOtherThread() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            // The reviewer's scenario: a filler hands the buffer to a helper thread and waits for it. segment() used
            // to create a confined arena on the helper, which the upcall then failed to close: the JVM exited.
            AtomicReference<Throwable> helperSaw = new AtomicReference<>();
            FillAbortedException e = assertThrows(FillAbortedException.class, () -> w.appendWith(1, 0, 0, 16, p -> {
                Thread helper = Thread.ofPlatform().start(() -> {
                    try {
                        p.segment().set(ValueLayout.JAVA_BYTE, 0, (byte) 1);
                    } catch (Throwable t) {
                        helperSaw.set(t);
                    }
                });
                helper.join();
                throw new Exception("the helper failed", helperSaw.get());
            }));
            assertInstanceOf(WrongThreadException.class, helperSaw.get());
            assertSame(helperSaw.get(), e.getCause().getCause());
            assertEquals(0, w.nextSeq(), "the append was aborted");

            // Every accessor refuses another thread, also when the filler returns normally after the helper.
            AtomicReference<Throwable> first = new AtomicReference<>();
            int[] refused = new int[1];
            long seq = w.appendWith(1, 0, 0, 16, p -> {
                Thread helper = Thread.ofPlatform().start(() -> {
                    Runnable[] uses = {
                        p::length,
                        () -> p.setByte(0, (byte) 1),
                        () -> p.set(ValueLayout.JAVA_LONG_UNALIGNED, 0, 1L),
                        () -> p.put(0, new byte[] {1}),
                        () -> p.put(0, ByteBuffer.allocateDirect(1)),
                        () -> p.put(0, MemorySegment.ofArray(new byte[1])),
                        p::segment,
                    };
                    for (Runnable use : uses) {
                        try {
                            use.run();
                        } catch (WrongThreadException expected) {
                            refused[0]++;
                        } catch (Throwable t) {
                            first.compareAndSet(null, t);
                        }
                    }
                });
                helper.join();
                p.setByte(15, (byte) 7);
            });
            assertEquals(null, first.get());
            assertEquals(7, refused[0], "every accessor threw WrongThreadException on the helper");
            assertEquals(0, seq, "the writer is still usable");
            assertEquals(1, w.append(1, 0, 0, new byte[] {2}));
        }
        try (Reader r = Reader.open(dir)) {
            RecordView v = r.tryNext();
            assertEquals(0, v.getByte(0), "nothing the helper tried was written");
            assertEquals(7, v.getByte(15));
            assertEquals(2, r.tryNext().getByte(0));
        }
    }

    @Test
    void callingTheWriterFromItsFillerIsRefused() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            FillAbortedException e = assertThrows(FillAbortedException.class,
                    () -> w.appendWith(1, 0, 0, 4, p -> w.append(1, 0, 0, new byte[1])));
            assertInstanceOf(IllegalStateException.class, e.getCause());
            assertTrue(e.getCause().getMessage().contains("inside a call on this thread"), e.getCause().getMessage());
            FillAbortedException closing = assertThrows(FillAbortedException.class,
                    () -> w.appendWith(1, 0, 0, 4, p -> w.close()));
            assertInstanceOf(IllegalStateException.class, closing.getCause());
            assertEquals(0, w.nextSeq());
            assertTrue(!w.isClosed());
        }
    }

    @Test
    void largeAndManyFills() {
        Path dir = tmp.resolve("j");
        byte[] big = new byte[60_000];
        for (int i = 0; i < big.length; i++) {
            big[i] = (byte) i;
        }
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < 200; i++) {
                int n = i % 3 == 0 ? big.length : i;
                w.appendWith(i, 0, 0, n, p -> p.put(0, big, 0, n));
            }
        }
        try (Reader r = Reader.open(dir)) {
            for (int i = 0; i < 200; i++) {
                RecordView v = r.tryNext();
                int n = i % 3 == 0 ? big.length : i;
                assertEquals(n, v.payloadLength());
                if (n > 0) {
                    assertEquals(big[n - 1], v.getByte(n - 1));
                }
            }
        }
    }
}
