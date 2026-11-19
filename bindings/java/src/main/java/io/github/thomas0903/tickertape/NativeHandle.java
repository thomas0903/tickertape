package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
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
 * they must be exceptions. Every call therefore enters the guard with one compare-and-set ({@code IDLE -> BUSY}) and
 * leaves it with another ({@code BUSY -> IDLE}); a call that finds the handle busy (another thread is inside a call, or
 * a fill callback calls back into its writer), closing or closed throws {@link IllegalStateException} without touching
 * the library.
 *
 * <p>{@link #close} on an idle handle moves it to {@code CLOSING}, frees it exactly once, and only then moves it to
 * {@code CLOSED}. On a busy handle it moves it to {@code CLOSE_PENDING} and waits: from then on every new call is
 * refused (so a thread calling in a loop cannot starve the close, and nothing is consumed after the close began), a
 * waiting read gives up within one wait slice, and the call in progress, when it leaves, hands the handle straight to
 * the closer ({@code CLOSE_PENDING -> CLOSING}) with no moment at which another call could enter. A concurrent second
 * {@link #close} waits for {@code CLOSED}, so every close returns after the C handle is freed. Waiting blocks on this
 * object's monitor (no spinning, and an interrupt does not end the wait: the interrupt status is kept for the caller).
 *
 * <p>Freeing runs in this order: the arena whose memory the owner hands out without a call (a reader's views) is
 * closed first, which ends every access to it on every thread (the shared arena's handshake); then the C close, whose
 * error, if any, is captured right away on the same thread; then the arena that holds memory the library calls into (a
 * writer's upcall stub), which no call can use any more.
 *
 * <p>A {@link Cleaner} frees a handle whose owner became unreachable without being closed. It is a safety net only:
 * close handles deterministically. Owners call {@link java.lang.ref.Reference#reachabilityFence} at the end of each
 * call, {@code close} included, so an owner cannot become unreachable while one of its calls is still running: an
 * explicit close therefore always runs the clean-up itself, on its own thread, and never finds it claimed by the
 * Cleaner (whose {@code clean()} would then return at once, before the handle is freed).
 */
final class NativeHandle {
    private static final int IDLE = 0;
    private static final int BUSY = 1;
    /** A call is in progress and a close waits for it: calls are refused, and the call's exit hands over. */
    private static final int CLOSE_PENDING = 2;
    private static final int CLOSING = 3;
    private static final int CLOSED = 4;

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

    /**
     * The state; changes to {@code CLOSING} after {@code CLOSE_PENDING}, and to {@code CLOSED}, happen while holding
     * this object's monitor, which closers wait on.
     */
    private volatile int state;
    /** The thread inside a call (plain field: only read to recognise a call from inside a callback). */
    private Thread caller;

    /** Frees the C handle and its arenas; holds no reference to the owner, so the Cleaner can run it. */
    private static final class Closer implements Runnable {
        private final MemorySegment ptr;
        private final ToIntFunction<MemorySegment> close;
        private final Arena beforeFree;
        private volatile Arena afterFree;
        /** The C close's error, captured on the thread that ran it, right after the call; null if it succeeded. */
        volatile TickertapeException error;

        Closer(MemorySegment ptr, ToIntFunction<MemorySegment> close, Arena beforeFree) {
            this.ptr = ptr;
            this.close = close;
            this.beforeFree = beforeFree;
        }

        @Override
        public void run() {
            if (beforeFree != null) {
                closeArena(beforeFree);
            }
            int status = close.applyAsInt(ptr);
            if (status < 0) {
                error = Errors.capture(status); // the details are per OS thread: read them before anything else
            }
            Arena after = afterFree;
            if (after != null) {
                closeArena(after);
            }
        }
    }

    /**
     * Closes a shared arena. Closing waits (a thread-local handshake) for every access in progress on any thread, and
     * an access that starts later throws {@link IllegalStateException}. It refuses only while a segment of the arena
     * is passed to a downcall, which the binding never does with these arenas; retrying covers that case anyway,
     * without spinning on an interrupted thread (whose interrupt status is kept).
     */
    private static void closeArena(Arena arena) {
        boolean interrupted = false;
        try {
            for (int spins = 0;; spins++) {
                try {
                    arena.close();
                    return;
                } catch (IllegalStateException e) {
                    if (!arena.scope().isAlive()) {
                        return;
                    }
                    if (spins < 100) {
                        Thread.onSpinWait();
                    } else {
                        interrupted |= Thread.interrupted(); // parkNanos returns at once while interrupted
                        LockSupport.parkNanos(50_000);
                    }
                }
            }
        } finally {
            if (interrupted) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /**
     * Guards {@code ptr}, which {@code close} frees. {@code beforeFree}, if not null, is a shared arena that is closed
     * before the C close (see the class documentation).
     */
    NativeHandle(Object owner, String kind, MemorySegment ptr, ToIntFunction<MemorySegment> close, Arena beforeFree) {
        this.ptr = ptr;
        this.kind = kind;
        this.closer = new Closer(ptr, close, beforeFree);
        this.cleanable = CLEANER.register(owner, closer);
    }

    /**
     * Registers a shared arena to close after the C close (once; call inside a call on this handle). It must not
     * reference the owner, or the Cleaner could never run.
     */
    void closeAfterFree(Arena arena) {
        closer.afterFree = arena;
    }

    /** Enters a call; throws {@link IllegalStateException} if the handle is closed or already inside a call. */
    void enter() {
        if (!STATE.compareAndSet(this, IDLE, BUSY)) {
            throw notIdle();
        }
        caller = Thread.currentThread();
    }

    /**
     * Leaves a call entered with {@link #enter()}. If a close is waiting, hands the handle to it: the state goes from
     * {@code CLOSE_PENDING} straight to {@code CLOSING}, so no call can enter in between.
     */
    void exit() {
        caller = null;
        if (!STATE.compareAndSet(this, BUSY, IDLE)) {
            synchronized (this) { // CLOSE_PENDING: only a closer changes the state of a busy handle
                state = CLOSING;
                notifyAll();
            }
        }
    }

    private IllegalStateException notIdle() {
        int s = state;
        if (s >= CLOSE_PENDING) {
            return new IllegalStateException("the " + kind + " is closed");
        }
        if (caller == Thread.currentThread()) {
            return new IllegalStateException("the " + kind + " is already inside a call on this thread (a call from"
                    + " inside a fill callback or a payload action into the same " + kind + ")");
        }
        return new IllegalStateException("the " + kind + " is in use by another thread; a " + kind
                + " may be used by one thread at a time");
    }

    /** True once {@link #close} waits for the call in progress (called from inside that call). */
    boolean closeRequested() {
        return state == CLOSE_PENDING;
    }

    /** True once {@link #close} has started (the handle refuses calls from then on). */
    boolean isClosed() {
        return state >= CLOSE_PENDING;
    }

    /** As {@link #close(Runnable)} with nothing to run. */
    TickertapeException close() {
        return close(null);
    }

    /**
     * Closes the handle: returns null if this call freed it without error or it was already closed, or the error the
     * C close reported (the handle is freed in that case too), captured on the thread that called the C close. Waits
     * for a call in progress on another thread to finish, and for a close in progress on another thread to free the
     * handle. {@code whileClosing}, if not null, runs once, after the handle refuses calls and before anything is
     * freed. The caller must keep the owner reachable until this returns.
     */
    TickertapeException close(Runnable whileClosing) {
        boolean interrupted = false;
        try {
            for (;;) {
                int s = state;
                if (s == IDLE) {
                    if (STATE.compareAndSet(this, IDLE, CLOSING)) {
                        return free(whileClosing);
                    }
                } else if (s == BUSY || s == CLOSE_PENDING) {
                    if (caller == Thread.currentThread()) {
                        throw new IllegalStateException("cannot close the " + kind + " from inside a call on it"
                                + " (a fill callback or a payload action)");
                    }
                    if (s == BUSY && STATE.compareAndSet(this, BUSY, CLOSE_PENDING)) {
                        interrupted |= await(CLOSING); // the call's exit() hands the handle over
                        return free(whileClosing);
                    }
                    if (s == CLOSE_PENDING) {
                        interrupted |= await(CLOSED); // another thread's close is waiting for the call
                        return null;
                    }
                } else {
                    interrupted |= await(CLOSED);
                    return null;
                }
            }
        } finally {
            if (interrupted) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /** Runs the clean-up on this thread (the handle is {@code CLOSING} and owned by this thread). */
    private TickertapeException free(Runnable whileClosing) {
        try {
            if (whileClosing != null) {
                whileClosing.run();
            }
        } finally {
            try {
                cleanable.clean();
            } finally {
                synchronized (this) {
                    state = CLOSED;
                    notifyAll();
                }
            }
        }
        return closer.error;
    }

    /**
     * Waits until the state is at least {@code target} (from {@code CLOSE_PENDING} on, it only grows). Returns whether
     * the thread was interrupted meanwhile; the interrupt does not end the wait.
     */
    private boolean await(int target) {
        boolean interrupted = false;
        synchronized (this) {
            while (state < target) {
                try {
                    wait();
                } catch (InterruptedException e) {
                    interrupted = true;
                }
            }
        }
        return interrupted;
    }
}
