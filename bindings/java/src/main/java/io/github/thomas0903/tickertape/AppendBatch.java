package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.BYTE;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;
import java.util.Arrays;
import java.util.Objects;

/**
 * Records collected for one {@link Writer#appendBatch} call, which appends them in order with one call into the
 * library. Payloads are copied into the batch's own native memory when added, so the caller's arrays and buffers can
 * be reused at once. Reuse a batch with {@link #clear()}: once it has grown to its working size, adding and appending
 * allocate nothing. Not thread-safe.
 *
 * <pre>{@code
 * AppendBatch batch = new AppendBatch();
 * for (Quote q : quotes) {
 *     batch.add(QUOTE, q.venueTs(), Tickertape.realtimeNs(), q.encoded());
 * }
 * long first = writer.appendBatch(batch);
 * batch.clear();
 * }</pre>
 */
public final class AppendBatch {
    private static final long RECORD_SIZE = Signatures.NEW_RECORD.byteSize();
    private static final long OFF_PAYLOAD = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("payload"));
    private static final long OFF_LEN = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("payload_len"));
    private static final long OFF_VENUE = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("venue_ts_ns"));
    private static final long OFF_LOCAL = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("local_ts_ns"));
    private static final long OFF_SCHEMA = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("schema_id"));
    private static final long OFF_RESERVED = Signatures.NEW_RECORD.byteOffset(PathElement.groupElement("reserved"));

    private MemorySegment records;
    private MemorySegment payloads;
    private long[] offsets;
    private int count;
    private long used;

    /** A batch with room for 64 records and 64 KiB of payload to start with; it grows as needed. */
    public AppendBatch() {
        this(64, 64 * 1024);
    }

    /** A batch with room for {@code records} records and {@code payloadBytes} bytes of payload; it grows as needed. */
    public AppendBatch(int records, long payloadBytes) {
        if (records < 1 || payloadBytes < 0) {
            throw new IllegalArgumentException("records must be positive and payloadBytes not negative");
        }
        this.records = Memory.allocate(records * RECORD_SIZE);
        this.payloads = Memory.allocate(payloadBytes);
        this.offsets = new long[records];
    }

    /** The number of records added since the last {@link #clear()}. */
    public int size() {
        return count;
    }

    /** The total payload bytes added since the last {@link #clear()}. */
    public long payloadBytes() {
        return used;
    }

    /** Removes every record, keeping the memory for reuse. */
    public void clear() {
        count = 0;
        used = 0;
    }

    /** Adds a record whose payload is all of {@code payload}. */
    public AppendBatch add(int schemaId, long venueTsNs, long localTsNs, byte[] payload) {
        return add(schemaId, venueTsNs, localTsNs, payload, 0, payload.length);
    }

    /** Adds a record whose payload is {@code length} bytes of {@code payload} from {@code offset}. */
    public AppendBatch add(int schemaId, long venueTsNs, long localTsNs, byte[] payload, int offset, int length) {
        Objects.checkFromIndexSize(offset, length, payload.length);
        long at = reserve(length);
        MemorySegment.copy(payload, offset, payloads, BYTE, at, length);
        return entry(schemaId, venueTsNs, localTsNs, at, length);
    }

    /** Adds a record whose payload is all of {@code payload} (heap or native). */
    public AppendBatch add(int schemaId, long venueTsNs, long localTsNs, MemorySegment payload) {
        long length = payload.byteSize();
        long at = reserve(length);
        MemorySegment.copy(payload, 0, payloads, at, length);
        return entry(schemaId, venueTsNs, localTsNs, at, length);
    }

    /** Adds a record whose payload is the bytes between {@code payload}'s position and limit; the buffer is not changed. */
    public AppendBatch add(int schemaId, long venueTsNs, long localTsNs, ByteBuffer payload) {
        int length = payload.remaining();
        long at = reserve(length);
        if (payload.hasArray()) {
            MemorySegment.copy(payload.array(), payload.arrayOffset() + payload.position(), payloads, BYTE, at, length);
        } else {
            MemorySegment.copy(MemorySegment.ofBuffer(payload), 0, payloads, at, length);
        }
        return entry(schemaId, venueTsNs, localTsNs, at, length);
    }

    private long reserve(long length) {
        if (count == offsets.length) {
            int n = Math.multiplyExact(offsets.length, 2);
            MemorySegment bigger = Memory.allocate(n * RECORD_SIZE);
            bigger.copyFrom(records.asSlice(0, count * RECORD_SIZE));
            records = bigger;
            offsets = Arrays.copyOf(offsets, n);
        }
        long need = Math.addExact(used, length);
        if (need > payloads.byteSize()) {
            long size = Math.max(need, Math.multiplyExact(payloads.byteSize(), 2));
            MemorySegment bigger = Memory.allocate(size);
            MemorySegment.copy(payloads, 0, bigger, 0, used);
            payloads = bigger;
        }
        long at = used;
        used = need;
        return at;
    }

    private AppendBatch entry(int schemaId, long venueTsNs, long localTsNs, long at, long length) {
        long base = count * RECORD_SIZE;
        records.set(JAVA_LONG, base + OFF_LEN, length);
        records.set(JAVA_LONG, base + OFF_VENUE, venueTsNs);
        records.set(JAVA_LONG, base + OFF_LOCAL, localTsNs);
        records.set(JAVA_INT, base + OFF_SCHEMA, schemaId);
        records.set(JAVA_INT, base + OFF_RESERVED, 0);
        offsets[count++] = at;
        return this;
    }

    /** Writes each record's payload pointer (the payload memory may have moved since it was added). */
    MemorySegment prepare() {
        long base = payloads.address();
        for (int i = 0; i < count; i++) {
            // A pointer is 8 bytes on every supported platform (checked at load); writing it as a long allocates nothing.
            records.set(JAVA_LONG, i * RECORD_SIZE + OFF_PAYLOAD, base + offsets[i]);
        }
        return records;
    }

    /** Keeps the payload memory reachable during the call that uses the pointers {@link #prepare()} wrote. */
    MemorySegment payloadMemory() {
        return payloads;
    }
}
