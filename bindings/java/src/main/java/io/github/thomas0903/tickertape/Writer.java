package io.github.thomas0903.tickertape;

import static io.github.thomas0903.tickertape.Memory.BYTE;
import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.nio.file.Path;
import java.util.Objects;
import java.util.Optional;
import java.util.OptionalLong;

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
 * storage assumptions) only after a later {@link #checkpoint()} returns, or once a checkpoint requested with
 * {@link #requestCheckpoint()} has finished ({@link #checkpointProgress()}). {@link #close()} does not checkpoint.
 *
 * <p><b>Threads.</b> A writer is used by one thread at a time; it may be handed to another thread between calls (with
 * the usual happens-before, such as through an executor or a lock). A call that finds the writer in use by another
 * thread, or closed, throws {@link IllegalStateException} and does nothing; it never reaches the library. Blocking
 * calls ({@link #open}, {@link #checkpoint()}, an append that rolls to a new segment) pin a virtual thread to its
 * carrier while they run. A background checkpoint ({@link #requestCheckpoint()}) runs on a thread the library starts
 * and owns; while it runs, every method may be called (by the thread using the writer): appends continue,
 * {@link #checkpointProgress()}, {@link #nextSeq()}, {@link #metrics()} and {@link #lastRetentionError()} return at
 * once, {@link #requestCheckpoint()} queues or joins a request, and {@link #checkpoint()} and {@link #close()} wait
 * for it.
 *
 * <p><b>Allocation.</b> The writer reuses one scratch buffer, its output memory and one {@link PayloadBuffer}, so once
 * warm the appends allocate nothing on the Java heap under the conditions that {@code docs/java-api.md} &rarr;
 * Allocation lists (and that the binding's allocation test measures): among them, for an append whose payload is
 * copied through the scratch buffer (a {@code byte[]}, a heap {@link MemorySegment}, any {@link ByteBuffer}), a payload
 * that fits the buffer's current capacity, and for {@link PayloadBuffer#put(long, ByteBuffer)} of a direct or
 * read-only buffer, the same buffer as before. The capacity starts at 256 bytes. When a copied payload does not fit,
 * the new capacity is the larger of the payload and twice the old capacity, where the doubled figure is capped at
 * {@code Integer.MAX_VALUE - 8} bytes: with a capacity of 1,200,000,000 bytes, a 1,300,000,000-byte payload grows it
 * to {@code Integer.MAX_VALUE - 8} bytes, not 2,400,000,000. A payload above the cap grows it to exactly the payload's
 * size, and a buffer that large cannot back a {@link ByteBuffer}: a {@code byte[]} or heap segment above the cap is
 * copied into it segment to segment; a {@code ByteBuffer} above the cap (direct, heap or read-only, with
 * {@code Integer.MAX_VALUE - 7} to {@code Integer.MAX_VALUE} bytes remaining) is copied into it through a segment over
 * the buffer, which allocates that wrapper; and the next {@code ByteBuffer} append at or below the cap replaces it
 * with a buffer of that append's size. The record then reaches the library, which appends it or refuses it with its
 * usual {@link RecordTooLargeException}. A native segment is passed as it is and does not count.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
public final class Writer implements AutoCloseable {
    /** The upcall target, {@link #fill(FillContext, long, long, long)}, bound per writer. */
    private static final MethodHandle FILL;

    static {
        try {
            FILL = MethodHandles.lookup().findStatic(Writer.class, "fill", MethodType.methodType(int.class,
                    FillContext.class, long.class, long.class, long.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    private final NativeHandle handle;
    private final Path dir;
    private final MemorySegment seqOut = Memory.allocate(8);
    private final MemorySegment countOut = Memory.allocate(8);
    /** The scratch buffer's starting capacity in bytes; it grows when a copied payload does not fit (see scratch). */
    static final int SCRATCH_INITIAL_BYTES = 256;

    private MemorySegment scratch = Memory.allocate(SCRATCH_INITIAL_BYTES);
    private ByteBuffer scratchBuffer = scratch.asByteBuffer();

    private MemorySegment fillStub;
    private final FillContext fill = new FillContext();
    /**
     * The buffers {@link PayloadBuffer#put(long, ByteBuffer)} copied from. Only the writer references it, and it lends
     * it to the payload buffer for each {@link #appendWith} (see {@link BufferCache}).
     */
    private final BufferCache sources = new BufferCache();

    /**
     * What the upcall stub is bound to: the filler of the append in progress, what it threw, and the reusable payload
     * buffer. It must never reference the writer: the stub keeps its target reachable from a global root until the
     * stub's arena is closed, and the writer's Cleaner could otherwise never run. Nor may it reference anything the
     * caller passed in outside a fill (the filler, its error and the writer's {@link BufferCache} are all cleared when
     * the append returns), since that could refer to the writer.
     */
    private static final class FillContext {
        PayloadFiller filler;
        Throwable error;
        final PayloadBuffer buffer = new PayloadBuffer();

        int run(long buf, long len) {
            PayloadFiller f = filler;
            if (f == null) {
                return 1;
            }
            try {
                buffer.begin(buf, len);
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
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws LockedException if another writer has the journal open
     * @throws InvalidConfigException if an option is out of range
     * @throws CorruptJournalException if the journal is damaged in a way recovery does not repair, or the tail is
     *     corrupt and the policy is {@link RecoveryPolicy#FAIL}
     * @throws UnsupportedVersionException if a segment has an unsupported format version
     * @throws JournalIOException if an operating-system call fails
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
     * @throws JournalIOException if a roll fails (the journal is unchanged), or for every append after a failed
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
            if (scratchCopy(Source.BYTE_BUFFER, length) == ScratchCopy.BUFFER_SEGMENT_COPY) {
                // Above the cap no ByteBuffer view of the scratch can exist: copy segment to segment (allocates).
                MemorySegment.copy(MemorySegment.ofBuffer(payload), 0, buf, 0, length);
                return appendRaw(schemaId, venueTsNs, localTsNs, buf, length);
            }
            if (scratchBuffer == null) { // a heap segment or array grew the scratch beyond what a ByteBuffer can cover
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

    /**
     * The scratch buffer, grown first if {@code length} does not fit: to the larger of {@code length} and twice the
     * capacity, the doubled figure capped at {@link #SCRATCH_VIEW_CAP}; above the cap, to exactly {@code length}, with
     * no {@code ByteBuffer} view (a later {@code ByteBuffer} append at or below the cap then replaces it).
     */
    private MemorySegment scratch(long length) {
        if (length > scratch.byteSize()) {
            long size = Math.max(length, Math.min(scratch.byteSize() * 2, SCRATCH_VIEW_CAP));
            scratch = Memory.allocate(size);
            scratchBuffer = size <= SCRATCH_VIEW_CAP ? scratch.asByteBuffer() : null;
        }
        return scratch;
    }

    /**
     * The largest scratch buffer that has a {@code ByteBuffer} view: {@code MemorySegment.asByteBuffer} refuses a
     * larger segment. Also the cap on the doubled capacity.
     */
    static final long SCRATCH_VIEW_CAP = Integer.MAX_VALUE - 8;

    /** The kinds of payload an append copies through the scratch buffer. */
    enum Source {
        BYTE_ARRAY,
        HEAP_SEGMENT,
        /** A direct, heap or read-only heap {@code ByteBuffer}: all three are copied the same way. */
        BYTE_BUFFER
    }

    /** How a copied payload reaches the scratch buffer. */
    enum ScratchCopy {
        /** {@code MemorySegment.copy} from the array or segment into the scratch segment, at any size. */
        SEGMENT_COPY,
        /**
         * {@code ByteBuffer.put} into the scratch buffer's {@code ByteBuffer} view, without allocating once warm; a
         * scratch buffer without a view (grown above the cap by an array or heap segment) is first replaced by one of
         * the payload's size.
         */
        VIEW_PUT,
        /**
         * {@code MemorySegment.copy} from {@code MemorySegment.ofBuffer(payload)} into the scratch segment, grown to
         * exactly the payload's size if it is smaller: a {@code ByteBuffer} above {@link #SCRATCH_VIEW_CAP}, which no
         * {@code ByteBuffer} view of the scratch can cover. It allocates the wrapper segment on every call.
         */
        BUFFER_SEGMENT_COPY
    }

    /** The copy path for a payload of {@code source}'s kind and {@code length} bytes (see {@link ScratchCopy}). */
    static ScratchCopy scratchCopy(Source source, long length) {
        if (source != Source.BYTE_BUFFER) {
            return ScratchCopy.SEGMENT_COPY;
        }
        return length > SCRATCH_VIEW_CAP ? ScratchCopy.BUFFER_SEGMENT_COPY : ScratchCopy.VIEW_PUT;
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
     * {@link JournalIOException} instead, with the filler's exception as its cause: treat it as a storage incident
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
            fill.buffer.attach(sources);
            int status;
            try {
                status = Native.writerAppendWith(handle.ptr, schemaId, venueTsNs, localTsNs, length, stub,
                        MemorySegment.NULL, seqOut);
            } finally {
                fill.filler = null;
                fill.buffer.detach();
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
    private static int fill(FillContext context, long userData, long buf, long len) {
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
            // At most count records were appended (the C ABI's contract), so the count fits in an int.
            throw new BatchAppendException(cause, (int) Math.min(countOut.get(JAVA_LONG, 0), count),
                    seqOut.get(JAVA_LONG, 0));
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
     * this result. Background checkpoints in flight ({@link #requestCheckpoint()}) are waited for first, and the error
     * of one that failed is thrown here if no call has thrown it yet.
     *
     * @throws JournalIOException if a sync fails ({@link StorageFullException} for lack of space). The writer is then
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

    /**
     * Starts a checkpoint of every record appended so far on the library's checkpoint thread and returns without
     * waiting for its syncs (ADR 0038). Returns the sequence number it covers up to: once it has finished (see
     * {@link #checkpointProgress()}), every record with a smaller sequence number is durable, exactly as after
     * {@link #checkpoint()}. Records appended after this call are not covered; appends continue while it runs.
     *
     * <p>One checkpoint runs at a time: a request made while one runs waits for it, and requests made meanwhile join
     * the waiting one, which then covers up to the newest of them. {@link #checkpoint()} waits for the background
     * checkpoints in flight before it checkpoints itself, and {@link #close()} waits for them and reports how they
     * ended. Configured retention runs on the checkpoint thread after each successful background checkpoint; its
     * errors reach {@link #lastRetentionError()} at the writer's next call to this method, {@link #checkpointProgress()},
     * {@link #checkpoint()} or {@link #close()}. The first call starts the checkpoint thread; later calls make no system
     * call.
     *
     * @throws JournalIOException the error of an earlier background checkpoint (or of early write-back on Linux) that
     *     failed and that no call has thrown yet ({@link StorageFullException} for lack of space); thrown once, after
     *     which the writer is poisoned as after a failed {@link #checkpoint()}. Also if the checkpoint thread cannot be
     *     started (the writer is then not poisoned). Nothing is requested.
     * @throws DurabilityLostException if the writer is poisoned
     * @throws SequenceExhaustedException if checkpoint generations are exhausted
     * @throws IllegalStateException if the writer is closed or in use by another thread
     */
    public long requestCheckpoint() {
        handle.enter();
        try {
            Errors.check(Native.writerRequestCheckpoint(handle.ptr, seqOut));
            return seqOut.get(JAVA_LONG, 0);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Where this writer's checkpoints stand: the marker it last made durable (or read when it opened), and the
     * frontier of a background checkpoint still in flight. Makes no system call; allocates the result.
     *
     * @throws JournalIOException the error of a background checkpoint (or of early write-back on Linux) that failed,
     *     the first time it is reported; the writer is then poisoned
     * @throws DurabilityLostException whenever the writer is poisoned after that, also after a failed
     *     {@link #checkpoint()} or a failed clean-up of an aborted fill: a poisoned writer reports no progress
     * @throws IllegalStateException if the writer is closed or in use by another thread
     */
    public CheckpointProgress checkpointProgress() {
        handle.enter();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = MetricsValues.sized(arena, Signatures.CHECKPOINT_PROGRESS);
            Errors.check(Native.writerCheckpointProgress(handle.ptr, out));
            Optional<Checkpoint> durable = Optional.empty();
            if (out.get(JAVA_INT, field("has_durable")) != 0) {
                durable = Optional.of(new Checkpoint(out.get(JAVA_LONG, field("durable_generation")),
                        out.get(JAVA_LONG, field("durable_next_seq")),
                        out.get(JAVA_LONG, field("durable_segment_base_seq")),
                        out.get(JAVA_LONG, field("durable_offset"))));
            }
            OptionalLong pending = out.get(JAVA_INT, field("has_pending")) != 0
                    ? OptionalLong.of(out.get(JAVA_LONG, field("pending_next_seq")))
                    : OptionalLong.empty();
            return new CheckpointProgress(durable, pending);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    private static long field(String name) {
        return Signatures.CHECKPOINT_PROGRESS.byteOffset(MemoryLayout.PathElement.groupElement(name));
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
     * What only the writer knows (ADR 0032): its head, the last checkpoint by the monotonic clock, uncheckpointed
     * records and bytes, whether it is poisoned, and failed retention runs. Makes no system call; allocates the result.
     */
    public WriterMetrics metrics() {
        handle.enter();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = MetricsValues.sized(arena, Signatures.WRITER_METRICS);
            Errors.check(Native.writerGetMetrics(handle.ptr, out));
            return new WriterMetrics(new MetricsValues.Struct(out, Signatures.WRITER_METRICS));
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
     * Closes the writer and releases the journal's lock, as Rust's {@code Writer::close} does. Does <b>not</b>
     * checkpoint: appended records stay visible and survive, but only those covered by a successful checkpoint are
     * durable. If background checkpoints ({@link #requestCheckpoint()}) are queued or running, it first waits for them
     * (it does not cancel them), then reports how this writer's checkpoints ended; the journal is released in every
     * case. Idempotent, and callable from
     * any thread. From the moment it starts, the writer refuses every new call and {@link #isClosed()} is true. If
     * another thread is inside a call on this writer, waits for that call to finish, without spinning (an interrupt does
     * not end the wait, and the thread's interrupt status is kept); if another thread is closing it, waits until the
     * journal is released. So when this returns, on any thread, the journal can be opened again. Frees the upcall stub
     * of {@link #appendWith} too, which lives in a shared arena: closing a writer that used {@code appendWith} costs a
     * handshake with every thread of the JVM, as closing a reader does.
     *
     * @throws JournalIOException the error of a background checkpoint (or of early write-back on Linux) that failed and
     *     that no earlier call threw ({@link StorageFullException} for lack of space); the writer is closed all the same
     * @throws DurabilityLostException if the writer was poisoned: a checkpoint failed and its error was thrown earlier,
     *     or the clean-up of an aborted fill failed; the writer is closed all the same
     * @throws InternalErrorException if the library reported an internal error while closing (the handle is freed
     *     all the same)
     * @throws IllegalStateException if called from inside a {@link PayloadFiller} of this writer
     */
    @Override
    public void close() {
        try {
            TickertapeException error = handle.close();
            sources.clear(); // no call can run any more: let go of the caller's buffers
            if (error != null) {
                throw error;
            }
        } finally {
            Reference.reachabilityFence(this); // see NativeHandle: the Cleaner must not claim the clean-up meanwhile
        }
    }

    @Override
    public String toString() {
        return "Writer[" + dir + (isClosed() ? ", closed]" : "]");
    }
}
