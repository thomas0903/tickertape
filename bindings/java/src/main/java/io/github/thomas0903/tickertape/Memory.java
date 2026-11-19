package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/** Shared memory helpers. */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
final class Memory {
    private Memory() {}

    /**
     * A segment over the whole address space, in the global scope. The {@link PayloadBuffer} writes library memory
     * through it at {@code address + offset}, after checking the offset, its validity and its thread, so the hot path
     * creates no segment objects; the writer's guard keeps that memory alive for as long as the buffer is valid.
     * Nothing outside this package sees it. Record views do not use it (see {@link #overAddressSpace}).
     */
    static final MemorySegment ALL = MemorySegment.NULL.reinterpret(Long.MAX_VALUE);

    /**
     * A segment over the whole address space whose lifetime is {@code arena}'s: a reader's views read library memory
     * through it at {@code address + offset}, so closing the arena ends every access on every thread, and later
     * accesses throw {@link IllegalStateException}. One object per reader; nothing per record.
     */
    static MemorySegment overAddressSpace(Arena arena) {
        return MemorySegment.NULL.reinterpret(Long.MAX_VALUE, arena, null);
    }

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
