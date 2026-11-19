package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.file.Path;
import java.time.Duration;
import java.util.Objects;

/**
 * Retention settings (ADR 0016): limits, plus what to do with segments that exceed them. Immutable; each method
 * returns a new value. There is no default action: start from {@link #delete()} or {@link #moveTo(Path)}. Without a
 * limit nothing is removed. Only a sealed prefix strictly before the checkpoint segment is ever eligible.
 *
 * <p>Use with {@link WriterOptions#retention(Retention)} (applied after open and after every successful checkpoint)
 * or {@link Journal#applyRetention(Path, Retention, boolean)} (once).
 *
 * <pre>{@code
 * Retention keepTen = Retention.delete().maxSegments(10);
 * Retention archive = Retention.moveTo(Path.of("/archive/journal")).maxAge(Duration.ofDays(7));
 * }</pre>
 */
public final class Retention {
    private final Path moveTo; // null: delete
    private final long maxSegments;
    private final long maxBytes;
    private final long maxAgeNs;

    private Retention(Path moveTo, long maxSegments, long maxBytes, long maxAgeNs) {
        this.moveTo = moveTo;
        this.maxSegments = maxSegments;
        this.maxBytes = maxBytes;
        this.maxAgeNs = maxAgeNs;
    }

    /** Settings that <b>permanently delete</b> eligible segments, with no limits yet. */
    public static Retention delete() {
        return new Retention(null, -1, -1, -1);
    }

    /**
     * Settings that move eligible segments into the existing directory {@code dir}, keeping their names, with no limits
     * yet. A move never replaces an existing file there (it fails instead).
     *
     * @throws IllegalArgumentException if {@code dir} is not on the default file system (see
     *     {@linkplain Tickertape##paths Tickertape &rarr; Paths}, which also says how the path is passed)
     */
    public static Retention moveTo(Path dir) {
        Objects.requireNonNull(dir, "dir");
        Paths.check(dir); // refuses a path of another file system now, not when the retention is applied
        return new Retention(dir, -1, -1, -1);
    }

    /** Keeps at most {@code count} segment files in total. Zero still keeps the checkpoint segment and those after it. */
    public Retention maxSegments(long count) {
        return new Retention(moveTo, requireNonNegative(count, "count"), maxBytes, maxAgeNs);
    }

    /** Keeps the total size of segment files at or below {@code bytes} (file sizes, not occupied bytes). */
    public Retention maxBytes(long bytes) {
        return new Retention(moveTo, maxSegments, requireNonNegative(bytes, "bytes"), maxAgeNs);
    }

    /** Removes segments sealed longer ago than {@code age}, by the writer's wall clock (nanosecond precision). */
    public Retention maxAge(Duration age) {
        Objects.requireNonNull(age, "age");
        if (age.isNegative()) {
            throw new IllegalArgumentException("age must not be negative: " + age);
        }
        long ns;
        try {
            ns = age.toNanos();
        } catch (ArithmeticException e) {
            ns = Long.MAX_VALUE;
        }
        return new Retention(moveTo, maxSegments, maxBytes, ns);
    }

    /** The move destination, or {@code null} if eligible segments are deleted. */
    public Path moveToDirectory() {
        return moveTo;
    }

    private static long requireNonNegative(long v, String name) {
        if (v < 0) {
            throw new IllegalArgumentException(name + " must not be negative: " + v);
        }
        return v;
    }

    /** Builds native retention settings in {@code arena}'s lifetime; free them with {@code tickertape_retention_free}. */
    MemorySegment toNative(Arena arena) {
        MemorySegment r = Native.retentionNew();
        try {
            if (maxSegments >= 0) {
                Errors.check(Native.retentionSetMaxSegments(r, maxSegments));
            }
            if (maxBytes >= 0) {
                Errors.check(Native.retentionSetMaxBytes(r, maxBytes));
            }
            if (maxAgeNs >= 0) {
                Errors.check(Native.retentionSetMaxAgeNs(r, maxAgeNs));
            }
            if (moveTo == null) {
                Errors.check(Native.retentionSetDelete(r));
            } else {
                Errors.check(Native.retentionSetMoveTo(r, Paths.toC(moveTo, arena)));
            }
            return r;
        } catch (Throwable e) { // an Error too: the native struct must not leak
            Native.retentionFree(r);
            throw e;
        }
    }

    @Override
    public boolean equals(Object o) {
        return o instanceof Retention r && Objects.equals(moveTo, r.moveTo) && maxSegments == r.maxSegments
                && maxBytes == r.maxBytes && maxAgeNs == r.maxAgeNs;
    }

    @Override
    public int hashCode() {
        return Objects.hash(moveTo, maxSegments, maxBytes, maxAgeNs);
    }

    @Override
    public String toString() {
        StringBuilder b = new StringBuilder(moveTo == null ? "Retention.delete()" : "Retention.moveTo(" + moveTo + ")");
        if (maxSegments >= 0) {
            b.append(".maxSegments(").append(maxSegments).append(')');
        }
        if (maxBytes >= 0) {
            b.append(".maxBytes(").append(maxBytes).append(')');
        }
        if (maxAgeNs >= 0) {
            b.append(".maxAge(").append(Duration.ofNanos(maxAgeNs)).append(')');
        }
        return b.toString();
    }
}
