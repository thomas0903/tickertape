package io.github.thomas0903.tickertape;

import java.lang.foreign.ValueLayout;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.junit.jupiter.api.io.TempDir;

/**
 * An informal measurement of the read path (a read plus the view's accessors, and the accessors alone), for ADR 0037
 * &rarr; Threads (what publishing a close to every accessor costs). Not a benchmark under
 * docs/playbooks/run-benchmark.md: it runs only with -Dtickertape.bench=true and prints nanoseconds per operation
 * (best of ten rounds), measured on whatever machine runs it.
 */
@EnabledIfSystemProperty(named = "tickertape.bench", matches = "true")
class ReadPathCostTest {
    @TempDir
    Path tmp;

    private static final int RECORDS = 100_000;
    private static final int N = 2_000_000;

    interface Op {
        long run();
    }

    private static volatile long sink;

    private static double nsPerOp(Op op) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += op.run();
        }
        double best = Double.MAX_VALUE;
        for (int round = 0; round < 10; round++) {
            long start = System.nanoTime();
            for (int i = 0; i < N; i++) {
                s += op.run();
            }
            best = Math.min(best, (System.nanoTime() - start) / (double) N);
        }
        sink = s;
        return best;
    }

    @Test
    void readPathCosts() {
        Path dir = tmp.resolve("j");
        try (Writer w = Writer.open(dir, WriterOptions.defaults().segmentSize(64L << 20).prefault(false))) {
            byte[] payload = new byte[64];
            for (int i = 0; i < RECORDS; i++) {
                payload[0] = (byte) i;
                w.append(1, i, i, payload);
            }
        }
        for (boolean mapped : new boolean[] {false, true}) {
            try (Reader r = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir)) {
                double read = nsPerOp(() -> {
                    RecordView v = r.tryNext();
                    if (v == null) {
                        r.seek(0);
                        return 0;
                    }
                    return v.seq() + v.schemaId() + v.venueTsNs() + v.localTsNs() + v.payloadLength()
                            + v.get(ValueLayout.JAVA_LONG_UNALIGNED, 0) + v.getByte(63);
                });
                r.seek(0);
                RecordView v = r.tryNext();
                double accessors = nsPerOp(() -> v.seq() + v.schemaId() + v.venueTsNs() + v.localTsNs()
                        + v.payloadLength() + v.get(ValueLayout.JAVA_LONG_UNALIGNED, 8) + v.getByte(63));
                System.out.printf("%s: tryNext + 7 accessors %.2f ns, 7 accessors alone %.2f ns%n",
                        mapped ? "mapped" : "safe", read, accessors);
            }
        }
    }
}
