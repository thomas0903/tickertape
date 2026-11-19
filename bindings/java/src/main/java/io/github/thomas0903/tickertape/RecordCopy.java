package io.github.thomas0903.tickertape;

import java.util.Arrays;
import java.util.HexFormat;

/**
 * A record copied out of the journal: independent of the reader, valid for as long as you keep it. Made by
 * {@link RecordView#copy()}, which gives it a new array of its own.
 *
 * <p><b>The payload array is shared, not copied.</b> The constructor stores the array it is given, and
 * {@link #payload()} returns that same array (no copy on either side, so a copy costs one array). Writing into it
 * changes this record, including {@link #equals}, which compares payloads by content, and {@link #hashCode}: do not
 * change the payload of a {@code RecordCopy} that is in a set or used as a map key, and do not pass an array to the
 * constructor that other code still writes. Copy it ({@code payload().clone()}) to change a payload safely.
 *
 * <p>Unsigned 64-bit and 32-bit fields of the format are returned as Java {@code long} and {@code int} with the same
 * bits (use {@link Long#toUnsignedString(long)} and {@link Integer#toUnsignedLong(int)} to print or widen them).
 *
 * @param seq the record's sequence number: its position in the journal, from 0, without gaps
 * @param schemaId the caller-defined schema identifier (unsigned 32-bit)
 * @param venueTsNs the venue timestamp as given at append (unsigned 64-bit; 0 means not set)
 * @param localTsNs the local timestamp as given at append (unsigned 64-bit; 0 means not set)
 * @param payload the payload bytes (shared, not copied: see above)
 */
public record RecordCopy(long seq, int schemaId, long venueTsNs, long localTsNs, byte[] payload) {
    /** Checks that {@code payload} is not null; stores the array itself, without a copy. */
    public RecordCopy {
        java.util.Objects.requireNonNull(payload, "payload");
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof RecordCopy r && seq == r.seq && schemaId == r.schemaId && venueTsNs == r.venueTsNs
                && localTsNs == r.localTsNs && Arrays.equals(payload, r.payload);
    }

    @Override
    public int hashCode() {
        return Long.hashCode(seq) * 31 + Arrays.hashCode(payload);
    }

    @Override
    public String toString() {
        String hex = HexFormat.of().formatHex(payload, 0, Math.min(payload.length, 32));
        return "RecordCopy[seq=" + Long.toUnsignedString(seq) + ", schemaId=" + Integer.toUnsignedString(schemaId)
                + ", venueTsNs=" + Long.toUnsignedString(venueTsNs) + ", localTsNs=" + Long.toUnsignedString(localTsNs)
                + ", payload=" + payload.length + " bytes " + hex + (payload.length > 32 ? "..." : "") + "]";
    }
}
