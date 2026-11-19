package io.github.thomas0903.tickertape;

import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;

/**
 * The segments over the last direct buffer and the last read-only heap buffer that a writer's fills copied from
 * ({@link PayloadBuffer#put(long, ByteBuffer)}), one slot per kind, so that alternating a direct buffer and a read-only
 * one allocates nothing either.
 *
 * <p>It keeps those buffers, and whatever their memory keeps alive, reachable, so only the {@link Writer} references
 * it: the writer hands it to its payload buffer for the length of one {@link Writer#appendWith} and takes it back
 * afterwards. The payload buffer is reachable from the writer's upcall stub, whose target the JVM roots globally until
 * the stub's arena is closed; anything the caller passed in and the stub could reach would outlive a writer that was
 * dropped without being closed (and keep it, and its journal's lock, if that memory refers back to the writer).
 * Reachable only from the writer, the cache and the writer form at most a cycle, which the collector frees.
 */
final class BufferCache {
    private BufferSegment direct;
    private BufferSegment readOnly;

    /** A segment over the whole of {@code buffer} (index 0 to its capacity), remembered for the next call. */
    MemorySegment segment(ByteBuffer buffer) {
        if (buffer.isDirect()) {
            BufferSegment s = BufferSegment.of(direct, buffer);
            direct = s;
            return s.segment;
        }
        BufferSegment s = BufferSegment.of(readOnly, buffer);
        readOnly = s;
        return s.segment;
    }

    /** Forgets the remembered buffers. */
    void clear() {
        direct = null;
        readOnly = null;
    }
}
