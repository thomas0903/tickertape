package io.github.thomas0903.tickertape;

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * A counter that a reader or batch advances on every call that invalidates the views it handed out, and that a reader's
 * close advances from the closing thread. Advanced with a release store and read with an acquire load, so a view on
 * any thread sees the change at its next accessor: the load cannot be hoisted out of a loop, and it costs no more than
 * a plain load on x86-64 (a plain {@code mov}) and little on AArch64 ({@code ldar}). Only one thread advances it at a
 * time (the owner inside a call, or the closer once the handle refuses calls).
 */
final class Generation {
    private static final VarHandle VALUE;

    static {
        try {
            VALUE = MethodHandles.lookup().findVarHandle(Generation.class, "value", long.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    @SuppressWarnings("unused") // accessed through VALUE
    private long value;

    /** The current value (an acquire load). */
    long get() {
        return (long) VALUE.getAcquire(this);
    }

    /** Advances the counter (a release store), invalidating every view of the old value. */
    void advance() {
        VALUE.setRelease(this, (long) VALUE.get(this) + 1);
    }
}
