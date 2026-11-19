package io.github.thomas0903.tickertape;

import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.ref.Cleaner;
import java.util.concurrent.locks.LockSupport;
import java.util.function.ToIntFunction;

/**
 * A C writer or reader handle and the guard that keeps Java callers inside the C ABI's rules (ADR 0037 -> Threads).
 *
 * <p>The C ABI makes use after close, double close and concurrent calls on one handle undefined behaviour; in Java
 * they must be exceptions, never a crashed JVM. Every call therefore enters the guard with one compare-and-set
 * ({@code IDLE -> BUSY}) and leaves it with a release store; a call that finds the handle busy (another thread is
 * inside a call, or a fill callback calls back into its writer) or closed throws {@link IllegalStateException}
 * without touching the library. {@link #close} waits for a call in progress to finish (a waiting read gives up within
 * one wait slice when it sees the close request), then frees the handle exactly once.
 *
 * <p>A {@link Cleaner} frees a handle whose owner became unreachable without being closed. It is a safety net only:
 * close handles deterministically. Owners call {@link java.lang.ref.Reference#reachabilityFence} at the end of each
 * call, so an owner cannot become unreachable while one of its calls is still in the library.
 */
final class NativeHandle {
    private static final int IDLE = 0;
    private static final int BUSY = 1;
    private static final int CLOSED = 2;

    private static final VarHandle STATE;

    static {
        try {
            STATE = MethodHandles.lookup().findVarHandle(NativeHandle.class, "state", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    private static final Cleaner CLEANER = Cleaner.create();

    /** The C pointer (a zero-length segment). */
    final MemorySegment ptr;
    private final String kind;
    private final Closer closer;
    private final Cleaner.Cleanable cleanable;

    @SuppressWarnings("unused") // accessed through STATE
    private volatile int state;
    private volatile boolean closeRequested;
    /** The thread inside a call (plain field: only read to recognise a call from inside a callback). */
    private Thread caller;

    /** Frees the C handle; holds no reference to the owner, so the Cleaner can run it. */
    private static final class Closer implements Runnable {
        private final MemorySegment ptr;
        private final ToIntFunction<MemorySegment> close;
        int status;

        Closer(MemorySegment ptr, ToIntFunction<MemorySegment> close) {
            this.ptr = ptr;
            this.close = close;
        }

        @Override
        public void run() {
            status = close.applyAsInt(ptr);
        }
    }

    NativeHandle(Object owner, String kind, MemorySegment ptr, ToIntFunction<MemorySegment> close) {
        this.ptr = ptr;
        this.kind = kind;
        this.closer = new Closer(ptr, close);
        this.cleanable = CLEANER.register(owner, closer);
    }

    /** Enters a call; throws {@link IllegalStateException} if the handle is closed or already inside a call. */
    void enter() {
        if (!STATE.compareAndSet(this, IDLE, BUSY)) {
            throw notIdle();
        }
        caller = Thread.currentThread();
    }

    /** Leaves a call entered with {@link #enter()}. */
    void exit() {
        caller = null;
        STATE.setRelease(this, IDLE);
    }

    private IllegalStateException notIdle() {
        int s = (int) STATE.getVolatile(this);
        if (s == CLOSED) {
            return new IllegalStateException("the " + kind + " is closed");
        }
        if (caller == Thread.currentThread()) {
            return new IllegalStateException("the " + kind + " is already inside a call on this thread (a call from"
                    + " inside a fill callback or a payload action into the same " + kind + ")");
        }
        return new IllegalStateException("the " + kind + " is in use by another thread; a " + kind
                + " may be used by one thread at a time");
    }

    /** True once {@link #close} has been requested from another thread while a call was in progress. */
    boolean closeRequested() {
        return closeRequested;
    }

    boolean isClosed() {
        return (int) STATE.getVolatile(this) == CLOSED;
    }

    /**
     * Closes the handle: returns 0 if this call freed it or it was already closed, or the negative status the C close
     * returned (the handle is freed in that case too). Waits for a call in progress on another thread to finish.
     */
    int close() {
        int spins = 0;
        for (;;) {
            int s = (int) STATE.getVolatile(this);
            if (s == CLOSED) {
                return 0;
            }
            if (s == IDLE) {
                if (STATE.compareAndSet(this, IDLE, CLOSED)) {
                    cleanable.clean();
                    return closer.status;
                }
                continue;
            }
            if (caller == Thread.currentThread()) {
                throw new IllegalStateException("cannot close the " + kind + " from inside a call on it"
                        + " (a fill callback or a payload action)");
            }
            closeRequested = true;
            if (++spins < 100) {
                Thread.onSpinWait();
            } else if (spins < 200) {
                Thread.yield();
            } else {
                LockSupport.parkNanos(50_000);
            }
        }
    }
}
