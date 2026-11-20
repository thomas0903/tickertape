package io.github.thomas0903.tickertape;

import java.util.Arrays;
import java.util.List;
import java.util.Objects;

/**
 * Where a writer's helper threads run: the checkpoint thread of {@link Writer#requestCheckpoint()}, the prefault and
 * early write-back helpers and the spare-segment thread ({@link WriterOptions#helperCpus(HelperCpus)}; ADR 0038 &rarr;
 * Helper threads and CPU affinity). <b>Linux:</b> each helper sets its affinity when it starts, and the kernel
 * intersects the mask with the CPUs the process's cpuset allows. <b>macOS:</b> no effect. Immutable.
 */
public final class HelperCpus {
    static final int PROCESS = 0;
    static final int INHERITED = 1;
    static final int LIST = 2;

    private static final HelperCpus PROCESS_CPUS = new HelperCpus(PROCESS, new int[0]);
    private static final HelperCpus INHERITED_CPUS = new HelperCpus(INHERITED, new int[0]);

    final int placement;
    private final int[] cpus;

    private HelperCpus(int placement, int[] cpus) {
        this.placement = placement;
        this.cpus = cpus;
    }

    /**
     * Every CPU the process's cpuset allows, except the CPUs the kernel lists as isolated, unless that would leave
     * none. Not limited by a mask set with {@code taskset} or {@code sched_setaffinity} (such a mask belongs to each
     * thread). The default.
     */
    public static HelperCpus process() {
        return PROCESS_CPUS;
    }

    /** The mask inherited from the thread that creates the helper (the writer's thread). */
    public static HelperCpus inherited() {
        return INHERITED_CPUS;
    }

    /**
     * These CPUs (numbers as the kernel counts them, below 8,192), intersected with the process's cpuset. If the
     * kernel refuses the mask (no listed CPU is allowed, or the list is empty), a helper uses {@link #process()}.
     *
     * @throws IllegalArgumentException if a number is negative
     */
    public static HelperCpus list(int... cpus) {
        int[] copy = Objects.requireNonNull(cpus, "cpus").clone();
        for (int cpu : copy) {
            if (cpu < 0) {
                throw new IllegalArgumentException("CPU numbers must not be negative: " + cpu);
            }
        }
        return new HelperCpus(LIST, copy);
    }

    /** The CPUs of {@link #list(int...)}; empty for the other placements. */
    public List<Integer> cpus() {
        return Arrays.stream(cpus).boxed().toList();
    }

    int[] cpuArray() {
        return cpus;
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof HelperCpus h && placement == h.placement && Arrays.equals(cpus, h.cpus);
    }

    @Override
    public int hashCode() {
        return 31 * placement + Arrays.hashCode(cpus);
    }

    @Override
    public String toString() {
        return switch (placement) {
            case PROCESS -> "HelperCpus.process()";
            case INHERITED -> "HelperCpus.inherited()";
            default -> "HelperCpus.list" + Arrays.toString(cpus);
        };
    }
}
