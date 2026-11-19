package io.github.thomas0903.tickertape;

import java.io.ByteArrayOutputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.nio.file.FileSystems;
import java.nio.file.Path;
import java.util.Arrays;

/**
 * Path conversion for the C ABI, which takes NUL-terminated byte strings and passes them to the OS unchanged (ADR 0037
 * &rarr; Paths).
 *
 * <p>The binding passes the C library the bytes the JDK itself uses for the path, read from the path's {@code file:}
 * URI rather than reconstructed from its text. For a path of the default file system on Unix and macOS,
 * {@link Path#toUri()} returns a URI whose raw path is the <b>absolute</b> path's bytes, percent-encoded byte by byte
 * (unreserved ASCII is left as it is; a {@code /} is appended when the path is an existing directory and does not
 * already end in one). The binding decodes {@code %XX} to that byte and every other character to its ASCII byte. So a
 * name that is not valid in the file-system encoding, a name in either Unicode normalization form, and a path that
 * carries a trailing slash all reach the library as the JDK holds them.
 *
 * <p>An absolute path is passed as those bytes. A relative path is passed relative when the JDK's default directory is
 * the process's working directory, against which the C library resolves it: the JDK forms its absolute path by joining
 * the bytes of its default directory, a {@code /} and the relative path's own bytes, so the binding decodes the default
 * directory's URI ({@code Path.of("").toUri()}) the same way and strips that prefix and the separator after it. An
 * empty remainder is passed as {@code .}. If the prefix does not match, the absolute bytes are passed rather than a
 * guess. Either way, trailing slashes are stripped except for the root.
 *
 * <p>The JDK reads its default directory from {@code user.dir} at start-up, so a JVM started with
 * {@code -Duser.dir=/b} in {@code /a} resolves {@code Path.of("journal")} under {@code /b}, while the C library would
 * resolve {@code journal} under {@code /a}. The JDK itself passes a relative path to the operating system as it is
 * only when its default directory is the process's working directory, and the resolved absolute path otherwise; the
 * binding does the same. When this class is initialised it reads the process's working directory once with the C
 * library's {@code getcwd} (through the FFM linker, in a buffer that grows on {@code ERANGE}) and compares its bytes
 * with the default directory's; when they differ, or {@code getcwd} fails, every relative path is passed as its
 * absolute bytes, to which the path-length limit then applies (as it does to the JDK's own file operations in that
 * configuration). A process that changes its working directory later, through native code, is outside what the
 * binding follows, as it is for the JDK.
 *
 * <p>The exact URI form is the default provider's behaviour, not part of the {@code Path.toUri} specification;
 * {@code PathsTest} pins it, and anything other than the expected form (a raw path that does not start with {@code /},
 * a character above U+007F, a malformed escape, an encoded NUL) is refused with {@link IllegalArgumentException} naming
 * the path rather than guessed at.
 *
 * <p>What callers see is documented once, on {@link Tickertape} &rarr; Paths.
 */
@SuppressWarnings("restricted") // calls getcwd through the FFM linker; needs native access
final class Paths {
    private Paths() {}

    /** The path the default directory has; a relative path is relative to it. */
    private static final Path EMPTY = Path.of("");

    /** {@code ERANGE} on Linux and macOS: {@code getcwd}'s buffer is too small. */
    private static final int ERANGE = 34;
    /** The largest buffer {@link #processWorkingDirectory} tries before it gives up. */
    private static final int GETCWD_MAX_BYTES = 1 << 20;

    /**
     * Whether a relative path is passed relative: the process's working directory, read once by {@code getcwd}, has
     * the bytes of the JDK's default directory (see the class documentation).
     */
    private static final boolean PASS_RELATIVE =
            passRelative(processWorkingDirectory(1024), stripTrailingSlashes(uriBytes(EMPTY)));

    /**
     * Whether a relative path may be passed relative, given the bytes {@code getcwd} returned for the process's working
     * directory ({@code null} if it failed) and those of the JDK's default directory: only when they are the same
     * directory by its bytes, trailing slashes aside.
     */
    static boolean passRelative(byte[] processCwd, byte[] defaultDirectory) {
        return processCwd != null && processCwd.length > 0 && processCwd[0] == '/'
                && Arrays.equals(stripTrailingSlashes(processCwd), stripTrailingSlashes(defaultDirectory));
    }

    /** Whether this JVM passes relative paths relative (see {@link #passRelative}). */
    static boolean passesRelative() {
        return PASS_RELATIVE;
    }

