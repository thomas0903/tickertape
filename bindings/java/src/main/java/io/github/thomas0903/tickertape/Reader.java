package io.github.thomas0903.tickertape;

import static java.lang.foreign.ValueLayout.ADDRESS;
import static java.lang.foreign.ValueLayout.JAVA_INT;
import static java.lang.foreign.ValueLayout.JAVA_LONG;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout.PathElement;
import java.lang.foreign.MemorySegment;
import java.lang.ref.Reference;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Objects;
import java.util.concurrent.TimeUnit;

/**
 * Reads a journal: replays it from any sequence number and follows it live. Readers take no locks and never slow the
 * writer; any number can run at once, in any process on the host. Every record is validated (length, CRC32C,
 * sequence number) before it is returned.
 *
 * <pre>{@code
 * try (Reader reader = Reader.open(Path.of("/data/journal"))) {
 *     reader.seek(1000);
 *     RecordView r;
 *     while ((r = reader.next(1, TimeUnit.SECONDS)) != null) {
 *         handle(r.seq(), r.get(ValueLayout.JAVA_LONG_UNALIGNED, 0));   // valid until the next call
 *     }
 * }
 * }</pre>
 *
 * <p><b>Two readers.</b> {@link #open(Path, ReaderOptions)}, the safe reader, reads the files with {@code pread} into a
 * buffer it owns: it is memory-safe whatever other processes do to the files. {@link #openMappedUnsafe} returns
 * payloads straight from a memory mapping, without a copy, under a precondition the binding cannot check; read its
 * documentation before using it.
 *
 * <p><b>Records.</b> {@link #tryNext()} and the {@code next} methods return a borrowed {@link RecordView}, the same
 * object every time, valid until the next call on this reader; or {@code null} when no record arrived in time.
 * {@link #readBatch} copies many records into a {@link ReadBatch} at once. Neither allocates per record.
 *
 * <p><b>Errors never lose records.</b> When a read throws, nothing was consumed: the next read returns the record that
 * was next before the failing call. A read that returns records does not throw for an error that happened after them
 * (a batch read returns what it read; the error is thrown by the next read).
 *
 * <p><b>Interruption.</b> Blocking reads wait in slices of at most {@link ReaderOptions#waitSlice(Duration)} (10 ms by
 * default). Between slices they check the thread's interrupt status and throw {@link InterruptedException} (clearing
 * it), and notice a {@link #close()} from another thread. A record that arrives during a slice is returned at once,
 * even if the thread was interrupted meanwhile; the interrupt status then stays set for the caller to see.
 *
 * <p><b>Threads.</b> A reader and its views are used by one thread at a time; they may be handed to another thread
 * between calls (with the usual happens-before). A call that finds the reader in use by another thread, or closed,
 * throws {@link IllegalStateException} and does nothing. {@link #close()} may be called from any thread, also while
 * another thread blocks in a read or reads the current view: from the moment it starts it refuses every new call, it
 * waits for a call in progress (a blocked read gives up within one wait slice), ends every access to the view (which
 * then throws {@link IllegalStateException}), and then frees the reader. Closing costs a handshake with every thread of
 * the JVM (see {@link #close()}): keep readers open rather than opening short-lived ones. A blocking read pins a virtual thread to its carrier for the whole wait; follow live journals from
 * platform threads, or poll with {@link #tryNext()} and park between polls.
 */
public final class Reader implements AutoCloseable {
    private static final long OFF_SEQ = off("seq");
    private static final long OFF_VENUE = off("venue_ts_ns");
    private static final long OFF_LOCAL = off("local_ts_ns");
    private static final long OFF_PAYLOAD = off("payload");
    private static final long OFF_LEN = off("payload_len");
    private static final long OFF_SCHEMA = off("schema_id");
    private static final int OK = 0;
    private static final int NONE = 1;
    /** Waits longer than this (about 146 years) are treated as waiting forever. */
    private static final long FOREVER_NS = Long.MAX_VALUE / 2;

    private static long off(String name) {
        return Signatures.RECORD.byteOffset(PathElement.groupElement(name));
    }

    private final NativeHandle handle;
    private final Path dir;
    private final boolean mapped;
    private final long waitSliceNs;
    private final Generation generation = new Generation();
    private final RecordView view;
    private final MemorySegment record = Memory.allocate(Signatures.RECORD.byteSize());
    private final MemorySegment u64Out = Memory.allocate(8);

