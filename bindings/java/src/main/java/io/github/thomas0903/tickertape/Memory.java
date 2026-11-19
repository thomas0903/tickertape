package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/** Shared memory helpers. */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
final class Memory {
    private Memory() {}

    /**
     * A segment over the whole address space. Views and payload buffers read and write library memory through it at
     * {@code address + offset}, after checking the offset against their own length and validity, so the hot paths
     * create no segment objects. Nothing outside this package sees it.
     */
    static final MemorySegment ALL = MemorySegment.NULL.reinterpret(Long.MAX_VALUE);

    /** Owned scratch memory: freed by the garbage collector once unreachable (it never holds a C handle). */
    static MemorySegment allocate(long bytes) {
        return Arena.ofAuto().allocate(Math.max(bytes, 1), 8);
    }

    static int toIntLength(long length) {
        if (length > Integer.MAX_VALUE - 8) {
            throw new IllegalStateException("payload of " + length + " bytes does not fit in a Java array");
        }
        return (int) length;
    }

    static final ValueLayout.OfByte BYTE = ValueLayout.JAVA_BYTE;
}
