package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.management.ManagementFactory;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * The hot paths allocate nothing per record once warm. Measured with the thread's allocated-bytes counter
 * ({@code com.sun.management.ThreadMXBean}) around many operations; a small fixed allowance covers the counter's own
 * granularity and one-off allocations (JIT, class loading), so a per-record allocation of even 16 bytes fails.
 */
class AllocationTest {
    private static final int OPS = 50_000;
    private static final long ALLOWANCE = 64 * 1024; // about 1.3 bytes per operation

    @TempDir
    Path tmp;

    private static final com.sun.management.ThreadMXBean THREADS =
            (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();

    interface Op {
        void run(int i) throws Exception;
    }

    private static long allocated(Op op) throws Exception {
        for (int i = 0; i < OPS; i++) { // warm-up: let the JIT compile the path
            op.run(i);
        }
        for (int round = 0; round < 3; round++) {
            long before = THREADS.getCurrentThreadAllocatedBytes();
            for (int i = 0; i < OPS; i++) {
                op.run(i);
            }
            long bytes = THREADS.getCurrentThreadAllocatedBytes() - before;
            if (bytes <= ALLOWANCE) {
                return bytes;
            }
        }
        long before = THREADS.getCurrentThreadAllocatedBytes();
        for (int i = 0; i < OPS; i++) {
            op.run(i);
        }
        return THREADS.getCurrentThreadAllocatedBytes() - before;
    }

    private static void assertNoAllocation(String what, Op op) throws Exception {
        long bytes = allocated(op);
        assertTrue(bytes <= ALLOWANCE, what + " allocated " + bytes + " bytes for " + OPS + " operations ("
                + (bytes / OPS) + " per operation)");
    }

    @Test
    void appendsAndReadsAllocateNothingPerRecord() throws Exception {
        assertTrue(THREADS.isThreadAllocatedMemorySupported());
        THREADS.setThreadAllocatedMemoryEnabled(true);
        Path dir = tmp.resolve("j");
        WriterOptions options = WriterOptions.defaults().segmentSize(64L << 20).prefault(false);
        byte[] payload = new byte[64];
        try (Writer w = Writer.open(dir, options); Arena arena = Arena.ofConfined()) {
            MemorySegment nativePayload = arena.allocate(64);
            ByteBuffer direct = ByteBuffer.allocateDirect(64);
            AppendBatch batch = new AppendBatch(16, 16 * 64);
            PayloadFiller filler = p -> p.set(ValueLayout.JAVA_LONG_UNALIGNED, 0, 42L);

            assertNoAllocation("append(byte[])", i -> w.append(1, i, i, payload));
            assertNoAllocation("append(MemorySegment)", i -> w.append(1, i, i, nativePayload));
            assertNoAllocation("append(ByteBuffer)", i -> w.append(1, i, i, direct));
            assertNoAllocation("appendWith", i -> w.appendWith(1, i, i, 64, filler));
            ByteBuffer heap = ByteBuffer.allocate(64);
            PayloadFiller bufferFiller = p -> p.put(0, direct).put(0, heap);
            assertNoAllocation("appendWith with PayloadBuffer.put(ByteBuffer), direct and heap",
                    i -> w.appendWith(1, i, i, 64, bufferFiller));
            assertNoAllocation("appendBatch", i -> {
                if (i % 16 == 0) {
                    batch.clear();
                    for (int k = 0; k < 16; k++) {
                        batch.add(1, i, i, payload);
                    }
                    w.appendBatch(batch);
                }
            });

            long total = w.nextSeq();
            for (boolean mapped : new boolean[] {false, true}) {
                try (Reader r = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir)) {
                    long[] sum = new long[1];
                    ByteBuffer directOut = ByteBuffer.allocateDirect(64);
                    ByteBuffer heapOut = ByteBuffer.allocate(64);
                    assertNoAllocation((mapped ? "mapped" : "safe") + " tryNext and view accessors", i -> {
                        RecordView v = r.tryNext();
                        if (v == null) {
                            r.seek(0);
                            return;
                        }
                        sum[0] += v.seq() + v.schemaId() + v.venueTsNs() + v.localTsNs() + v.payloadLength()
                                + v.get(ValueLayout.JAVA_LONG_UNALIGNED, 0) + v.getByte(63);
                        v.copyPayloadTo(nativePayload, 0);
                        v.copyPayloadTo(payload, 0);
                        directOut.clear();
                        v.copyPayloadTo(directOut);
                        heapOut.clear();
                        v.copyPayloadTo(heapOut);
                    });
                    assertTrue(sum[0] != 0);
                }
            }
            try (Reader r = Reader.open(dir)) {
                ReadBatch rb = new ReadBatch(64, 64 * 64);
                long[] seen = new long[1];
                assertNoAllocation("readBatch", i -> {
                    if (i % 16 != 0) {
                        return;
                    }
                    int n = r.tryReadBatch(rb);
                    if (n == 0) {
                        r.seek(0);
                    }
                    for (int k = 0; k < n; k++) {
                        seen[0] += rb.get(k).getByte(0) + rb.get(k).seq();
                    }
                });
                assertTrue(seen[0] > 0);
                r.seek(0);
                assertNoAllocation("timed readBatch with records available", i -> {
                    if (i % 16 != 0) {
                        return;
                    }
                    int n = r.readBatch(rb, 1, java.util.concurrent.TimeUnit.SECONDS);
                    if (n == 0 || r.position() >= total - 64) {
                        r.seek(0);
                    }
                    for (int k = 0; k < n; k++) {
                        seen[0] += rb.get(k).getByte(0);
                    }
                });
            }
            try (Reader r = Reader.open(dir)) {
                assertNoAllocation("timed next with records available", i -> {
                    if (r.next(1, java.util.concurrent.TimeUnit.SECONDS) == null || r.position() >= total - 1) {
                        r.seek(0);
                    }
                });
                r.seek(0);
                assertNoAllocation("next() with records available", i -> {
                    r.next();
                    if (r.position() >= total - 1) {
                        r.seek(0);
                    }
                });
            }
            assertEquals(total, w.nextSeq());
        }
    }
}
