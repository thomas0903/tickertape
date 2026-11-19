package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.ref.WeakReference;
import java.nio.file.Path;
import java.util.concurrent.atomic.AtomicReference;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Use after close, views after the next call, payload segments after their scope, and the Cleaner safety net. */
@SuppressWarnings("try") // closes resources inside their try blocks on purpose
class LifetimeTest {
    @TempDir
    Path tmp;

    private Path journal(int records) {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < records; i++) {
                w.append(i, 0, 0, new byte[] {(byte) i, 2, 3});
            }
        }
        return dir;
    }

    @Test
    void useAfterCloseThrowsInsteadOfCrashing() {
        Path dir = journal(3);
        Writer w = Writer.open(dir, TestSupport.small());
        w.close();
        assertThrows(IllegalStateException.class, () -> w.append(1, 0, 0, new byte[1]));
        assertThrows(IllegalStateException.class, () -> w.appendWith(1, 0, 0, 1, p -> {}));
        assertThrows(IllegalStateException.class, () -> w.appendBatch(new AppendBatch().add(1, 0, 0, new byte[1])));
        assertThrows(IllegalStateException.class, w::checkpoint);
        assertThrows(IllegalStateException.class, w::nextSeq);
        assertThrows(IllegalStateException.class, w::lastRetentionError);

        Reader r = Reader.open(dir);
        r.close();
        assertThrows(IllegalStateException.class, r::tryNext);
        assertThrows(IllegalStateException.class, () -> r.next(1, java.util.concurrent.TimeUnit.MILLISECONDS));
        assertThrows(IllegalStateException.class, () -> r.seek(0));
        assertThrows(IllegalStateException.class, r::position);
        assertThrows(IllegalStateException.class, () -> r.tryReadBatch(new ReadBatch(1, 16)));
    }

    @Test
    void aViewIsValidUntilTheNextCall() throws Exception {
        Path dir = journal(3);
        try (Reader r = Reader.open(dir)) {
            RecordView first = r.tryNext();
            assertEquals(0, first.seq());
            assertEquals(1, r.position(), "position does not invalidate the view");
            assertTrue(first.isValid());
            RecordView second = r.tryNext();
            assertSame(first, second, "one reusable view per reader");
            assertEquals(1, second.seq());
            r.seek(0);
            assertFalse(second.isValid());
            assertThrows(IllegalStateException.class, second::seq);
            assertThrows(IllegalStateException.class, () -> second.getByte(0));
            assertThrows(IllegalStateException.class, second::payloadToArray);
            assertThrows(IllegalStateException.class, () -> second.copyPayloadTo(new byte[3], 0));
            assertThrows(IllegalStateException.class, () -> second.withPayload(s -> s));
            assertTrue(second.toString().contains("no longer valid"));
            // A read that returns nothing also invalidates the previous view.
            RecordView v = r.tryNext();
            r.seek(10);
            assertEquals(null, r.tryNext());
            assertThrows(IllegalStateException.class, v::seq);
            // Closing invalidates it.
            r.seek(0);
            RecordView last = r.tryNext();
            r.close();
            assertThrows(IllegalStateException.class, last::seq);
        }
    }

    @Test
    void aPayloadSegmentCannotOutliveItsAction() {
        Path dir = journal(2);
        try (Reader r = Reader.openMappedUnsafe(dir)) {
            RecordView v = r.tryNext();
            AtomicReference<MemorySegment> leaked = new AtomicReference<>();
            byte b = v.withPayload(seg -> {
                leaked.set(seg);
                // The reader refuses calls while the payload is exposed.
                assertThrows(IllegalStateException.class, r::tryNext);
                assertThrows(IllegalStateException.class, r::close);
                return seg.get(ValueLayout.JAVA_BYTE, 0);
            });
            assertEquals(0, b);
            assertThrows(IllegalStateException.class, () -> leaked.get().get(ValueLayout.JAVA_BYTE, 0));
            assertEquals(1, r.tryNext().seq(), "the reader works again after the action");
        }
    }

    @Test
    void batchViewsAreValidUntilTheBatchIsFilledAgain() throws Exception {
        Path dir = journal(5);
        try (Reader r = Reader.open(dir)) {
            ReadBatch batch = new ReadBatch(2, 64);
            assertEquals(2, r.tryReadBatch(batch));
            RecordView v = batch.get(1);
            assertEquals(1, v.seq());
            assertEquals(2, r.tryReadBatch(batch));
            assertThrows(IllegalStateException.class, v::seq, "the batch was refilled");
            assertEquals(3, batch.get(1).seq());
            assertThrows(IndexOutOfBoundsException.class, () -> batch.get(2));
        }
    }

    @Test
    void aBatchCannotBeRefilledWhileOneOfItsViewsIsInsideWithPayload() throws Exception {
        Path dir = journal(5);
        try (Reader r = Reader.open(dir)) {
            ReadBatch batch = new ReadBatch(2, 64);
            ReadBatch other = new ReadBatch(2, 64);
            assertEquals(2, r.tryReadBatch(batch));
            RecordView v = batch.get(0);
            byte[] seen = v.withPayload(segment -> {
                byte before = segment.get(ValueLayout.JAVA_BYTE, 0);
                IllegalStateException e = assertThrows(IllegalStateException.class, () -> r.tryReadBatch(batch));
                assertTrue(e.getMessage().contains("withPayload"), e.getMessage());
                assertThrows(IllegalStateException.class,
                        () -> r.readBatch(batch, 0, java.util.concurrent.TimeUnit.MILLISECONDS));
                assertEquals(2, r.position(), "a refused read consumes nothing");
                assertEquals(2, r.tryReadBatch(other), "another batch can be read meanwhile");
                assertEquals(2, other.get(0).seq());
                return new byte[] {before, segment.get(ValueLayout.JAVA_BYTE, 0)};
            });
            assertArrayEquals(new byte[] {0, 0}, seen, "the payload did not change inside its action");
            assertEquals(0, v.seq(), "the batch was not refilled");
            assertEquals(1, r.tryReadBatch(batch), "after the action the batch can be refilled");
            assertEquals(4, batch.get(0).seq());
        }
    }

    @Test
    void batchViewsOfDifferentRecordsDoNotAlias() throws Exception {
        Path dir = journal(5);
        try (Reader r = Reader.open(dir)) {
            ReadBatch batch = new ReadBatch(4, 64);
            assertEquals(4, r.tryReadBatch(batch));
            RecordView x = batch.get(0);
            RecordView y = batch.get(1);
            assertEquals(0, x.seq(), "an earlier get is not re-pointed by a later one");
            assertEquals(1, y.seq());
            assertEquals(0, x.getByte(0));
            assertEquals(1, y.getByte(0));
            assertSame(x, batch.get(0), "one view object per index, reused");
            assertEquals(1, r.tryReadBatch(batch));
            assertThrows(IllegalStateException.class, x::seq, "a view from the previous read is stale");
            assertThrows(IllegalStateException.class, y::seq);
            assertEquals(4, batch.get(0).seq());
        }
    }

    @Test
    void aBatchViewKeepsItsBatchAndItsMemoryAlive() throws Exception {
        Path dir = tmp.resolve("j");
        byte[] payload = new byte[4096];
        java.util.Arrays.fill(payload, (byte) 90);
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, payload);
        }
        AtomicReference<WeakReference<ReadBatch>> batch = new AtomicReference<>();
        RecordView v;
        try (Reader r = Reader.open(dir)) {
            v = readIntoForgottenBatch(r, batch);
        }
        // Only the view references the batch now. Collect hard, and churn native memory so freed memory is reused.
        for (int i = 0; i < 20; i++) {
            System.gc();
            try (java.lang.foreign.Arena a = java.lang.foreign.Arena.ofConfined()) {
                a.allocate(4096).fill((byte) 17);
            }
            Thread.sleep(5);
        }
        assertNotNull(batch.get().get(), "the batch must stay reachable through its view");
        assertTrue(v.isValid());
        for (int i = 0; i < payload.length; i++) {
            assertEquals(90, v.getByte(i), "byte " + i);
        }
        assertArrayEquals(payload, v.payloadToArray());
    }

    private static RecordView readIntoForgottenBatch(Reader r, AtomicReference<WeakReference<ReadBatch>> ref) {
        ReadBatch batch = new ReadBatch(1, 4096);
        assertEquals(1, r.tryReadBatch(batch));
        ref.set(new WeakReference<>(batch));
        return batch.get(0);
    }

    @Test
    void aReaderViewKeepsAForgottenReaderOpen() throws Exception {
        Path dir = journal(2);
        AtomicReference<WeakReference<Reader>> reader = new AtomicReference<>();
        RecordView v = openReadAndForget(dir, reader);
        for (int i = 0; i < 10; i++) {
            System.gc();
            Thread.sleep(5);
        }
        assertNotNull(reader.get().get(), "the reader must stay reachable (and unfreed) through its view");
        assertEquals(0, v.seq());
        assertEquals(0, v.getByte(0));
        assertEquals(3, v.getByte(2));
        reader.get().get().close();
        assertThrows(IllegalStateException.class, () -> v.getByte(0));
    }

    private static RecordView openReadAndForget(Path dir, AtomicReference<WeakReference<Reader>> ref) {
        Reader r = Reader.openMappedUnsafe(dir);
        ref.set(new WeakReference<>(r));
        return r.tryNext();
    }

    @Test
    void anUnclosedWriterIsReleasedByTheCleaner() throws Exception {
        Path dir = tmp.resolve("j");
        WeakReference<Writer> ref = openAndForget(dir, false);
        awaitReopenable(dir, ref);
    }

    @Test
    void anUnclosedWriterThatUsedAppendWithIsReleasedByTheCleaner() throws Exception {
        Path dir = tmp.resolve("j");
        WeakReference<Writer> ref = openAndForget(dir, true);
        awaitReopenable(dir, ref);
    }

    @Test
    void aClosedWriterThatUsedAppendWithCanBeCollected() throws Exception {
        Path dir = tmp.resolve("j");
        Writer w = Writer.open(dir, TestSupport.small());
        w.appendWith(1, 0, 0, 8, p -> p.setByte(0, (byte) 1));
        w.close();
        WeakReference<Writer> ref = new WeakReference<>(w);
        w = null;
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (ref.get() != null) {
            if (System.nanoTime() > deadline) {
                throw new AssertionError("a closed writer that used appendWith was never collected");
            }
            System.gc();
            Thread.sleep(10);
        }
    }

    private static void awaitReopenable(Path dir, WeakReference<Writer> ref) throws Exception {
        // The journal stays locked until the cleaner closes the forgotten writer.
        long deadline = System.nanoTime() + 20_000_000_000L;
        Writer again = null;
        while (again == null) {
            System.gc();
            try {
                again = Writer.open(dir, TestSupport.small());
            } catch (JournalLockedException e) {
                if (System.nanoTime() > deadline) {
                    throw e;
                }
                Thread.sleep(20);
            }
        }
        try (Writer w = again) {
            assertEquals(null, ref.get());
            assertEquals(1, w.nextSeq());
        }
    }

    private static WeakReference<Writer> openAndForget(Path dir, boolean appendWith) {
        Writer w = Writer.open(dir, TestSupport.small());
        if (appendWith) {
            w.appendWith(1, 0, 0, 1, p -> p.setByte(0, (byte) 1));
        } else {
            w.append(1, 0, 0, new byte[1]);
        }
        assertNotNull(w);
        return new WeakReference<>(w);
    }
}
