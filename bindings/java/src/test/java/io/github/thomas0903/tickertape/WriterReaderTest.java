package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertNotSame;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** The record and replay surface: every append form, every read form, seek, position, durable-only, rolls. */
class WriterReaderTest {
    @TempDir
    Path tmp;

    @Test
    void libraryIsCompatibleAndDescribesItself() {
        assertEquals(NativeLibrary.VERSION, Tickertape.libraryVersion());
        assertEquals(NativeLibrary.VERSION_NUMBER, Tickertape.libraryVersionNumber());
        assertEquals(NativeLibrary.ABI_VERSION, Tickertape.abiVersion());
        assertEquals(NativeLibrary.VERSION, Tickertape.bindingVersion());
        assertTrue(Tickertape.librarySource().contains("tickertape.library.path"), Tickertape.librarySource());
        long now = System.currentTimeMillis() * 1_000_000L;
        assertTrue(Math.abs(Tickertape.realtimeNs() - now) < 60_000_000_000L);
    }

    @Test
    void everyAppendFormReadsBack() throws Exception {
        Path dir = tmp.resolve("j");
        byte[] hello = "hello".getBytes(StandardCharsets.UTF_8);
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            assertEquals(0, w.nextSeq());
            assertEquals(0, w.append(1, 10, 20, hello));
            assertEquals(1, w.append(2, 11, 21, "xxhelloxx".getBytes(StandardCharsets.UTF_8), 2, 5));
            try (Arena arena = Arena.ofConfined()) {
                assertEquals(2, w.append(3, 12, 22, arena.allocateFrom("hello", StandardCharsets.US_ASCII).asSlice(0, 5)));
            }
            assertEquals(3, w.append(4, 13, 23, MemorySegment.ofArray(hello)));
            ByteBuffer direct = ByteBuffer.allocateDirect(16).put(hello).flip();
            assertEquals(4, w.append(5, 14, 24, direct));
            assertEquals(0, direct.position(), "the buffer's position is unchanged");
            assertEquals(5, w.append(6, 15, 25, ByteBuffer.wrap("..hello".getBytes(StandardCharsets.UTF_8), 2, 5)));
            assertEquals(6, w.appendWith(7, 16, 26, 5, p -> p.put(0, hello)));
            assertEquals(7, w.append(8, 17, 27, new byte[0]));
            assertEquals(8, w.appendWith(9, 18, 28, 0, p -> assertEquals(0, p.length())));
            AppendBatch batch = new AppendBatch(1, 1);
            for (int i = 0; i < 5; i++) {
                batch.add(10 + i, 100 + i, 200 + i, hello);
            }
            assertEquals(9, w.appendBatch(batch));
            assertEquals(5, batch.size());
            assertEquals(14, w.nextSeq());
        }
        try (Reader r = Reader.open(dir)) {
            for (long seq = 0; seq < 14; seq++) {
                RecordView v = r.tryNext();
                assertNotNull(v, "record " + seq);
                assertEquals(seq, v.seq());
                if (seq == 7 || seq == 8) {
                    assertEquals(0, v.payloadLength());
                    assertArrayEquals(new byte[0], v.payloadToArray());
                } else {
                    assertArrayEquals(hello, v.payloadToArray(), "payload of " + seq);
                }
                if (seq < 9) {
                    assertEquals(seq + 1, v.schemaId());
                    assertEquals(10 + seq, v.venueTsNs());
                    assertEquals(20 + seq, v.localTsNs());
                } else {
                    assertEquals(10 + seq - 9, v.schemaId());
                    assertEquals(100 + seq - 9, v.venueTsNs());
                }
            }
            assertNull(r.tryNext());
            assertEquals(14, r.position());
        }
    }

    @Test
    void viewAccessorsAndCopies() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            ByteBuffer b = ByteBuffer.allocate(23).order(ByteOrder.nativeOrder());
            b.put((byte) 7).putShort((short) -2).putInt(123456).putLong(Long.MIN_VALUE).putDouble(2.5);
            w.append(1, 0, 0, b.array());
        }
        try (Reader r = Reader.open(dir)) {
            RecordView v = r.tryNext();
            assertEquals(7, v.getByte(0));
            assertEquals(-2, v.get(ValueLayout.JAVA_SHORT_UNALIGNED, 1));
            assertEquals(123456, v.get(ValueLayout.JAVA_INT_UNALIGNED, 3));
            assertEquals(Long.MIN_VALUE, v.get(ValueLayout.JAVA_LONG_UNALIGNED, 7));
            assertEquals(2.5, v.get(ValueLayout.JAVA_DOUBLE_UNALIGNED, 15));
            assertThrows(IndexOutOfBoundsException.class, () -> v.get(ValueLayout.JAVA_LONG_UNALIGNED, 16));
            assertThrows(IndexOutOfBoundsException.class, () -> v.getByte(-1));
            assertThrows(IndexOutOfBoundsException.class, () -> v.getByte(23));

            byte[] arr = new byte[30];
            v.copyPayloadTo(arr, 5);
            assertEquals(7, arr[5]);
            assertThrows(IndexOutOfBoundsException.class, () -> v.copyPayloadTo(new byte[22], 0));
            ByteBuffer direct = ByteBuffer.allocateDirect(30).position(3);
            v.copyPayloadTo(direct);
            assertEquals(26, direct.position());
            assertEquals(7, direct.get(3));
            assertThrows(java.nio.BufferOverflowException.class, () -> v.copyPayloadTo(ByteBuffer.allocate(5)));
            try (Arena arena = Arena.ofConfined()) {
                MemorySegment seg = arena.allocate(30);
                v.copyPayloadTo(seg, 1);
                assertEquals(123456, seg.get(ValueLayout.JAVA_INT_UNALIGNED, 4));
            }
            long sum = v.withPayload(seg -> {
                assertTrue(seg.isReadOnly());
                assertEquals(23, seg.byteSize());
                return seg.get(ValueLayout.JAVA_INT_UNALIGNED, 3) + 1L;
            });
            assertEquals(123457, sum);
            RecordCopy copy = v.copy();
            assertEquals(0, copy.seq());
            assertEquals(23, copy.payload().length);
            assertEquals(copy, new RecordCopy(0, 1, 0, 0, copy.payload().clone()));
        }
    }

    @Test
    void seekPositionAndTimedNext() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small()); Reader r = Reader.open(dir)) {
            for (int i = 0; i < 100; i++) {
                w.append(1, i, i, new byte[] {(byte) i});
            }
            r.seek(42);
            assertEquals(42, r.position());
            assertEquals(42, r.next(1, TimeUnit.SECONDS).seq());
            assertEquals(43, r.position());
            r.seek(0);
            assertEquals(0, r.next(Duration.ofSeconds(1)).seq());
            r.seek(100);
            long start = System.nanoTime();
            assertNull(r.next(30, TimeUnit.MILLISECONDS));
            long waited = System.nanoTime() - start;
            assertTrue(waited >= 25_000_000L, "waited only " + waited + " ns");
            assertNull(r.next(-5, TimeUnit.SECONDS), "a negative timeout does not wait");
            // Seeking past the end waits for the record.
            r.seek(105);
            for (int i = 100; i < 105; i++) {
                w.append(1, i, i, new byte[] {(byte) i});
            }
            assertNull(r.tryNext());
            w.append(1, 105, 105, new byte[] {1});
            RecordView v = r.next();
            assertEquals(105, v.seq());
        }
    }

    @Test
    void seekToEndSkipsWhatIsCommitted() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small()); Reader r = Reader.open(dir)) {
            for (int i = 0; i < 1000; i++) {
                w.append(1, i, i, new byte[] {(byte) i});
            }
            RecordView first = r.tryNext();
            assertEquals(0, first.seq());
            r.seekToEnd();
            assertFalse(first.isValid());
            assertEquals(1000, r.position());
            assertNull(r.tryNext());
            w.append(1, 0, 0, new byte[] {7});
            assertEquals(1000, r.tryNext().seq());
        }
    }

    @Test
    void followsLiveAcrossThreadsAndRolls() throws Exception {
        Path dir = tmp.resolve("j");
        int count = 3000;
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            List<Long> seen = new ArrayList<>();
            Thread follower = Thread.ofPlatform().start(() -> {
                try (Reader r = Reader.open(dir)) {
                    while (seen.size() < count) {
                        RecordView v = r.next(5, TimeUnit.SECONDS);
                        assertNotNull(v, "timed out after " + seen.size());
                        assertArrayEquals(new byte[(int) (v.seq() % 200)], v.payloadToArray());
                        seen.add(v.seq());
                    }
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                }
            });
            for (int i = 0; i < count; i++) {
                w.append(1, 0, 0, new byte[i % 200]);
            }
            follower.join(30_000);
            assertFalse(follower.isAlive());
            assertEquals(count, seen.size());
            for (int i = 0; i < count; i++) {
                assertEquals(i, seen.get(i));
            }
            assertTrue(Journal.listSegments(dir).size() > 2, "the journal rolled");
        }
    }

    @Test
    void mappedReaderReadsTheSame() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (long seq = 0; seq < 500; seq++) {
                w.append(TestSupport.patternSchema(seq), TestSupport.patternVenue(seq), TestSupport.patternLocal(seq),
                        TestSupport.patternPayload(seq));
            }
        }
        try (Reader r = Reader.openMappedUnsafe(dir)) {
            assertTrue(r.isMapped());
            for (long seq = 0; seq < 500; seq++) {
                TestSupport.checkPattern(r.tryNext(), seq);
            }
            assertNull(r.tryNext());
        }
    }

    @Test
    void durableOnlyReadsStopAtTheCheckpoint() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < 10; i++) {
                w.append(1, 0, 0, new byte[] {(byte) i});
            }
            try (Reader none = Reader.open(dir, ReaderOptions.defaults().durableOnly(true))) {
                assertNull(none.tryNext(), "no checkpoint marker yet");
            }
            w.checkpoint();
            for (int i = 10; i < 15; i++) {
                w.append(1, 0, 0, new byte[] {(byte) i});
            }
            try (Reader durable = Reader.open(dir, ReaderOptions.defaults().durableOnly(true));
                    Reader visible = Reader.open(dir)) {
                int d = 0;
                while (durable.tryNext() != null) {
                    d++;
                }
                assertEquals(10, d);
                int v = 0;
                while (visible.tryNext() != null) {
                    v++;
                }
                assertEquals(15, v);
                w.checkpoint();
                assertEquals(10, durable.next(1, TimeUnit.SECONDS).seq());
            }
        }
    }

    @Test
    void readBatchReturnsRecordsAndHoldsBackWhatDoesNotFit() throws Exception {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            for (int i = 0; i < 20; i++) {
                w.append(i, 0, 0, new byte[i]);
            }
            w.append(99, 0, 0, new byte[1000]);
        }
        try (Reader r = Reader.open(dir)) {
            ReadBatch batch = new ReadBatch(8, 64);
            assertEquals(8, r.readBatch(batch, 1, TimeUnit.SECONDS));
            assertEquals(8, batch.size());
            for (int i = 0; i < 8; i++) {
                RecordView v = batch.get(i);
                assertEquals(i, v.seq());
                assertEquals(i, v.payloadLength());
                assertSame(v, batch.get(i), "one view per index");
            }
            assertNotSame(batch.get(0), batch.get(1), "views of different records do not alias");
            // Payloads 8..13 take 8+9+...+13 = 63 bytes; 14 more do not fit in what is left of 64.
            int n = r.tryReadBatch(batch);
            assertEquals(6, n, "records 8 to 13 fit in 64 bytes; 14 is held back");
            List<Long> seqs = new ArrayList<>();
            batch.forEach(v -> seqs.add(v.seq()));
            assertEquals(List.of(8L, 9L, 10L, 11L, 12L, 13L), seqs);
            while (r.tryReadBatch(batch) > 0 && batch.get(batch.size() - 1).seq() < 19) {
                // drain up to 19
            }
            BufferTooSmallException e = assertThrows(BufferTooSmallException.class, () -> r.tryReadBatch(batch));
            assertEquals(1000, e.requiredBytes());
            assertEquals(20, r.position(), "the record that did not fit stays next");
            assertEquals(0, batch.size());
            ReadBatch big = new ReadBatch(4, 4096);
            assertEquals(1, r.tryReadBatch(big));
            assertEquals(20, big.get(0).seq());
            assertEquals(0, r.tryReadBatch(big));
            assertEquals(0, r.readBatch(big, 20, TimeUnit.MILLISECONDS));
        }
    }

    @Test
    void optionsAreValidatedAndApplied() throws Exception {
        assertThrows(IllegalArgumentException.class, () -> WriterOptions.defaults().segmentSize(0));
        assertThrows(IllegalArgumentException.class, () -> ReaderOptions.defaults().waitSlice(Duration.ZERO));
        assertThrows(IllegalArgumentException.class,
                () -> ReaderOptions.defaults().backoff(1, 1, Duration.ofMillis(2), Duration.ofMillis(1)));
        assertThrows(IllegalArgumentException.class, () -> Retention.delete().maxSegments(-1));
        assertEquals(WriterOptions.defaults().segmentSize(65_536), WriterOptions.defaults().segmentSize(65_536));
        Path dir = tmp.resolve("j");
        WriterOptions options = WriterOptions.defaults().segmentSize(65_536).notify(false).prefault(false)
                .recoveryPolicy(RecoveryPolicy.FAIL);
        try (Writer w = Writer.open(dir, options)) {
            w.append(1, 0, 0, new byte[60_000]);
            w.append(1, 0, 0, new byte[60_000]);
        }
        assertEquals(2, Journal.listSegments(dir).size());
        assertEquals(65_536, Journal.listSegments(dir).get(0).segmentSize());
        ReaderOptions spin = ReaderOptions.defaults().busySpin().notify(false).waitSlice(Duration.ofMillis(1));
        try (Reader r = Reader.open(dir, spin)) {
            assertEquals(0, r.tryNext().seq());
            assertEquals(1, r.next(10, TimeUnit.MILLISECONDS).seq());
            assertNull(r.next(5, TimeUnit.MILLISECONDS));
        }
        ReaderOptions backoff = ReaderOptions.defaults().backoff(0, 0, Duration.ofNanos(50_000), Duration.ofNanos(200_000));
        try (Reader r = Reader.open(dir, backoff)) {
            assertEquals(0, r.next(10, TimeUnit.MILLISECONDS).seq());
        }
    }

    @Test
    void closingTwiceIsHarmless() throws Exception {
        Path dir = tmp.resolve("j");
        Writer w = Writer.open(dir, TestSupport.small());
        w.close();
        w.close();
        assertTrue(w.isClosed());
        Reader r = Reader.open(dir);
        r.close();
        r.close();
        assertTrue(r.isClosed());
        assertTrue(w.toString().contains("closed") && r.toString().contains("closed"));
    }
}
