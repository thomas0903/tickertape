package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicReference;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Public-API details settled before 1.0 (ADR 0037 &rarr; API conventions), and small contracts around them. */
class ApiDetailsTest {
    @TempDir
    Path tmp;

    /** Every public class of the binding, read from the compiled classes. */
    private static List<Class<?>> publicClasses() throws IOException, ClassNotFoundException {
        Path dir = TestSupport.CLASSES.resolve("io/github/thomas0903/tickertape");
        List<Class<?>> out = new ArrayList<>();
        try (Stream<Path> files = Files.list(dir)) {
            for (Path f : files.toList()) {
                String name = f.getFileName().toString();
                if (name.endsWith(".class") && !name.contains("$")) {
                    Class<?> c = Class.forName("io.github.thomas0903.tickertape." + name.replace(".class", ""));
                    if (Modifier.isPublic(c.getModifiers())) {
                        out.add(c);
                    }
                }
            }
        }
        assertTrue(out.size() > 30, "found only " + out);
        return out;
    }

    @Test
    void everyPayloadBufferWriteReturnsTheBufferForChaining() {
        List<String> wrong = new ArrayList<>();
        for (Method m : PayloadBuffer.class.getDeclaredMethods()) {
            if (Modifier.isPublic(m.getModifiers()) && (m.getName().startsWith("set") || m.getName().startsWith("put"))
                    && m.getReturnType() != PayloadBuffer.class) {
                wrong.add(m.toString());
            }
        }
        assertEquals(List.of(), wrong, "writes that do not return the buffer");
        try (Writer w = Writer.open(tmp.resolve("j"), TestSupport.small())) {
            w.appendWith(1, 0, 0, 16, p -> p.setByte(0, (byte) 1).set(ValueLayout.JAVA_INT_UNALIGNED, 4, 7)
                    .put(8, new byte[] {9}).set(ValueLayout.JAVA_SHORT_UNALIGNED, 10, (short) 3));
        }
        try (Reader r = Reader.open(tmp.resolve("j"))) {
            RecordView v = r.tryNext();
            assertEquals(1, v.getByte(0));
            assertEquals(7, v.get(ValueLayout.JAVA_INT_UNALIGNED, 4));
            assertEquals(9, v.getByte(8));
            assertEquals(3, v.get(ValueLayout.JAVA_SHORT_UNALIGNED, 10));
        }
    }

    @Test
    void everyBooleanAccessorIsNamedIs() throws Exception {
        List<String> wrong = new ArrayList<>();
        for (Class<?> c : publicClasses()) {
            for (Method m : c.getDeclaredMethods()) {
                if (Modifier.isPublic(m.getModifiers()) && m.getReturnType() == boolean.class
                        && m.getParameterCount() == 0 && !m.getName().startsWith("is")) {
                    wrong.add(m.toString());
                }
            }
        }
        assertEquals(List.of(), wrong, "boolean accessors not named isXxx()");
        assertTrue(WriterOptions.defaults().isNotifyEnabled());
        assertTrue(WriterOptions.defaults().isPrefaultEnabled());
        assertTrue(!WriterOptions.defaults().prefault(false).isPrefaultEnabled());
        assertTrue(!ReaderOptions.defaults().notify(false).isNotifyEnabled());
        assertTrue(ReaderOptions.defaults().durableOnly(true).isDurableOnly());
    }

    @Test
    void aBatchFailureReportsAnIndexOfTheSameTypeAsTheBatchSize() throws Exception {
        assertEquals(AppendBatch.class.getMethod("size").getReturnType(),
                BatchAppendException.class.getMethod("appended").getReturnType());
        assertEquals(int.class, BatchAppendException.class.getMethod("appended").getReturnType());
    }

    @Test
    void aRecordCopySharesItsPayloadArrayAsDocumented() {
        byte[] payload = {1, 2, 3};
        RecordCopy copy = new RecordCopy(5, 1, 2, 3, payload);
        assertSame(payload, copy.payload(), "neither the constructor nor payload() copies the array");
        RecordCopy same = new RecordCopy(5, 1, 2, 3, payload.clone());
        assertEquals(same, copy);
        assertEquals(same.hashCode(), copy.hashCode());
        payload[0] = 9;
        assertNotEquals(same, copy, "writing into the shared array changes the record's content");
    }

    @Test
    void anAddWhoseCopyFailsLeavesTheBatchUnchanged() throws Exception {
        AppendBatch batch = new AppendBatch(1, 0); // no payload room: every add must grow the buffer first
        batch.add(1, 10, 20, new byte[] {4, 5, 6, 7});
        Arena closed = Arena.ofConfined();
        MemorySegment gone = closed.allocate(100);
        closed.close();
        assertThrows(IllegalStateException.class, () -> batch.add(2, 0, 0, gone));
        Arena shared = Arena.ofShared();
        ByteBuffer goneBuffer = shared.allocate(50).asByteBuffer();
        shared.close();
        assertThrows(IllegalStateException.class, () -> batch.add(3, 0, 0, goneBuffer));
        AtomicReference<MemorySegment> confined = new AtomicReference<>();
        Thread t = Thread.ofPlatform().start(() -> confined.set(Arena.ofConfined().allocate(30))); // that thread's
        t.join();
        assertThrows(WrongThreadException.class, () -> batch.add(4, 0, 0, confined.get()));
        assertEquals(1, batch.size());
        assertEquals(4, batch.payloadBytes(), "bytes that were never added are not counted");
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            assertEquals(0, w.appendBatch(batch));
            assertEquals(1, w.nextSeq());
            batch.clear();
            batch.add(5, 0, 0, ByteBuffer.wrap(new byte[] {8}));
            assertEquals(1, w.appendBatch(batch));
        }
        try (Reader r = Reader.open(dir)) {
            RecordView v = r.tryNext();
            assertEquals(1, v.schemaId());
            assertArrayEquals(new byte[] {4, 5, 6, 7}, v.payloadToArray());
            assertEquals(5, r.tryNext().schemaId());
        }
    }

    @Test
    void versionNumbersAreReadAsUnsigned() {
        assertEquals("0.4.0", NativeLibrary.versionString(4000));
        assertEquals("1.2.3", NativeLibrary.versionString(1_002_003));
        assertEquals("4294.967.295", NativeLibrary.versionString(-1));
        assertEquals("2147.483.648", NativeLibrary.versionString(Integer.MIN_VALUE));
    }

    @Test
    void aVerifyReportKeepsTheRawReasonCode() {
        VerifyReport unknown = new VerifyReport(1, 1, 0, 0, 1, 0, 64, 999);
        assertEquals(CorruptReason.UNKNOWN, unknown.firstProblemReason());
        assertEquals(999, unknown.firstProblemReasonCode());
        assertTrue(unknown.toString().contains("999"), unknown.toString());
        assertNotEquals(unknown, new VerifyReport(1, 1, 0, 0, 1, 0, 64, 998), "two unknown reasons differ");
        VerifyReport intact = Journal.verify(writeOne());
        assertEquals(CorruptReason.NONE, intact.firstProblemReason());
        assertEquals(CorruptReason.NONE.code(), intact.firstProblemReasonCode());
    }

    private Path writeOne() {
        Path dir = tmp.resolve("v");
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[1]);
        }
        return dir;
    }
}
