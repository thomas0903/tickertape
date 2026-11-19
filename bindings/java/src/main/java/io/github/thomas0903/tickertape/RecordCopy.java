package io.github.thomas0903.tickertape;

import java.util.Arrays;
import java.util.HexFormat;

/**
 * A record copied out of the journal: independent of the reader, valid for as long as you keep it. Made by
 * {@link RecordView#copy()}; the payload array belongs to this value and is not copied again by {@link #payload()}.
 *
 * <p>Unsigned 64-bit and 32-bit fields of the format are returned as Java {@code long} and {@code int} with the same
 * bits (use {@link Long#toUnsignedString(long)} and {@link Integer#toUnsignedLong(int)} to print or widen them).
 *
 * @param seq the record's sequence number: its position in the journal, from 0, without gaps
 * @param schemaId the caller-defined schema identifier (unsigned 32-bit)
 * @param venueTsNs the venue timestamp as given at append (unsigned 64-bit; 0 means not set)
 * @param localTsNs the local timestamp as given at append (unsigned 64-bit; 0 means not set)
 * @param payload the payload bytes
 */
public record RecordCopy(long seq, int schemaId, long venueTsNs, long localTsNs, byte[] payload) {
    /** Checks that {@code payload} is not null. */
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