    private Reader(Path dir, MemorySegment ptr, boolean mapped, long waitSliceNs) {
        this.dir = dir;
        this.mapped = mapped;
        this.waitSliceNs = waitSliceNs;
        // The view reads the library's memory only through this arena, which close() (or the Cleaner) closes before
        // it frees the reader: closing a shared arena waits for every access in progress on any thread.
        Arena memoryArena = Arena.ofShared();
        this.view = RecordView.ofReader(this, generation, Memory.overAddressSpace(memoryArena));
        this.handle = new NativeHandle(this, "reader", ptr, Native::readerClose, memoryArena);
    }

    /** Opens the safe reader on {@code dir} with the default options; see {@link #open(Path, ReaderOptions)}. */
    public static Reader open(Path dir) {
        return open(dir, ReaderOptions.defaults());
    }

    /**
     * Opens the safe reader on the journal in {@code dir}, positioned at the first record of the oldest segment. It
     * reads the files with {@code pread} into a buffer it owns and validates the copy, so it is memory-safe whatever
     * other processes do to the files; at worst it reports corruption.
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     * @throws JournalIOException with {@code ENOENT} if {@code dir} does not exist (a reader that starts before its
     *     writer sees this, since the writer creates the directory)
     * @throws JournalNotFoundException if {@code dir} has no segment yet (the writer is creating it). A loop that waits
     *     for the writer retries on exactly these two.
     * @throws CorruptJournalException if the oldest segment's header is invalid
     * @throws UnsupportedVersionException if it has an unsupported format version
     */
    public static Reader open(Path dir, ReaderOptions options) {
        return open(dir, options, false);
    }

    /** Opens the mapped reader with the default options; see {@link #openMappedUnsafe(Path, ReaderOptions)}. */
    public static Reader openMappedUnsafe(Path dir) {
        return openMappedUnsafe(dir, ReaderOptions.defaults());
    }

    /**
     * Opens a reader whose records point directly into the memory-mapped segment files, without a copy.
     *
     * <p><b>Precondition: while this reader exists, no process other than a tickertape writer may modify the
     * journal's segment files</b>: no writing into them, truncating them, or replacing their contents with other
     * tools. A tickertape writer never modifies committed bytes, including during recovery, so this holds in normal
     * operation. If it is broken, payloads can change under you, and if a mapped file is truncated the JVM receives
     * {@code SIGBUS} and dies. The binding cannot check this. Use {@link #open(Path, ReaderOptions)} when you cannot
     * rule it out (for example, for journals on shared or untrusted storage).
     *
     * <p>Errors are as for {@link #open(Path, ReaderOptions)}.
     */
    public static Reader openMappedUnsafe(Path dir, ReaderOptions options) {
        return open(dir, options, true);
    }