    /**
     * The process's working directory as {@code getcwd} returns it, without the NUL, or {@code null} if it fails (or
     * the call cannot be made). The buffer starts at {@code initialBytes} and doubles on {@code ERANGE}, up to
     * {@link #GETCWD_MAX_BYTES}.
     */
    static byte[] processWorkingDirectory(int initialBytes) {
        try {
            Linker linker = Linker.nativeLinker();
            MemorySegment symbol = linker.defaultLookup().find("getcwd").orElse(null);
            if (symbol == null) {
                return null;
            }
            MemoryLayout sizeT = linker.canonicalLayouts().get("size_t");
            if (sizeT == null || sizeT.byteSize() != 8) {
                return null;
            }
            Linker.Option errno = Linker.Option.captureCallState("errno");
            MethodHandle getcwd = linker.downcallHandle(symbol,
                    FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, sizeT), errno);
            MemoryLayout stateLayout = Linker.Option.captureStateLayout();
            VarHandle errnoHandle = stateLayout.varHandle(MemoryLayout.PathElement.groupElement("errno"));
            try (Arena arena = Arena.ofConfined()) {
                MemorySegment state = arena.allocate(stateLayout);
                for (long size = Math.max(1, initialBytes); size <= GETCWD_MAX_BYTES; size *= 2) {
                    MemorySegment buf = arena.allocate(size);
                    MemorySegment result = (MemorySegment) getcwd.invokeExact(state, buf, size);
                    if (!result.equals(MemorySegment.NULL)) {
                        for (long i = 0; i < size; i++) {
                            if (buf.get(ValueLayout.JAVA_BYTE, i) == 0) {
                                return buf.asSlice(0, i).toArray(ValueLayout.JAVA_BYTE);
                            }
                        }
                        return null; // not NUL-terminated: not what getcwd promises
                    }
                    if ((int) errnoHandle.get(state, 0L) != ERANGE) {
                        return null;
                    }
                }
                return null;
            }
        } catch (Error e) {
            throw e;
        } catch (Throwable t) { // invokeExact declares Throwable; a failed call means "unknown", hence absolute paths
            return null;
        }
    }

    /**
     * The bytes the operating system knows {@code p} by: absolute for an absolute path, relative (or {@code .}) for a
     * relative one when the JDK's default directory is the process's working directory (absolute otherwise), without
     * trailing slashes (except for the root) and without the terminating NUL; throws {@link IllegalArgumentException}
     * for a path that is not on the default file system or whose URI is not in the expected form (see the class
     * documentation).
     */
    static byte[] nativeBytes(Path p) {
        if (p.getFileSystem() != FileSystems.getDefault()) {
            throw new IllegalArgumentException("not a path on the default file system: " + p);
        }
        byte[] absolute = stripTrailingSlashes(uriBytes(p));
        if (p.isAbsolute() || !PASS_RELATIVE) {
            return absolute;
        }
        byte[] base = stripTrailingSlashes(uriBytes(EMPTY));
        int from;
        if (base.length == 1) { // the root: the JDK joins without a second separator
            from = 1;
        } else if (startsWith(absolute, base) && (absolute.length == base.length || absolute[base.length] == '/')) {
            from = Math.min(base.length + 1, absolute.length);
        } else {
            return absolute;
        }
        return from == absolute.length ? new byte[] {'.'} : Arrays.copyOfRange(absolute, from, absolute.length);
    }

    /** The decoded raw path of {@code p.toUri()}: the absolute path's bytes, perhaps with a trailing slash. */
    private static byte[] uriBytes(Path p) {
        String raw = p.toUri().getRawPath();
        if (raw == null || !raw.startsWith("/")) {
            throw unexpected(p, raw, "it is not an absolute path");
        }
        ByteArrayOutputStream out = new ByteArrayOutputStream(raw.length());
        for (int i = 0; i < raw.length(); i++) {
            char c = raw.charAt(i);
            int b;
            if (c == '%') {
                int hi = i + 2 < raw.length() ? Character.digit(raw.charAt(i + 1), 16) : -1;
                int lo = hi >= 0 ? Character.digit(raw.charAt(i + 2), 16) : -1;
                if (lo < 0) {
                    throw unexpected(p, raw, "it holds a malformed escape at index " + i);
                }
                b = hi << 4 | lo;
                i += 2;
            } else if (c > 0x7F) {
                throw unexpected(p, raw, "it holds a character above U+007F at index " + i);
            } else {
                b = c;
            }
            if (b == 0) {
                throw unexpected(p, raw, "it holds a NUL byte");
            }
            out.write(b);
        }
        return out.toByteArray();
    }

    private static byte[] stripTrailingSlashes(byte[] bytes) {
        int length = bytes.length;
        while (length > 1 && bytes[length - 1] == '/') {
            length--;
        }
        return length == bytes.length ? bytes : Arrays.copyOf(bytes, length);
    }

    private static boolean startsWith(byte[] bytes, byte[] prefix) {
        return bytes.length >= prefix.length && Arrays.equals(bytes, 0, prefix.length, prefix, 0, prefix.length);
    }

    private static IllegalArgumentException unexpected(Path p, String raw, String why) {
        return new IllegalArgumentException("cannot convert path " + p + ": the raw path of its file URI (" + raw
                + ") is not in the form the binding expects from the JDK, because " + why
                + "; please report this with your JDK version");
    }

    /** The path as a NUL-terminated string of its native bytes in {@code arena} (see {@link #nativeBytes}). */
    static MemorySegment toC(Path p, Arena arena) {
        byte[] bytes = nativeBytes(p);
        MemorySegment out = arena.allocate(bytes.length + 1L);
        MemorySegment.copy(bytes, 0, out, ValueLayout.JAVA_BYTE, 0, bytes.length);
        out.set(ValueLayout.JAVA_BYTE, bytes.length, (byte) 0);
        return out;
    }
}
