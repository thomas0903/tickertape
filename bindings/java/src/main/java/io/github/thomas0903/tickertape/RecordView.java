package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.ALL;
import static io.github.thomas0903.tickertape.Memory.BYTE;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.BufferOverflowException;
import java.nio.ByteBuffer;
import java.util.Objects;
import java.util.function.Function;

/**
 * A borrowed view of one record: from {@link Reader#next} and friends, or from {@link ReadBatch#get(int)}.
 *
 * <p><b>Lifetime.</b> A view from a reader is valid until the next call on that reader (any method except
 * {@link Reader#position()} and {@link Reader#isClosed()}) or until the reader is closed; a view from a batch until
 * the batch is filled again. The reader returns the <em>same</em> view object every time (no allocation per record),
 * so do not keep it: copy what you need with {@link #copy()}, {@link #payloadToArray()} or {@code copyPayloadTo}.
 * Every method checks validity first and throws {@link IllegalStateException} on a view that is no longer valid.
 *
 * <p>The payload is not copied when the view is made: for a mapped reader it lies in the journal's memory mapping,
 * for the safe reader in the reader's own buffer. The accessors read it through the checks above. {@link #withPayload}
 * hands out a {@link MemorySegment} whose scope closes when the action returns, so a segment can never outlive the
 * record. What the checks cannot catch: another thread closing or advancing the reader <em>while</em> this thread reads
 * a view. Readers, and their views, are for one thread at a time (see {@link Reader}).
 *
 * <p>Typed reads take the layout to use, as {@link MemorySegment#get(ValueLayout.OfInt, long)} does. Payloads have no
 * alignment guarantee, so use the unaligned layouts ({@link ValueLayout#JAVA_INT_UNALIGNED} and so on, in native byte
 * order, or with {@code withOrder}); an aligned layout at a misaligned address throws
 * {@link IllegalArgumentException}.
 *
 * <p>Unsigned fields of the format are returned with the same bits in Java {@code long} and {@code int}.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
public final class RecordView {
    private final Generation owner;
    /** The reader whose memory the payload is in, or null for a batch (whose memory the batch owns). */
    private final Reader reader;
    private long generation = -1;

    private long seq;
    private long venueTsNs;
    private long localTsNs;
    private int schemaId;
    private long address;
    private long length;

    RecordView(Generation owner, Reader reader) {
        this.owner = owner;
        this.reader = reader;
    }

    /** Points the view at a record, valid for the owner's current generation. */
    void set(long seq, int schemaId, long venueTsNs, long localTsNs, long address, long length) {
        this.seq = seq;
        this.schemaId = schemaId;
        this.venueTsNs = venueTsNs;
        this.localTsNs = localTsNs;
        this.address = address;
        this.length = length;
        this.generation = owner.value;
    }

    private void check() {
        if (generation != owner.value) {
            throw new IllegalStateException("this record view is no longer valid: the " + (reader != null ? "reader" : "batch")
                    + " it came from has moved on or was closed; copy what you need before the next call");
        }
    }

    /** Whether the view is still valid (see the class documentation). */
    public boolean isValid() {
        return generation == owner.value;
    }

    /** The record's sequence number: its position in the journal, from 0, without gaps. */
    public long seq() {
        check();
        return seq;
    }

    /** The caller-defined schema identifier given at append (unsigned 32-bit). */
    public int schemaId() {
        check();
        return schemaId;
    }

    /** The venue timestamp given at append (unsigned 64-bit; by convention nanoseconds since the Unix epoch). */
    public long venueTsNs() {
        check();
        return venueTsNs;
    }

    /** The local timestamp given at append (unsigned 64-bit; by convention nanoseconds since the Unix epoch). */
    public long localTsNs() {
        check();
        return localTsNs;
    }

    /** The number of payload bytes. */
    public long payloadLength() {
        check();
        return length;
    }

    private long at(long offset, long size) {
        check();
        Objects.checkFromIndexSize(offset, size, length);
        return address + offset;
    }

    /** The payload byte at {@code offset}. */
    public byte getByte(long offset) {
        return ALL.get(BYTE, at(offset, 1));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public byte get(ValueLayout.OfByte layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public short get(ValueLayout.OfShort layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public char get(ValueLayout.OfChar layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public int get(ValueLayout.OfInt layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public long get(ValueLayout.OfLong layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public float get(ValueLayout.OfFloat layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public double get(ValueLayout.OfDouble layout, long offset) {
        return ALL.get(layout, at(offset, layout.byteSize()));
    }

    /** Copies the whole payload into {@code dst} from {@code dstOffset}. */
    public void copyPayloadTo(byte[] dst, int dstOffset) {
        long src = at(0, length);
        Objects.checkFromIndexSize(dstOffset, length, dst.length);
        MemorySegment.copy(ALL, BYTE, src, dst, dstOffset, (int) length);
    }

    /** Copies the whole payload into {@code dst} from {@code dstOffset} (any segment the caller can write). */
    public void copyPayloadTo(MemorySegment dst, long dstOffset) {
        long src = at(0, length);
        MemorySegment.copy(ALL, src, dst, dstOffset, length);
    }

    /**
     * Copies the whole payload into {@code dst} at its position and advances the position.
     *
     * @throws BufferOverflowException if fewer than {@link #payloadLength()} bytes remain in {@code dst}
     */
    public void copyPayloadTo(ByteBuffer dst) {
        long src = at(0, length);
        if (dst.isReadOnly()) {
            throw new java.nio.ReadOnlyBufferException();
        }
        if (dst.remaining() < length) {
            throw new BufferOverflowException();
        }
        int n = (int) length;
        int pos = dst.position();
        if (dst.hasArray()) {
            MemorySegment.copy(ALL, BYTE, src, dst.array(), dst.arrayOffset() + pos, n);
        } else {
            MemorySegment.copy(ALL, src, MemorySegment.ofBuffer(dst), 0, n);
        }
        dst.position(pos + n);
    }

    /** A new array with the payload. */
    public byte[] payloadToArray() {
        long src = at(0, length);
        byte[] out = new byte[Memory.toIntLength(length)];
        MemorySegment.copy(ALL, BYTE, src, out, 0, out.length);
        return out;
    }

    /** A copy of the whole record, independent of the reader. */
    public RecordCopy copy() {
        byte[] payload = payloadToArray();
        return new RecordCopy(seq, schemaId, venueTsNs, localTsNs, payload);
    }

    /**
     * Runs {@code action} with a read-only segment over the payload, without copying it, and returns its result. The
     * segment's scope is closed when the action returns, so using it afterwards throws {@link IllegalStateException}.
     * While the action runs, the reader refuses every other call (it would move the payload), so the action must not
     * use the reader. Creates a small scope object per call; the typed accessors above allocate nothing.
     */
    public <R> R withPayload(Function<? super MemorySegment, ? extends R> action) {
        Objects.requireNonNull(action, "action");
        check();
        if (reader != null) {
            reader.enterForView();
        }
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment payload = MemorySegment.ofAddress(address).reinterpret(length, arena, null).asReadOnly();
            return action.apply(payload);
        } finally {
            if (reader != null) {
                reader.exitForView();
            }
        }
    }

    @Override
    public String toString() {
        if (!isValid()) {
            return "RecordView[no longer valid]";
        }
        return "RecordView[seq=" + Long.toUnsignedString(seq) + ", schemaId=" + Integer.toUnsignedString(schemaId)
                + ", venueTsNs=" + Long.toUnsignedString(venueTsNs) + ", localTsNs=" + Long.toUnsignedString(localTsNs)
                + ", payload=" + length + " bytes]";
    }
}
