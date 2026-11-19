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
 * <p>The records stay in the batch until it is filled again. {@link #get(int)} returns the batch's one reusable
 * {@link RecordView}, positioned at that record; it is valid until the next read into this batch. The payloads are in
 * the batch's own memory, so they never point into the journal. Not thread-safe.
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
    final Generation generation = new Generation();
    private final RecordView view = new RecordView(generation, null);
    private int size;

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
    }

    private final long bufferLength;

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

    /** Starts a read into this batch: invalidates its views and empties it. */
    void begin() {
        generation.value++;
        size = 0;
    }

    void filled(int count) {
        size = count;
    }

    /** The payload length the library stored in the first record (the size needed after a too-small buffer). */
    long firstPayloadLength() {
        return records.get(JAVA_LONG, OFF_LEN);
    }

    /**
     * The batch's reusable view, positioned at record {@code index} of the last read. Each call repositions the same
     * object.
     *
     * @throws IndexOutOfBoundsException if {@code index} is not below {@link #size()}
     */
    public RecordView get(int index) {
        Objects.checkIndex(index, size);
        long base = index * RECORD_SIZE;
        view.set(records.get(JAVA_LONG, base + OFF_SEQ), records.get(JAVA_INT, base + OFF_SCHEMA),
                records.get(JAVA_LONG, base + OFF_VENUE), records.get(JAVA_LONG, base + OFF_LOCAL),
                records.get(JAVA_LONG, base + OFF_PAYLOAD), records.get(JAVA_LONG, base + OFF_LEN));
        return view;
    }

    /** Calls {@code action} with the view positioned at each record of the last read, in order. */
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
