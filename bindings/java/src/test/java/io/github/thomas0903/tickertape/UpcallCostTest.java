package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.file.Path;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfSystemProperty;
import org.junit.jupiter.api.io.TempDir;

/**
 * An informal measurement of what the zero-copy append's upcall costs against a copying append of the same payload,
 * for ADR 0037. Not a benchmark under docs/playbooks/run-benchmark.md: it runs only with -Dtickertape.bench=true and
 * prints nanoseconds per append (best of five rounds), measured on whatever machine runs it.
 */
@EnabledIfSystemProperty(named = "tickertape.bench", matches = "true")
class UpcallCostTest {
    @TempDir
    Path tmp;

    private static final int N = 200_000;

    interface Op {
        void run();
    }

    private static double nsPerOp(int n, Op op) {
        long start = System.nanoTime();
        for (int i = 0; i < n; i++) {
            op.run();
        }
        return (System.nanoTime() - start) / (double) n;
    }

    private static double nsPerOp(Op op) {
        for (int i = 0; i < N; i++) {
            op.run();
        }
        double best = Double.MAX_VALUE;
        for (int round = 0; round < 5; round++) {
            best = Math.min(best, nsPerOp(N, op));
        }
        return best;
    }

    /**
     * Appends write fresh journal memory, so page faults and rolls are part of every append. The three forms are
     * measured in alternating rounds over the same amount of payload (32 MiB per round), and the best round is kept.
     */
    @Test
    void appendCosts() {
        WriterOptions options = WriterOptions.defaults().segmentSize(256L << 20).notify(false);
        System.out.printf("%-8s %14s %14s %14s %14s%n", "payload", "append byte[]", "append segment", "appendWith",
                "with - segment");
        for (int size : new int[] {64, 1024, 16 * 1024}) {
            Path dir = tmp.resolve("j" + size);
            int n = Math.max(2_000, (32 << 20) / size);
            try (Writer w = Writer.open(dir, options); Arena arena = Arena.ofConfined()) {
                byte[] bytes = new byte[size];
                MemorySegment segment = arena.allocate(size);
                PayloadFiller filler = p -> p.put(0, segment);
                double arr = Double.MAX_VALUE;
                double seg = Double.MAX_VALUE;
                double with = Double.MAX_VALUE;
                for (int round = 0; round < 8; round++) {
                    arr = Math.min(arr, nsPerOp(n, () -> w.append(1, 0, 0, bytes)));
                    seg = Math.min(seg, nsPerOp(n, () -> w.append(1, 0, 0, segment)));
                    with = Math.min(with, nsPerOp(n, () -> w.appendWith(1, 0, 0, size, filler)));
                }
                System.out.printf("%-8d %14.1f %14.1f %14.1f %14.1f%n", size, arr, seg, with, with - seg);
            }
        }
        Path dir = tmp.resolve("read");
        try (Writer w = Writer.open(dir, options)) {
            for (int i = 0; i < N; i++) {
                w.append(1, 0, 0, new byte[64]);
            }
        }
        for (boolean mapped : new boolean[] {false, true}) {
            try (Reader r = mapped ? Reader.openMappedUnsafe(dir) : Reader.open(dir)) {
                double next = nsPerOp(() -> {
                    if (r.tryNext() == null) {
                        r.seek(0);
                    }
                });
                System.out.printf("tryNext (%s, 64 B): %.1f ns%n", mapped ? "mapped" : "safe", next);
            }
        }
    }
}
