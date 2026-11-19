package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.ALL;
import static io.github.thomas0903.tickertape.Memory.BYTE;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.util.Objects;

/**
 * The payload of a record being appended by {@link Writer#appendWith}, written in place inside the journal's mapped
 * segment. A {@link PayloadFiller} receives one; it is valid only until the filler returns, and every method throws
 * {@link IllegalStateException} after that. The writer reuses one object for all its appends, so filling allocates
 * nothing (unless you ask for {@link #segment()}).
 *
 * <p>The bytes start zeroed, and no reader can see them until the filler returns normally and the record is committed.
 * Offsets are checked against {@link #length()}. Typed writes take the layout to use, as
 * {@link MemorySegment#set(ValueLayout.OfInt, long, int)} does; payloads have no alignment guarantee, so use the
 * unaligned layouts ({@link ValueLayout#JAVA_LONG_UNALIGNED} and so on).
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
public final class PayloadBuffer {
    private long address;
    private long length;
    private boolean active;
    private Arena arena;

    PayloadBuffer() {}

    void begin(long address, long length) {
        this.address = address;
        this.length = length;
        this.active = true;
    }

    void end() {
        active = false;
        if (arena != null) {
            Arena a = arena;
            arena = null;
            a.close();
        }
    }

    private long at(long offset, long size) {
        if (!active) {
            throw new IllegalStateException("this payload buffer is no longer valid: its fill callback has returned");
        }
        Objects.checkFromIndexSize(offset, size, length);
        return address + offset;
    }

    /** The payload length given to {@link Writer#appendWith}. */
    public long length() {
        at(0, 0);
        return length;
    }

    /** Writes one byte at {@code offset}. */
    public void setByte(long offset, byte value) {
        ALL.set(BYTE, at(offset, 1), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfByte layout, long offset, byte value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfShort layout, long offset, short value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfChar layout, long offset, char value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfInt layout, long offset, int value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfLong layout, long offset, long value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfFloat layout, long offset, float value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Writes {@code value} at {@code offset} with {@code layout}. */
    public void set(ValueLayout.OfDouble layout, long offset, double value) {
        ALL.set(layout, at(offset, layout.byteSize()), value);
    }

    /** Copies all of {@code src} to {@code offset}. */
    public PayloadBuffer put(long offset, byte[] src) {
        return put(offset, src, 0, src.length);
    }

    /** Copies {@code len} bytes of {@code src} from {@code srcOffset} to {@code offset}. */
    public PayloadBuffer put(long offset, byte[] src, int srcOffset, int len) {
        Objects.checkFromIndexSize(srcOffset, len, src.length);
        MemorySegment.copy(src, srcOffset, ALL, BYTE, at(offset, len), len);
        return this;
    }

    /** Copies all of {@code src} to {@code offset}. */
    public PayloadBuffer put(long offset, MemorySegment src) {
        MemorySegment.copy(src, 0, ALL, at(offset, src.byteSize()), src.byteSize());
        return this;
    }

    /** Copies the bytes between {@code src}'s position and limit to {@code offset}; {@code src} is not changed. */
    public PayloadBuffer put(long offset, ByteBuffer src) {
        int n = src.remaining();
        long dst = at(offset, n);
        if (src.hasArray()) {
            MemorySegment.copy(src.array(), src.arrayOffset() + src.position(), ALL, BYTE, dst, n);
        } else {
            MemorySegment.copy(MemorySegment.ofBuffer(src), 0, ALL, dst, n);
        }
        return this;
    }

    /**
     * A writable segment over the whole payload, for code that wants a {@link MemorySegment}. Its scope closes when the
     * filler returns, so it cannot outlive the append. The first call per append allocates a small scope object.
     */
    public MemorySegment segment() {
        at(0, 0);
        if (arena == null) {
            arena = Arena.ofConfined();
        }
        return MemorySegment.ofAddress(address).reinterpret(length, arena, null);
    }

    @Override
    public String toString() {
        return active ? "PayloadBuffer[" + length + " bytes]" : "PayloadBuffer[no longer valid]";
    }
}
