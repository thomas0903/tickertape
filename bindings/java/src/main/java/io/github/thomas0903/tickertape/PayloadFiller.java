package io.github.thomas0903.tickertape;

/**
 * Writes a payload in place for {@link Writer#appendWith}. Called once, on the calling thread, with a zeroed
 * {@link PayloadBuffer} of exactly the requested length inside the journal's mapped segment.
 *
 * <p>Return normally to commit the record. Throw anything to abort the append: nothing is committed, the writer zeroes
 * and syncs the reserved bytes (slow: aborting is for exceptional cases), and {@code appendWith} throws a
 * {@link FillAbortedException} with your exception as its cause ({@link Error}s are rethrown as they are). The binding
 * catches every exception before it reaches the library, so no exception ever unwinds through native frames. A filler
 * must not keep the buffer, and must not call the writer that invoked it (that throws
 * {@link IllegalStateException}, which aborts the append).
 */
@FunctionalInterface
public interface PayloadFiller {
    /**
     * Fills {@code payload}.
     *
     * @param payload the record's payload, valid until this method returns
     * @throws Exception to abort the append
     */
    void fill(PayloadBuffer payload) throws Exception;
}
