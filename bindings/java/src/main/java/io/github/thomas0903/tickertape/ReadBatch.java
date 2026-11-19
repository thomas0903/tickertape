package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.util.Objects;
import java.util.function.Consumer;

/**
 * Memory for {@link Reader#readBatch}: room for up to {@link #maxRecords()} records whose payloads are copied one
 * after another into a buffer of {@link #bufferBytes()} bytes, all owned by the batch. Reuse one batch for every read:
 * reading into it allocates nothing.
 *
 * <p>The records stay in the batch until it is filled again. {@link #get(int)} returns the batch's view of record
 * {@code index}: one view object per index, so views of different records never alias ({@code get(0)} and
 * {@code get(1)} can be used side by side), created on the first {@code get} of that index and reused for that index
 * by every later read, so a warm batch allocates nothing. Every view is valid until the next read into this batch;
 * after that it throws {@link IllegalStateException} until {@code get} positions it again (a view kept from before a
 * refill is re-pointed by a later {@code get} of its index). While one of its views is inside
 * {@link RecordView#withPayload}, reading into the batch throws {@link IllegalStateException}. The payloads are in the
 * batch's own memory, so they never point into the journal and stay readable after the reader is closed; a view keeps
 * its batch, and so that memory, reachable. Not thread-safe.
 */
public final class ReadBatch {
    private static final long RECORD_SIZE = Signatures.RECORD.byteSize();
    private static final long OFF_SEQ = off("seq");
    private static final long OFF_VENUE = off("venue_ts_ns");
    private static final long OFF_LOCAL = off("local_ts_ns");
    private static final long OFF_PAYLOAD = off("payload");
    private static final long OFF_LEN = off("payload_len");
    private static final long OFF_SCHEMA = off("schema_id");

    private static long off(String name) {
        return Signatures.RECORD.byteOffset(PathElement.groupElement(name));
    }

    final MemorySegment records;
    final MemorySegment buffer;
    private final int maxRecords;
    private final long bufferLength;
    final Generation generation = new Generation();
    /** The view of each index, created on first use. */
    private final RecordView[] views;
    private int size;
    /** The number of this batch's views inside {@link RecordView#withPayload} (nested actions count each). */
    private int payloadActions;

    /**
     * A batch for up to {@code maxRecords} records and {@code bufferBytes} bytes of payload. A record whose payload is
     * larger than the whole buffer cannot be read with this batch ({@link BufferTooSmallException}).
     */
    public ReadBatch(int maxRecords, long bufferBytes) {
        if (maxRecords < 1 || bufferBytes < 0) {
            throw new IllegalArgumentException("maxRecords must be positive and bufferBytes not negative");
        }
        this.maxRecords = maxRecords;
        this.records = Memory.allocate(maxRecords * RECORD_SIZE);
        this.buffer = Memory.allocate(bufferBytes);
        this.bufferLength = bufferBytes;
        this.views = new RecordView[maxRecords];
    }

    /** The most records one read can return. */
    public int maxRecords() {
        return maxRecords;
    }

    /** The size of the payload buffer in bytes. */
    public long bufferBytes() {
        return bufferLength;
    }

    /** The number of records the last read returned (0 before the first read). */
    public int size() {
        return size;
    }

    /**
     * Starts a read into this batch: invalidates its views and empties it. Refuses while one of its views is inside
     * {@link RecordView#withPayload}, whose segment shows the batch's buffer: refilling would change the payload under
     * the action. Called before the reader reads anything, so a refused read consumes nothing.
     */
    void begin() {
        if (payloadActions != 0) {
            throw new IllegalStateException("cannot read into this batch while one of its views is inside withPayload"
                    + " (the action's segment shows the batch's buffer); read into another batch, or after the action");
        }
        generation.advance();
        size = 0;
    }

    /** A view of this batch enters {@link RecordView#withPayload}. */
    void enterPayloadAction() {
        payloadActions++;
    }

    /** A view of this batch leaves {@link RecordView#withPayload}. */
    void exitPayloadAction() {
        payloadActions--;
    }

    void filled(int count) {
        size = count;
    }

    /** The payload length the library stored in the first record (the size needed after a too-small buffer). */
    long firstPayloadLength() {
        return records.get(JAVA_LONG, OFF_LEN);
    }

    /**
     * The batch's view of record {@code index} of the last read, valid until the next read into this batch. The same
     * object for the same index every time, and a different one for each index; the first call for an index creates it
     * (once per batch), later calls allocate nothing.
     *
     * @throws IndexOutOfBoundsException if {@code index} is not below {@link #size()}
     */
    public RecordView get(int index) {
        Objects.checkIndex(index, size);
        RecordView view = views[index];
        if (view == null) {
            view = views[index] = RecordView.ofBatch(this, generation, buffer);
        }
        long base = index * RECORD_SIZE;
        view.set(records.get(JAVA_LONG, base + OFF_SEQ), records.get(JAVA_INT, base + OFF_SCHEMA),
                records.get(JAVA_LONG, base + OFF_VENUE), records.get(JAVA_LONG, base + OFF_LOCAL),
                records.get(JAVA_LONG, base + OFF_PAYLOAD), records.get(JAVA_LONG, base + OFF_LEN));
        return view;
    }

    /** Calls {@code action} with the view of each record of the last read ({@link #get(int)}), in order. */
    public void forEach(Consumer<? super RecordView> action) {
        for (int i = 0; i < size; i++) {
            action.accept(get(i));
        }
    }

    @Override
    public String toString() {
        return "ReadBatch[size=" + size + ", maxRecords=" + maxRecords + ", bufferBytes=" + bufferLength + "]";
    }
}