    private static Reader open(Path dir, ReaderOptions options, boolean mapped) {
        Objects.requireNonNull(dir, "dir");
        Objects.requireNonNull(options, "options");
        NativeLibrary.lookup();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment path = Paths.toC(dir, arena);
            MemorySegment out = arena.allocate(ADDRESS);
            MemorySegment nativeOptions = options.toNative();
            int status;
            TickertapeException error = null;
            try {
                status = mapped ? Native.readerOpenMappedUnsafe(path, nativeOptions, out)
                        : Native.readerOpen(path, nativeOptions, out);
                if (status < 0) {
                    error = Errors.capture(status);
                }
            } finally {
                Native.readerOptionsFree(nativeOptions);
            }
            if (error != null) {
                throw error;
            }
            return new Reader(dir, out.get(ADDRESS, 0), mapped, options.waitSliceNanos());
        }
    }

    /** The journal directory this reader was opened on. */
    public Path directory() {
        return dir;
    }

    /** Whether this is the mapped reader from {@link #openMappedUnsafe}. */
    public boolean isMapped() {
        return mapped;
    }

    /**
     * Returns the next record if one is committed (for a durable-only reader: covered by a checkpoint), without
     * waiting; otherwise {@code null}. Follows seals into the next segment transparently.
     *
     * @return the reader's view of the record, valid until the next call on this reader, or {@code null}
     * @throws CorruptJournalException if the bytes at the position fail validation (the position does not move, so
     *     calling again retries it, which may succeed after a writer recovery)
     * @throws DiscardedException if writer recovery or retention replaced or removed the segment at the
     *     position; call {@link #seek} to continue
     * @throws JournalIOException if reading the files fails
     * @throws IllegalStateException if the reader is closed or in use by another thread
     */
    public RecordView tryNext() {
        handle.enter();
        try {
            generation.advance();
            return result(Native.readerNext(handle.ptr, 0, record));
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Returns the next record, waiting up to {@code timeout} for one to be committed; {@code null} if none arrived in
     * time. A zero or negative timeout does not wait. Errors are as for {@link #tryNext()}.
     *
     * <p>The timeout bounds only the wait for a record: catching up after a seek (every skipped record is validated)
     * and file I/O are not interrupted, so a call can take longer.
     *
     * @throws InterruptedException if the thread was interrupted before or while waiting (nothing was consumed)
     */
    public RecordView next(long timeout, TimeUnit unit) throws InterruptedException {
        long ns = unit.toNanos(timeout);
        return await(Math.max(ns, 0));
    }

    /** As {@link #next(long, TimeUnit)}, with a {@link Duration}. */
    public RecordView next(Duration timeout) throws InterruptedException {
        long ns;
        try {
            ns = timeout.toNanos();
        } catch (ArithmeticException e) {
            ns = timeout.isNegative() ? 0 : Long.MAX_VALUE;
        }
        return await(Math.max(ns, 0));
    }

    /**
     * Returns the next record, waiting as long as it takes. Never returns {@code null}. Errors are as for
     * {@link #tryNext()}.
     *
     * @throws InterruptedException if the thread was interrupted before or while waiting (nothing was consumed)
     * @throws IllegalStateException if the reader was closed by another thread while waiting
     */
    public RecordView next() throws InterruptedException {
        return await(Long.MAX_VALUE);
    }

    private RecordView await(long timeoutNs) throws InterruptedException {
        if (Thread.interrupted()) {
            throw new InterruptedException();
        }
        handle.enter();
        try {
            generation.advance();
            boolean forever = timeoutNs >= FOREVER_NS;
            long deadline = forever ? 0 : System.nanoTime() + timeoutNs;
            for (;;) {
                long slice = waitSliceNs;
                if (!forever) {
                    slice = Math.max(0, Math.min(deadline - System.nanoTime(), waitSliceNs));
                }
                int status = Native.readerNext(handle.ptr, slice, record);
                if (status != NONE) {
                    return result(status);
                }
                waited();
                if (!forever && deadline - System.nanoTime() <= 0) {
                    return null;
                }
            }
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /** After a slice that returned nothing: honours an interrupt or a close request. */
    private void waited() throws InterruptedException {
        if (Thread.interrupted()) {
            throw new InterruptedException();
        }
        if (handle.closeRequested()) {
            throw new IllegalStateException("the reader was closed by another thread while waiting");
        }
    }

    private RecordView result(int status) {
        if (status == OK) {
            view.set(record.get(JAVA_LONG, OFF_SEQ), record.get(JAVA_INT, OFF_SCHEMA), record.get(JAVA_LONG, OFF_VENUE),
                    record.get(JAVA_LONG, OFF_LOCAL), record.get(JAVA_LONG, OFF_PAYLOAD), record.get(JAVA_LONG, OFF_LEN));
            return view;
        }
        if (status == NONE) {
            return null;
        }
        throw Errors.capture(status);
    }

    /** Reads records into {@code batch} without waiting; see {@link #readBatch(ReadBatch, long, TimeUnit)}. */
    public int tryReadBatch(ReadBatch batch) {
        Objects.requireNonNull(batch, "batch");
        handle.enter();
        try {
            batch.begin();
            return batchResult(batch, readBatch(batch, 0));
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * Reads up to {@link ReadBatch#maxRecords()} records into {@code batch} and returns how many (also
     * {@link ReadBatch#size()}). Waits up to {@code timeout} for the first record only, then takes only records that
     * are already committed; stops early at a record whose payload does not fit in the rest of the batch's buffer
     * (it is returned first by the next read). Returns 0 if no record arrived in time.
     *
     * <p>An error before the first record is thrown. An error after one or more records is not: this returns the
     * records read so far, and the next read on this reader throws the error (a successful {@link #seek} discards
     * it). No record handed out is ever lost to an error.
     *
     * @throws BufferTooSmallException if the first record's payload does not fit in the batch's buffer;
     *     {@link BufferTooSmallException#requiredBytes()} says how large it is, and it stays next
     * @throws InterruptedException if the thread was interrupted before or while waiting (nothing was consumed)
     */
    public int readBatch(ReadBatch batch, long timeout, TimeUnit unit) throws InterruptedException {
        Objects.requireNonNull(batch, "batch");
        long timeoutNs = Math.max(unit.toNanos(timeout), 0);
        if (Thread.interrupted()) {
            throw new InterruptedException();
        }
        handle.enter();
        try {
            batch.begin();
            boolean forever = timeoutNs >= FOREVER_NS;
            long deadline = forever ? 0 : System.nanoTime() + timeoutNs;
            for (;;) {
                long slice = waitSliceNs;
                if (!forever) {
                    slice = Math.max(0, Math.min(deadline - System.nanoTime(), waitSliceNs));
                }
                int status = readBatch(batch, slice);
                if (status != NONE) {
                    return batchResult(batch, status);
                }
                waited();
                if (!forever && deadline - System.nanoTime() <= 0) {
                    return 0;
                }
            }
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    private int readBatch(ReadBatch batch, long timeoutNs) {
        generation.advance();
        return Native.readerReadBatch(handle.ptr, timeoutNs, batch.records, batch.maxRecords(), batch.buffer,
                batch.bufferBytes(), u64Out);
    }

    private int batchResult(ReadBatch batch, int status) {
        if (status == OK) {
            int count = (int) u64Out.get(JAVA_LONG, 0);
            batch.filled(count);
            return count;
        }
        if (status == NONE) {
            return 0;
        }
        long required = status == Status.BUFFER_TOO_SMALL.code() ? batch.firstPayloadLength() : -1;
        throw Errors.capture(status, required, null);
    }

    /**
     * Positions the reader so that the next record returned is {@code seq}. If {@code seq} has not been written yet,
     * reads return nothing until the writer gets there. Every record on the way is validated by the reads that
     * follow. A successful seek also discards a record held back and an error deferred by {@link #readBatch}; a
     * failed seek leaves the reader as it was.
     *
     * @throws SeqUnavailableException if {@code seq} is older than the oldest segment (retention removed it);
     *     seek to the oldest segment's {@link SegmentInfo#baseSeq()} from {@link Journal#listSegments} to continue
     * @throws CorruptJournalException if a segment header is invalid while locating it
     * @throws JournalIOException if reading the files fails while locating it
     */
    public void seek(long seq) {
        handle.enter();
        try {
            generation.advance();
            Errors.check(Native.readerSeek(handle.ptr, seq));
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /** The sequence number of the next record this reader will return. Does not invalidate the current view. */
    public long position() {
        handle.enter();
        try {
            Errors.check(Native.readerPosition(handle.ptr, u64Out));
            return u64Out.get(JAVA_LONG, 0);
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    /**
     * This reader's cursor and its lag behind {@code status} (behind the head, or behind the checkpoint marker for a
     * durable-only reader), as of that sample (ADR 0032): take the sample after the reads you want counted. If the
     * reader's segment is not the file the sample listed under that name, the lag is empty with the reason. Makes no
     * system call, allocates the result, and does not invalidate the current view.
     *
     * @throws IllegalStateException if {@code status} is closed, or as for every call on a closed or busy reader
     */
    public ReaderMetrics metrics(JournalStatus status) {
        Objects.requireNonNull(status, "status");
        handle.enter();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = MetricsValues.sized(arena, Signatures.READER_METRICS);
            Errors.check(status.withHandle(s -> Native.readerGetMetrics(handle.ptr, s, out)));
            return new ReaderMetrics(new MetricsValues.Struct(out, Signatures.READER_METRICS));
        } finally {
            handle.exit();
            Reference.reachabilityFence(this);
        }
    }

    void enterForView() {
        handle.enter();
    }

    void exitForView() {
        handle.exit();
        Reference.reachabilityFence(this);
    }

    /** Whether {@link #close()} has started (from then on the reader refuses every call). */
    public boolean isClosed() {
        return handle.isClosed();
    }

    /**
     * Closes the reader and frees it, invalidating its view. Idempotent, and callable from any thread. From the moment
     * it starts, the reader refuses every new call and {@link #isClosed()} is true. If another thread is inside a call
     * on this reader, waits for it to finish (a blocked read ends within one wait slice, throwing
     * {@link IllegalStateException}), without spinning; an interrupt does not end that wait, and the thread's interrupt
     * status is kept. If another thread is reading the view, its access in progress either finishes or throws
     * {@link IllegalStateException}, every later one throws, and none reads freed memory. When this returns, on any
     * thread, the reader is freed (a concurrent second {@code close} waits for the first).
     *
     * <p>Ending the accesses to the view closes a shared arena, which the JDK does with a handshake with every thread of
     * the JVM: every running Java thread is stopped briefly, and the cost grows with the number of threads (see
     * {@code docs/java-api.md} &rarr; Interruption and closing). Keep a reader open and {@link #seek(long)} it instead of
     * reopening.
     *
     * @throws InternalErrorException if the library reported an internal error while closing (the reader is freed
     *     all the same)
     * @throws IllegalStateException if called from inside {@link RecordView#withPayload} on this reader's view
     */
    @Override
    public void close() {
        try {
            TickertapeException error = handle.close(generation::advance);
            if (error != null) {
                throw error;
            }
        } finally {
            Reference.reachabilityFence(this); // see NativeHandle: the Cleaner must not claim the clean-up meanwhile
        }
    }

    @Override
    public String toString() {
        return "Reader[" + dir + (mapped ? ", mapped" : "") + (isClosed() ? ", closed]" : "]");
    }
}
