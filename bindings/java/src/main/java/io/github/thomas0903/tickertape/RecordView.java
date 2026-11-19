package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.BYTE;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.ref.Reference;
import java.nio.BufferOverflowException;
import java.nio.ByteBuffer;
import java.nio.ReadOnlyBufferException;
import java.util.Objects;
import java.util.function.Function;

/**
 * A borrowed view of one record: from {@link Reader#next} and friends, or from {@link ReadBatch#get(int)}.
 *
 * <p><b>Lifetime.</b> A view from a reader is valid until the next call on that reader (any method except
 * {@link Reader#position()} and {@link Reader#isClosed()}) or until the reader is closed; a view from a batch until
 * the batch is filled again. The reader returns the <em>same</em> view object every time (no allocation per record),
 * so do not keep it: copy what you need with {@link #copy()}, {@link #payloadToArray()} or {@code copyPayloadTo}.
 * Every method checks validity first and throws {@link IllegalStateException} on a view that is no longer valid. A view
 * keeps what it reads from alive (its reader or batch), so it never reads freed memory because its owner was
 * garbage collected.
 *
 * <p>Because the view object is reused, a reference kept past its validity becomes valid again: a saved reader view
 * shows the new record after the next successful read, and a saved batch view is re-pointed by a later
 * {@link ReadBatch#get(int)} of its index after a refill.
 *
 * <p>The payload is not copied when the view is made: for a mapped reader it lies in the journal's memory mapping,
 * for the safe reader in the reader's own buffer, for a batch in the batch's buffer. A reader's views read it through a
 * segment in the reader's shared arena, which {@link Reader#close()} closes before it frees the reader: an access in
 * progress when the reader is closed either finishes or throws {@link IllegalStateException}, every later access
 * throws, and none reads freed memory. Accessors that touch no payload memory (the metadata, {@link #payloadLength()},
 * copying an empty payload, {@link #isValid()}) see the close through the view's generation, which {@code close}
 * advances first. So closing a reader from another thread while a view is being read is safe. {@link #withPayload}
 * hands out a {@link MemorySegment} whose scope closes when the action returns, so a segment can never outlive the
 * record.
 *
 * <p>What the checks cannot catch: another thread <em>advancing</em> the reader (a read or {@code seek}) while this
 * thread reads its view breaks the rule that a reader is used by one thread at a time, with either reader; the view may
 * then read bytes of another record, or memory the reader has released (the safe reader may reuse its native buffer;
 * with the mapped reader, possibly an unmapped page: a crash). Use a reader and its views on one thread at a time (see
 * {@link Reader}).
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
    /** The reader or batch this view belongs to: a strong reference, so it stays reachable while the view is. */
    private final Object owner;
    /** The reader, for a reader's view (its guard is held during {@link #withPayload}); null for a batch's view. */
    private final Reader reader;
    private final Generation ownerGeneration;
    /**
     * What the payload is read through: the reader's segment over the address space, in its shared arena, or the
     * batch's buffer. Its scope is checked on every access, so memory that is gone is never read.
     */
    private final MemorySegment memory;
    private long generation = -1;

    private long seq;
    private long venueTsNs;
    private long localTsNs;
    private int schemaId;
    /** The payload's offset in {@link #memory}. */
    private long base;
    private long length;
    /** The last direct (or read-only) buffer {@link #copyPayloadTo(ByteBuffer)} copied into, with its segment. */
    private BufferSegment lastBuffer;

    private RecordView(Object owner, Reader reader, Generation generation, MemorySegment memory) {
        this.owner = owner;
        this.reader = reader;
        this.ownerGeneration = generation;
        this.memory = memory;
    }

    /** A view of {@code reader}'s records, which lie in memory that {@code memory}'s arena keeps readable. */
    static RecordView ofReader(Reader reader, Generation generation, MemorySegment memory) {
        return new RecordView(reader, reader, generation, memory);
    }

    /** A view of {@code batch}'s records, which lie in {@code buffer}. */
    static RecordView ofBatch(ReadBatch batch, Generation generation, MemorySegment buffer) {
        return new RecordView(batch, null, generation, buffer);
    }

    /** Points the view at a record whose payload is at {@code address}, valid for the owner's current generation. */
    void set(long seq, int schemaId, long venueTsNs, long localTsNs, long address, long length) {
        this.seq = seq;
        this.schemaId = schemaId;
        this.venueTsNs = venueTsNs;
        this.localTsNs = localTsNs;
        this.base = length == 0 ? 0 : address - memory.address();
        this.length = length;
        this.generation = ownerGeneration.get();
    }

    /**
     * Throws unless the view is valid. The owner's generation is read with an acquire load, so a close or a read on
     * another thread is seen by the next accessor, also in a compiled loop that reads nothing else.
     */
    private void check() {
        if (generation != ownerGeneration.get()) {
            throw new IllegalStateException("this record view is no longer valid: the " + (reader != null ? "reader" : "batch")
                    + " it came from has moved on or was closed; copy what you need before the next call");
        }
    }

    /** An access found the reader's arena closed: the reader was closed, possibly by another thread. */
    private IllegalStateException closedWhileReading(IllegalStateException cause) {
        return new IllegalStateException("this record view is no longer valid: its reader was closed", cause);
    }

    /**
     * Whether the view is still valid (see the class documentation). On any thread, this returns false once
     * {@link Reader#close()} has started invalidating the reader's view (before {@code close} returns), and every
     * accessor throws from then on.
     */
    public boolean isValid() {
        return generation == ownerGeneration.get() && memory.scope().isAlive();
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
        return base + offset;
    }

    /** The payload byte at {@code offset}. */
    public byte getByte(long offset) {
        long at = at(offset, 1);
        try {
            return memory.get(BYTE, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public byte get(ValueLayout.OfByte layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public short get(ValueLayout.OfShort layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public char get(ValueLayout.OfChar layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public int get(ValueLayout.OfInt layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public long get(ValueLayout.OfLong layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public float get(ValueLayout.OfFloat layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Reads a value at {@code offset} of the payload with {@code layout}. */
    public double get(ValueLayout.OfDouble layout, long offset) {
        long at = at(offset, layout.byteSize());
        try {
            return memory.get(layout, at);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Copies the whole payload into {@code dst} from {@code dstOffset}. */
    public void copyPayloadTo(byte[] dst, int dstOffset) {
        long src = at(0, length);
        Objects.checkFromIndexSize(dstOffset, length, dst.length);
        try {
            MemorySegment.copy(memory, BYTE, src, dst, dstOffset, (int) length);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /** Copies the whole payload into {@code dst} from {@code dstOffset} (any segment the caller can write). */
    public void copyPayloadTo(MemorySegment dst, long dstOffset) {
        long src = at(0, length);
        try {
            MemorySegment.copy(memory, src, dst, dstOffset, length);
        } catch (IllegalStateException e) {
            if (!memory.scope().isAlive()) {
                throw closedWhileReading(e);
            }
            throw e; // dst's own scope
        } finally {
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Copies the whole payload into {@code dst} at its position and advances the position. For a direct buffer the
     * view remembers a segment over the last buffer it copied into (and so keeps that buffer reachable), so repeated
     * copies into one buffer allocate nothing; the first copy into another direct buffer allocates a small wrapper.
     *
     * @throws BufferOverflowException if fewer than {@link #payloadLength()} bytes remain in {@code dst}
     */
    public void copyPayloadTo(ByteBuffer dst) {
        long src = at(0, length);
        if (dst.isReadOnly()) {
            throw new ReadOnlyBufferException();
        }
        if (dst.remaining() < length) {
            throw new BufferOverflowException();
        }
        int n = (int) length;
        int pos = dst.position();
        try {
            if (dst.hasArray()) {
                MemorySegment.copy(memory, BYTE, src, dst.array(), dst.arrayOffset() + pos, n);
            } else {
                BufferSegment target = BufferSegment.of(lastBuffer, dst);
                lastBuffer = target;
                MemorySegment.copy(memory, src, target.segment, pos, n);
            }
        } catch (IllegalStateException e) {
            if (!memory.scope().isAlive()) {
                throw closedWhileReading(e);
            }
            throw e;
        } finally {
            Reference.reachabilityFence(this);
        }
        dst.position(pos + n);
    }

    /** A new array with the payload. */
    public byte[] payloadToArray() {
        long src = at(0, length);
        byte[] out = new byte[Memory.toIntLength(length)];
        try {
            MemorySegment.copy(memory, BYTE, src, out, 0, out.length);
        } catch (IllegalStateException e) {
            throw closedWhileReading(e);
        } finally {
            Reference.reachabilityFence(this);
        }
        return out;
    }

    /** A copy of the whole record, independent of the reader. */
    public RecordCopy copy() {
        byte[] payload = payloadToArray();
        return new RecordCopy(seq, schemaId, venueTsNs, localTsNs, payload);
    }

    /**
     * Runs {@code action} with a read-only segment over the payload, without copying it, and returns its result. The
     * segment's scope is closed when the action returns, so using it afterwards throws {@link IllegalStateException};
     * it is confined to the calling thread. For a reader's view, the reader refuses every other call while the action
     * runs (it would move the payload), so the action must not use the reader, and a {@link Reader#close()} from
     * another thread waits for the action to return. For a batch's view, reading into that batch while the action runs
     * throws {@link IllegalStateException} (it would change the payload). Creates a small scope object per call; the
     * typed accessors above allocate nothing.
     */
    public <R> R withPayload(Function<? super MemorySegment, ? extends R> action) {
        Objects.requireNonNull(action, "action");
        check();
        ReadBatch batch = reader == null ? (ReadBatch) owner : null;
        if (reader != null) {
            reader.enterForView();
        } else {
            batch.enterPayloadAction();
        }
        try (Arena arena = Arena.ofConfined()) {
            check(); // the reader or batch cannot move or close from here until the action returns
            MemorySegment payload = MemorySegment.ofAddress(memory.address() + base).reinterpret(length, arena, null)
                    .asReadOnly();
            return action.apply(payload);
        } finally {
            if (reader != null) {
                reader.exitForView();
            } else {
                batch.exitPayloadAction();
            }
            Reference.reachabilityFence(this);
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
