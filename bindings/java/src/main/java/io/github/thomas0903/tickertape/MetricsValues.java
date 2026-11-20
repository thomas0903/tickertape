package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.StructLayout;
import java.time.Duration;
import java.util.Optional;
import java.util.OptionalLong;

/** Reads the metrics structs: {@code TICKERTAPE_UNKNOWN} ({@code UINT64_MAX}, -1 as a {@code long}) is empty. */
final class MetricsValues {
    private MetricsValues() {}

    static final long UNKNOWN = -1L;

    /** A struct of {@code layout} in {@code segment}, read by field name. */
    record Struct(MemorySegment segment, StructLayout layout) {
        long u64(String name) {
            return segment.get(JAVA_LONG, layout.byteOffset(PathElement.groupElement(name)));
        }

        int i32(String name) {
            return segment.get(JAVA_INT, layout.byteOffset(PathElement.groupElement(name)));
        }

        boolean flag(String name) {
            return i32(name) != 0;
        }

        OptionalLong optional(String name) {
            long v = u64(name);
            return v == UNKNOWN ? OptionalLong.empty() : OptionalLong.of(v);
        }

        /** Nanoseconds as a {@link Duration}; values beyond {@code Long.MAX_VALUE} saturate. */
        Optional<Duration> duration(String name) {
            long v = u64(name);
            if (v == UNKNOWN) {
                return Optional.empty();
            }
            return Optional.of(Duration.ofNanos(v < 0 ? Long.MAX_VALUE : v));
        }
    }

    /** Allocates a struct of {@code layout} in {@code arena}, with its {@code struct_size} set. */
    static MemorySegment sized(Arena arena, StructLayout layout) {
        MemorySegment s = arena.allocate(layout);
        s.set(JAVA_LONG, 0, layout.byteSize());
        return s;
    }
}
