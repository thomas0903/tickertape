package io.github.thomas0903.tickertape;

import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;

/**
 * A segment over the whole of a {@link ByteBuffer} (index 0 to its capacity), remembered by the object that copies to
 * or from buffers. {@link MemorySegment#ofBuffer} builds a new segment object (and, for a direct buffer, a scope
 * object) on every call, and only the JIT's escape analysis can remove them, which is not guaranteed (ADR 0037 &rarr;
 * Allocation); remembering the last buffer's segment makes repeated copies with one buffer allocate nothing. One
 * immutable object per buffer, read and replaced with one reference, so a racy use can at worst build it again. It
 * keeps the last buffer reachable until another buffer replaces it.
 */
final class BufferSegment {
    private final ByteBuffer buffer;
    /** The buffer's memory from index 0 to its capacity; read-only if the buffer is. Its scope is the buffer's. */
    final MemorySegment segment;

    private BufferSegment(ByteBuffer buffer) {
        this.buffer = buffer;
        this.segment = MemorySegment.ofBuffer(buffer.duplicate().clear());
    }

    /** {@code cached} if it is for {@code buffer}, else a new one (store it back for the next call). */
    static BufferSegment of(BufferSegment cached, ByteBuffer buffer) {
        return cached != null && cached.buffer == buffer ? cached : new BufferSegment(buffer);
    }
}
