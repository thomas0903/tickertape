package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.BYTE;
import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.Objects;
import java.util.Optional;

/**
 * Appends records to a journal. Only one writer may have a journal open at a time, across processes.
 *
 * <pre>{@code
 * try (Writer writer = Writer.open(Path.of("/data/journal"))) {
 *     long seq = writer.append(1, venueTs, Tickertape.realtimeNs(), payload);   // visible to readers now
 *     writer.checkpoint();                                                      // durable from here
 * }
 * }</pre>
 *
 * <p><b>Durability.</b> When an append returns, the record is <b>visible</b>: readers in any process on this host can
 * read it, and it survives this process dying. It is <b>durable</b> (survives a power loss, under the crash contract's
 * storage assumptions) only after a later {@link #checkpoint()} returns. {@link #close()} does not checkpoint.
 *
 * <p><b>Threads.</b> A writer is used by one thread at a time; it may be handed to another thread between calls (with
 * the usual happens-before, such as through an executor or a lock). A call that finds the writer in use by another
 * thread, or closed, throws {@link IllegalStateException} and does nothing; it never reaches the library. Blocking
 * calls ({@link #open}, {@link #checkpoint()}, an append that rolls to a new segment) pin a virtual thread to its
 * carrier while they run.
 *
 * <p><b>Allocation.</b> After warm-up, {@code append}, {@link #appendWith} and {@link #appendBatch} allocate nothing on
 * the Java heap: the writer reuses one scratch buffer, its output memory and one {@link PayloadBuffer}.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
public final class Writer implements AutoCloseable {
    /** The upcall target, {@link #fill(FillContext, MemorySegment, MemorySegment, long)}, bound per writer. */
    private static final MethodHandle FILL;

    static {
        try {
            FILL = MethodHandles.lookup().findStatic(Writer.class, "fill", MethodType.methodType(int.class,
                    FillContext.class, MemorySegment.class, MemorySegment.class, long.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    private final NativeHandle handle;
    private final Path dir;
    private final MemorySegment seqOut = Memory.allocate(8);
    private final MemorySegment countOut = Memory.allocate(8);
    private MemorySegment scratch = Memory.allocate(256);
    private ByteBuffer scratchBuffer = scratch.asByteBuffer();

    private MemorySegment fillStub;
    private final FillContext fill = new FillContext();

    /**
     * What the upcall stub is bound to: the filler of the append in progress, what it threw, and the reusable payload
     * buffer. It must never reference the writer: the stub keeps its target reachable from a global root until the
     * stub's arena is closed, and the writer's Cleaner could otherwise never run.
     */
    private static final class FillContext {
        PayloadFiller filler;
        Throwable error;
        final PayloadBuffer buffer = new PayloadBuffer();

        int run(MemorySegment buf, long len) {
            PayloadFiller f = filler;
            if (f == null) {
                return 1;
            }
            try {
                buffer.begin(buf.address(), len);
                f.fill(buffer);
                return 0;
            } catch (Throwable t) {
                error = t;
                return 1;
            } finally {
                buffer.end();
            }
        }
    }

    private Writer(Path dir, MemorySegment ptr) {
        this.dir = dir;
        this.handle = new NativeHandle(this, "writer", ptr, Native::writerClose, null);
    }

    /** Opens the journal in {@code dir} with the default options; see {@link #open(Path, WriterOptions)}. */
    public static Writer open(Path dir) {
        return open(dir, WriterOptions.defaults());
    }

    /**
     * Opens the journal in {@code dir} for writing, creating the directory and the first segment if needed. Takes the
     * journal's lock, then recovers it ({@code docs/format.md} &rarr; Recovery): everything after the last checkpoint
     * is validated, and anything invalid is repaired or quarantined before this returns.
     *
     * @throws JournalLockedException if another writer has the journal open
     * @throws InvalidConfigException if an option is out of range
     * @throws CorruptJournalException if the journal is damaged in a way recovery does not repair, or the tail is
     *     corrupt and the policy is {@link RecoveryPolicy#FAIL}
     * @throws UnsupportedFormatVersionException if a segment has an unsupported format version
     * @throws JournalIoException if an operating-system call fails
     * @throws UnsatisfiedLinkError if libtickertape cannot be found or is not compatible (see {@link Tickertape})
     */
    public static Writer open(Path dir, WriterOptions options) {
        Objects.requireNonNull(dir, "dir");
        Objects.requireNonNull(options, "options");
        NativeLibrary.lookup();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment path = Paths.toC(dir, arena);
            MemorySegment out = arena.allocate(ADDRESS);
            MemorySegment nativeOptions = options.toNative(arena);
            int status;
            TickertapeException error = null;
            try {
                status = Native.writerOpen(path, nativeOptions, out);
                if (status < 0) {
                    error = Errors.capture(status);
                }
            } finally {
                Native.writerOptionsFree(nativeOptions);
            }
            if (error != null) {
                throw error;
            }
            return new Writer(dir, out.get(ADDRESS, 0));
        }
    }

    /** The journal directory this writer was opened on. */
    public Path directory() {
        return dir;
    }

    /** Appends a record whose payload is all of {@code payload}; see {@link #append(int, long, long, MemorySegment)}. */
    public long append(int schemaId, long venueTsNs, long localTsNs, byte[] payload) {
        return append(schemaId, venueTsNs, localTsNs, payload, 0, payload.length);
    }

    /** Appends a record whose payload is {@code length} bytes of {@code payload} from {@code offset}. */
    public long append(int schemaId, long venueTsNs, long localTsNs, byte[] payload, int offset, int length) {
        Objects.checkFromIndexSize(offset, length, payload.length);
        handle.enter();
        try {
            MemorySegment buf = scratch(length);
            MemorySegment.copy(payload, offset, buf, BYTE, 0, length);
            return appendRaw(schemaId, venueTsNs, localTsNs, buf, length);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Appends one record and returns its sequence number. The payload is copied into the journal; a native segment is
     * passed to the library as it is, a heap segment is first copied into the writer's scratch buffer.
     *
     * <p>When this returns the record is visible, not durable (see the class documentation). If the record does not
     * fit in the current segment, the writer first rolls to a new one (slower: it creates and syncs a file).
     *
     * @param schemaId a caller-defined schema identifier (unsigned 32-bit), opaque to the library
     * @param venueTsNs a caller-supplied timestamp (by convention the venue's event time, ns since the Unix epoch;
     *     0 means not set), never interpreted by the library
     * @param localTsNs a caller-supplied timestamp (by convention the local receive time), never interpreted
     * @param payload the payload bytes
     * @return the record's sequence number
     * @throws RecordTooLargeException if the payload does not fit in one segment
     * @throws StorageFullException if a roll fails for lack of space (the journal is unchanged; retry later)
     * @throws JournalIoException if a roll fails (the journal is unchanged), or for every append after a failed
     *     clean-up of an aborted fill (retrying does not help)
     * @throws SequenceExhaustedException if sequence numbers are exhausted
     * @throws IllegalStateException if the writer is closed or in use by another thread
     */
    public long append(int schemaId, long venueTsNs, long localTsNs, MemorySegment payload) {
        long length = payload.byteSize();
        handle.enter();
        try {
            if (payload.isNative()) {
                return appendRaw(schemaId, venueTsNs, localTsNs, payload, length);
            }
            MemorySegment buf = scratch(length);
            MemorySegment.copy(payload, 0, buf, 0, length);
            return appendRaw(schemaId, venueTsNs, localTsNs, buf, length);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Appends a record whose payload is the bytes between {@code payload}'s position and limit. The buffer's position
     * is not changed.
     */
    public long append(int schemaId, long venueTsNs, long localTsNs, ByteBuffer payload) {
        int length = payload.remaining();
        handle.enter();
        try {
            MemorySegment buf = scratch(length);
            if (scratchBuffer == null) { // the scratch buffer grew beyond what a ByteBuffer can cover
                buf = scratch = Memory.allocate(length);
                scratchBuffer = scratch.asByteBuffer();
            }
            scratchBuffer.put(0, payload, payload.position(), length);
            return appendRaw(schemaId, venueTsNs, localTsNs, buf, length);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    private MemorySegment scratch(long length) {
        if (length > scratch.byteSize()) {
            long size = Math.max(length, Math.min(scratch.byteSize() * 2, Integer.MAX_VALUE - 8));
            scratch = Memory.allocate(size);
            scratchBuffer = size <= Integer.MAX_VALUE - 8 ? scratch.asByteBuffer() : null;
        }
        return scratch;
    }

    private long appendRaw(int schemaId, long venueTsNs, long localTsNs, MemorySegment payload, long length) {
        int status = Native.writerAppend(handle.ptr, schemaId, venueTsNs, localTsNs, payload, length, seqOut);
        if (status < 0) {
            throw Errors.capture(status);
        }
        return seqOut.get(JAVA_LONG, 0);
    }

    /**
     * Appends one record of {@code length} bytes whose payload {@code filler} writes in place inside the mapped
     * segment, without a copy, and returns its sequence number. Durability is as for
     * {@link #append(int, long, long, MemorySegment)}.
     *
     * <p>{@code filler} is called once, on this thread, through an upcall from the library. If it returns normally the
     * record is committed. If it throws, nothing is committed: the writer zeroes the reserved bytes and syncs that to
     * storage (slow; aborting is for exceptional cases), and this method throws {@link FillAbortedException} with the
     * filler's exception as its cause (an {@link Error} is rethrown as it is). If that sync fails, it throws
     * {@link JournalIoException} instead, with the filler's exception as its cause: treat it as a storage incident
     * like a failed checkpoint; the writer then refuses every further append and checkpoint (see
     * {@code docs/c-api.md} &rarr; A failed clean-up).
     *
     * <p>{@code filler} and the {@link PayloadBuffer} it receives belong to this thread: handing the buffer to another
     * thread makes every use there throw {@link WrongThreadException} (rethrow it from the filler to abort).
     *
     * <p>The first call on a writer creates its upcall stub (once per writer); {@link #close()} frees it.
     *
     * @throws FillAbortedException if the filler threw
     * @throws IllegalStateException if the writer is closed or in use by another thread (including a call from inside
     *     the filler, which then aborts)
     */
    public long appendWith(int schemaId, long venueTsNs, long localTsNs, long length, PayloadFiller filler) {
        Objects.requireNonNull(filler, "filler");
        if (length < 0) {
            throw new IllegalArgumentException("length must not be negative: " + length);
        }
        handle.enter();
        try {
            MemorySegment stub = fillStub();
            fill.filler = filler;
            fill.error = null;
            int status;
            try {
                status = Native.writerAppendWith(handle.ptr, schemaId, venueTsNs, localTsNs, length, stub,
                        MemorySegment.NULL, seqOut);
            } finally {
                fill.filler = null;
            }
            Throwable thrown = fill.error;
            fill.error = null;
            if (status >= 0) {
                return seqOut.get(JAVA_LONG, 0);
            }
            TickertapeException error = Errors.capture(status, -1, thrown);
            if (thrown instanceof Error e && error instanceof FillAbortedException) {
                throw e;
            }
            throw error;
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * The writer's upcall stub, created on first use in a shared arena that the handle closes after the C writer is
     * freed (by {@link #close()} or the Cleaner), bound to {@link #fill} and not to the writer.
     */
    private MemorySegment fillStub() {
        if (fillStub == null) {
            Arena arena = Arena.ofShared();
            try {
                fillStub = Linker.nativeLinker().upcallStub(FILL.bindTo(fill), Signatures.FILL_FN, arena);
            } catch (RuntimeException | Error e) {
                arena.close();
                throw e;
            }
            handle.closeAfterFree(arena);
        }
        return fillStub;
    }

    /**
     * The upcall target: runs the filler and turns anything it throws into an abort. Nothing may escape an upcall (the
     * JVM would exit), so everything, the payload buffer's clean-up included, runs inside a catch of
     * {@link Throwable}, and so does recording what was caught.
     */
    @SuppressWarnings("unused") // called from native code through FILL
    private static int fill(FillContext context, MemorySegment userData, MemorySegment buf, long len) {
        try {
            return context.run(buf, len);
        } catch (Throwable t) {
            try {
                if (context.error == null) {
                    context.error = t;
                }
            } catch (Throwable ignored) {
                // nothing more can be done; the append is aborted all the same
            }
            return 1;
        }
    }

    /**
     * Appends every record of {@code batch} in order with one call into the library and returns the first record's
     * sequence number (the others follow without gaps). The batch is not cleared.
     *
     * <p>Every record is checked first: an argument error appends nothing. Then the records are appended one by one;
     * if one fails, the records before it stay appended and this throws {@link BatchAppendException}, which says how
     * many were appended. Durability is as for {@link #append(int, long, long, MemorySegment)}.
     *
     * @throws BatchAppendException if the library rejected the batch or an append failed part-way
     * @throws IllegalStateException if the writer is closed or in use by another thread
     */
    public long appendBatch(AppendBatch batch) {
        Objects.requireNonNull(batch, "batch");
        handle.enter();
        // The pointers prepare() writes point into this memory: keep exactly this object reachable during the call.
        MemorySegment payloads = batch.payloadMemory();
        try {
            MemorySegment records = batch.prepare();
            int count = batch.size();
            seqOut.set(JAVA_LONG, 0, 0);
            countOut.set(JAVA_LONG, 0, 0);
            int status = Native.writerAppendBatch(handle.ptr, records, count, countOut, seqOut);
            if (status >= 0) {
                return seqOut.get(JAVA_LONG, 0);
            }
            TickertapeException cause = Errors.capture(status);
            throw new BatchAppendException(cause, countOut.get(JAVA_LONG, 0), seqOut.get(JAVA_LONG, 0));
        } finally {
            handle.exit();
            Reference.reachabilityFence(payloads);
            Reference.reachabilityFence(batch);
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Makes every record appended so far <b>durable</b>, with the platform's sync calls, and blocks until they finish.
     * Configured retention then runs; its failures are reported by {@link #lastRetentionError()} and do not change
     * this result.
     *
     * @throws JournalIoException if a sync fails ({@link StorageFullException} for lack of space). The writer is then
     *     poisoned: every later checkpoint throws {@link DurabilityLostException}, while appends keep working. Treat a
     *     checkpoint error as an incident: records written before it may not be durable even if a later writer's
     *     checkpoint succeeds.
     * @throws DurabilityLostException after an earlier failed checkpoint or a failed clean-up of an aborted fill
     */
    public void checkpoint() {
        handle.enter();
        try {
            Errors.check(Native.writerCheckpoint(handle.ptr));
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /** The sequence number the next append will get. */
    public long nextSeq() {
        handle.enter();
        try {
            Errors.check(Native.writerNextSeq(handle.ptr, seqOut));
            return seqOut.get(JAVA_LONG, 0);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * The most recent retention error since the writer was opened, as an exception value (not thrown), or empty if
     * there was none. Retention errors never fail open, appends or checkpoints, and a later successful run does not
     * clear the error.
     */
    public Optional<TickertapeException> lastRetentionError() {
        handle.enter();
        try {
            int status = Native.writerLastRetentionError(handle.ptr);
            return status < 0 ? Optional.of(Errors.capture(status)) : Optional.empty();
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /** Whether {@link #close()} has started (from then on the writer refuses every call). */
    public boolean isClosed() {
        return handle.isClosed();
    }

    /**
     * Closes the writer and releases the journal's lock. Does <b>not</b> checkpoint: appended records stay visible and
     * survive, but only those covered by a successful {@link #checkpoint()} are durable. Idempotent, and callable from
     * any thread. If another thread is inside a call on this writer, waits for that call to finish; if another thread
     * is closing it, waits until the journal is released. So when this returns, on any thread, the journal can be
     * opened again. Frees the upcall stub of {@link #appendWith} too.
     *
     * @throws InternalErrorException if the library reported an internal error while closing (the handle is freed
     *     all the same)
     * @throws IllegalStateException if called from inside a {@link PayloadFiller} of this writer
     */
    @Override
    public void close() {
        int status = handle.close();
        if (status < 0) {
            throw Errors.capture(status);
        }
    }

    @Override
    public String toString() {
        return "Writer[" + dir + (isClosed() ? ", closed]" : "]");
    }
}
