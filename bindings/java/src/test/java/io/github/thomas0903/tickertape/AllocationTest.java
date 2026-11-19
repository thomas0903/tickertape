package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * The hot paths allocate nothing per call once warm. Each operation is measured on its own (so a failure names the
 * operation that allocates) with the thread's allocated-bytes counter ({@code com.sun.management.ThreadMXBean}) around
 * {@link #CALLS} calls of it. A batch operation counts as one call per batch call, not per record. The allowance is one
 * byte per call, for the counter's granularity and one-off allocations (JIT, class loading): an allocation of even one
 * object (16 bytes at least) per call fails.
 *
 * <p>The build runs this class a second time with {@code -XX:-DoEscapeAnalysis} (see {@code pom.xml}): an allocation
 * that only the JIT's escape analysis removes (a wrapper object built per call) then fails deterministically, instead
 * of only on a runner where the JIT happens to compile the path differently (ADR 0037 &rarr; Allocation).
 */
class AllocationTest {
    private static final int CALLS = Integer.getInteger("tickertape.test.allocCalls", 20_000);
    private static final long ALLOWANCE = CALLS; // one byte per call

    @TempDir
    Path tmp;

    private static final com.sun.management.ThreadMXBean THREADS =
            (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();

    interface Op {
        void run(int i) throws Exception;
    }

    private final List<String> failures = new ArrayList<>();

    @BeforeAll
    static void enableCounter() {
        assertTrue(THREADS.isThreadAllocatedMemorySupported());
        THREADS.setThreadAllocatedMemoryEnabled(true);
    }

    private static long allocated(Op op) throws Exception {
        for (int i = 0; i < CALLS; i++) { // warm-up: let the JIT compile the path
            op.run(i);
        }
        long bytes = Long.MAX_VALUE;
        for (int round = 0; round < 4 && bytes > ALLOWANCE; round++) {
            long before = THREADS.getCurrentThreadAllocatedBytes();
            for (int i = 0; i < CALLS; i++) {
                op.run(i);
            }
            bytes = THREADS.getCurrentThreadAllocatedBytes() - before;
        }
        return bytes;
    }

    /** Measures {@code op} and records a failure naming it; every operation is measured before the test fails. */
    private void measure(String what, Op op) throws Exception {
        long bytes = allocated(op);
        if (bytes > ALLOWANCE) {
            failures.add(what + ": " + bytes + " bytes for " + CALLS + " calls (" + (bytes / CALLS) + " per call)");
        }
    }

    private void assertNoFailures() {
        if (!failures.isEmpty()) {
            fail("allocating operations (escape analysis "
                    + (ManagementFactory.getRuntimeMXBean().getInputArguments().contains("-XX:-DoEscapeAnalysis")
                            ? "off" : "on")
                    + "):\n  " + String.join("\n  ", failures));
        }
    }

    private static final byte[] PAYLOAD = new byte[64];

    private Path writeJournal(int records) {
        Path dir = tmp.resolve("r");
        try (Writer w = Writer.open(dir, WriterOptions.defaults().segmentSize(16L << 20).prefault(false))) {
            for (int i = 0; i < records; i++) {
                PAYLOAD[63] = (byte) (i | 1);
                w.append(1, i, i, PAYLOAD);
            }
        }
        return dir;
    }

    @Test
    void writerCallsAllocateNothing() throws Exception {
        Path dir = tmp.resolve("w");
        WriterOptions options = WriterOptions.defaults().segmentSize(64L << 20).prefault(false);
        byte[] payload = new byte[64];
        try (Writer w = Writer.open(dir, options); Arena arena = Arena.ofConfined()) {
            MemorySegment nativePayload = arena.allocate(64);
            MemorySegment heapPayload = MemorySegment.ofArray(new byte[64]);
            ByteBuffer direct = ByteBuffer.allocateDirect(64);
            ByteBuffer heap = ByteBuffer.allocate(64);

            measure("append(byte[])", i -> w.append(1, i, i, payload));
            measure("append(native MemorySegment)", i -> w.append(1, i, i, nativePayload));
            measure("append(heap MemorySegment)", i -> w.append(1, i, i, heapPayload));
            measure("append(direct ByteBuffer)", i -> w.append(1, i, i, direct));
            measure("append(heap ByteBuffer)", i -> w.append(1, i, i, heap));

            PayloadFiller setLong = p -> p.set(ValueLayout.JAVA_LONG_UNALIGNED, 0, 42L);
            measure("appendWith, PayloadBuffer.set", i -> w.appendWith(1, i, i, 64, setLong));
            PayloadFiller setByte = p -> p.setByte(63, (byte) 1);
            measure("appendWith, PayloadBuffer.setByte", i -> w.appendWith(1, i, i, 64, setByte));
            PayloadFiller putArray = p -> p.put(0, payload);
            measure("appendWith, PayloadBuffer.put(byte[])", i -> w.appendWith(1, i, i, 64, putArray));
            PayloadFiller putSegment = p -> p.put(0, nativePayload);
            measure("appendWith, PayloadBuffer.put(MemorySegment)", i -> w.appendWith(1, i, i, 64, putSegment));
            PayloadFiller putDirect = p -> p.put(0, direct);
            measure("appendWith, PayloadBuffer.put(direct ByteBuffer)", i -> w.appendWith(1, i, i, 64, putDirect));
            PayloadFiller putHeap = p -> p.put(0, heap);
            measure("appendWith, PayloadBuffer.put(heap ByteBuffer)", i -> w.appendWith(1, i, i, 64, putHeap));

            // One call each: add (with a clear every 16 adds, so the batch keeps its size) and appendBatch.
            AppendBatch adds = new AppendBatch(16, 16 * 64);
            measure("AppendBatch.add(byte[])", i -> {
                if (adds.size() == 16) {
                    adds.clear();
                }
                adds.add(1, i, i, payload);
            });
            measure("AppendBatch.add(native MemorySegment)", i -> {
                if (adds.size() == 16) {
                    adds.clear();
                }
                adds.add(1, i, i, nativePayload);
            });
            measure("AppendBatch.add(direct ByteBuffer)", i -> {
                if (adds.size() == 16) {
                    adds.clear();
                }
                adds.add(1, i, i, direct);
            });
            measure("AppendBatch.add(heap ByteBuffer)", i -> {
                if (adds.size() == 16) {
                    adds.clear();
                }
                adds.add(1, i, i, heap);
            });
            AppendBatch batch = new AppendBatch(4, 4 * 64);
            for (int k = 0; k < 4; k++) {
                batch.add(1, k, k, payload);
            }
            measure("appendBatch (4 records per call)", i -> w.appendBatch(batch));
            measure("nextSeq", i -> w.nextSeq());
        }
        assertNoFailures();
    }

    @Test
    void readerCallsAndViewsAllocateNothing() throws Exception {
        int records = 4096;
        Path dir = writeJournal(records);
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment segmentOut = arena.allocate(64);
            byte[] arrayOut = new byte[64];
            ByteBuffer directOut = ByteBuffer.allocateDirect(64);
            ByteBuffer heapOut = ByteBuffer.allocate(64);
            for (boolean mapped : new boolean[] {false, true}) {
                String kind = mapped ? "mapped " : "safe ";
                try (Reader r = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir)) {
                    long[] sum = new long[1];
                    measure(kind + "tryNext", i -> {
                        if (r.tryNext() == null) {
                            r.seek(0);
                        }
                    });
                    measure(kind + "next(timeout) with records available", i -> {
                        if (r.next(1, TimeUnit.SECONDS) == null || r.position() >= records - 1) {
                            r.seek(0);
                        }
                    });
                    measure(kind + "next() with records available", i -> {
                        r.next();
                        if (r.position() >= records - 1) {
                            r.seek(0);
                        }
                    });
                    r.seek(0);
                    RecordView v = r.tryNext();
                    measure(kind + "view metadata", i -> sum[0] += v.seq() + v.schemaId() + v.venueTsNs()
                            + v.localTsNs() + v.payloadLength() + (v.isValid() ? 1 : 0));
                    measure(kind + "view get(layout)", i -> sum[0] += v.get(ValueLayout.JAVA_LONG_UNALIGNED, i & 7));
                    measure(kind + "view getByte", i -> sum[0] += v.getByte(63));
                    measure(kind + "view copyPayloadTo(MemorySegment)", i -> v.copyPayloadTo(segmentOut, 0));
                    measure(kind + "view copyPayloadTo(byte[])", i -> v.copyPayloadTo(arrayOut, 0));
                    measure(kind + "view copyPayloadTo(direct ByteBuffer)", i -> {
                        directOut.clear();
                        v.copyPayloadTo(directOut);
                    });
                    measure(kind + "view copyPayloadTo(heap ByteBuffer)", i -> {
                        heapOut.clear();
                        v.copyPayloadTo(heapOut);
                    });
                    assertTrue(sum[0] != 0);
                }
            }

            try (Reader r = Reader.open(dir)) {
                ReadBatch rb = new ReadBatch(64, 64 * 64);
                long[] seen = new long[1];
                measure("tryReadBatch and its views", i -> {
                    int n = r.tryReadBatch(rb);
                    if (n == 0) {
                        r.seek(0);
                    }
                    for (int k = 0; k < n; k++) {
                        RecordView v = rb.get(k);
                        seen[0] += v.getByte(63) + v.seq();
                    }
                });
                r.seek(0);
                measure("readBatch(timeout) with records available", i -> {
                    int n = r.readBatch(rb, 1, TimeUnit.SECONDS);
                    if (n == 0 || r.position() >= records - 64) {
                        r.seek(0);
                    }
                    seen[0] += n;
                });
                RecordView v = rb.get(0);
                measure("batch view copyPayloadTo(direct ByteBuffer)", i -> {
                    directOut.clear();
                    v.copyPayloadTo(directOut);
                });
                assertTrue(seen[0] > 0);
            }
        }
        assertNoFailures();
    }
}
