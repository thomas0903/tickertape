package io.github.thomas0903.tickertape;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.net.URI;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.Arrays;
import java.util.function.BooleanSupplier;
import java.util.function.Supplier;

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
 * <p>An absolute path is passed as those bytes. A relative path is passed relative when the JDK passes it to the
 * operating system as it is (below), so that the C library resolves it against the same directory: the JDK forms its
 * absolute path by joining the bytes of its default directory, a {@code /} and the relative path's own bytes, so the
 * binding decodes the default directory's URI ({@code Path.of("").toUri()}) the same way and strips that prefix and the
 * separator after it. An empty remainder is passed as {@code .}. If the prefix does not match, the absolute bytes are
 * passed rather than a guess. Either way, trailing slashes are stripped except for the root.
 *
 * <p>The JDK reads its default directory from {@code user.dir} at start-up and decides then, once, whether it passes a
 * relative path to the operating system as it is (when the default directory has the bytes {@code getcwd} returned
 * at that moment, and {@code sun.nio.fs.chdirAllowed} is not set) or resolved against the default directory
 * (otherwise, for example under {@code -Duser.dir=/b} in {@code /a}). That flag is not readable, so the binding infers
 * it, the first time it converts a relative path, from what the JDK does: see
 * {@link #passRelative(byte[], byte[], boolean, Directories)}. In short, a relative path is passed relative when the
 * process's working directory now has the default directory's bytes, or when the JDK's own {@code stat} of
 * {@code Path.of("")} reaches the working directory while the default directory's path does not (the working directory
 * was renamed after start-up); otherwise it is passed as its absolute bytes, to which the path-length limit then applies, as it does to the JDK's own file operations in that
 * configuration. A determination that succeeded is kept for the life of the class (the JDK's flag does not change
 * either); one that failed is not, and the path is passed absolute while the next conversion tries again. A process
 * that changes its working directory through native code is outside what the binding follows, as it is for the JDK.
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

    /** This JVM's determination, made at the first conversion of a relative path. */
    private static final Decision DECISION = new Decision();
    /** The working directory as {@code getcwd} returns it now, or {@code null} if the call fails. */
    private static final Supplier<byte[]> GETCWD = () -> processWorkingDirectory(1024);

    /**
     * Whether a relative path is passed relative, determined on first use and kept once a determination succeeds. A
     * failed one (the {@code getcwd} call refused, say because the module has no native access yet, or a {@code stat}
     * failing) is not kept: that conversion passes the path absolute, and the next one tries again. The cost of a try
     * is a {@code getcwd} and at most three {@code stat}s; conversions happen when a journal is opened or inspected or
     * a retention destination is applied, never per record, and once a try succeeds a conversion reads one volatile
     * field.
     */
    static final class Decision {
        private volatile Boolean passRelative;

        /** The kept determination, or {@code null} if none has succeeded yet. */
        Boolean kept() {
            return passRelative;
        }

        /** Whether to pass a relative path relative; {@code workingDirectory} is asked only until one try succeeds. */
        boolean passRelative(Supplier<byte[]> workingDirectory) {
            Boolean kept = passRelative;
            if (kept != null) {
                return kept;
            }
            Boolean now = Paths.passRelative(workingDirectory.get(), stripTrailingSlashes(uriBytes(EMPTY)),
                    chdirAllowed(System.getProperty("sun.nio.fs.chdirAllowed", "false")), JDK_DIRECTORIES);
            if (now == null) {
                return false;
            }
            passRelative = now; // two threads may both decide; they reach the same answer
            return now;
        }
    }

    /** {@code sun.nio.fs.chdirAllowed} as the JDK reads it: empty or {@code true} makes it resolve every path. */
    static boolean chdirAllowed(String value) {
        return value.isEmpty() || Boolean.parseBoolean(value);
    }

    /**
     * The identities of the three directories {@link #passRelative(byte[], byte[], boolean, Directories)} compares; any
     * value with {@code equals}, {@code null} if unknown.
     */
    interface Directories {
        /** The directory the JDK reaches for {@code Path.of("")}: {@code .} or its default directory. */
        Object empty() throws IOException;

        /** The directory the default directory's absolute path names now. */
        Object defaultDirectory() throws IOException;

        /** The directory the bytes {@code getcwd} returned name now. */
        Object workingDirectory(byte[] processCwd) throws IOException;
    }

    /** {@link Directories} through the JDK's own {@code stat}: the file key, device and inode on Linux and macOS. */
    static final Directories JDK_DIRECTORIES = new Directories() {
        @Override
        public Object empty() throws IOException {
            return fileKey(EMPTY);
        }

        @Override
        public Object defaultDirectory() throws IOException {
            return fileKey(EMPTY.toAbsolutePath());
        }

        @Override
        public Object workingDirectory(byte[] processCwd) throws IOException {
            return fileKey(fromBytes(processCwd));
        }
    };

    private static Object fileKey(Path p) throws IOException {
        return Files.readAttributes(p, BasicFileAttributes.class).fileKey();
    }

    /**
     * The path of the default file system with exactly these absolute bytes: the default provider builds a path from a
     * {@code file:} URI's percent-encoded bytes without decoding them as text (the inverse of {@link #uriBytes}).
     */
    static Path fromBytes(byte[] absolute) {
        StringBuilder uri = new StringBuilder("file://");
        for (byte b : absolute) {
            int c = b & 0xFF;
            if (c == '/' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || c == '-' || c == '.'
                    || c == '_' || c == '~') {
                uri.append((char) c);
            } else {
                uri.append('%').append(Character.toUpperCase(Character.forDigit(c >> 4, 16)))
                        .append(Character.toUpperCase(Character.forDigit(c & 0xF, 16)));
            }
        }
        return Path.of(URI.create(uri.toString()));
    }

    /**
     * Whether a relative path is passed relative, as the JDK passes it: {@code TRUE} relative, {@code FALSE} absolute,
     * {@code null} if it cannot be determined now. The JDK passes relative paths as they are exactly when, at start-up,
     * {@code sun.nio.fs.chdirAllowed} was not set and its default directory had the bytes {@code getcwd} returned
     * ({@code UnixFileSystem}'s {@code needToResolveAgainstDefaultDirectory}, used by
     * {@code UnixPath.getByteArrayForSysCalls}). Given {@code getcwd}'s bytes now ({@code null} if it failed), the
     * default directory's bytes and the property:
     *
     * <ol>
     *   <li>{@code getcwd} failed, or returned no absolute path: {@code null}.
     *   <li>The property is set: {@code FALSE}, as the JDK resolves every path.
     *   <li>The two byte strings are equal (trailing slashes aside): {@code TRUE}; the working directory is the one the
     *       JVM started in, unless native code moved it.
     *   <li>Otherwise either the JDK resolves against another directory, or names the working directory by another path
     *       ({@code -Duser.dir=} naming a symbolic link to it, say), or it passes relative paths as they are and the
     *       working directory was renamed after start-up. The JDK's own {@code stat} of {@code Path.of("")} (of
     *       {@code .} when it passes relative paths as they are, of the default directory otherwise) tells: if it does
     *       not reach the directory {@code getcwd} names, {@code FALSE}.
     *   <li>It does. If the default directory's absolute path also reaches it, {@code FALSE}: the JDK resolves against
     *       that path (step 4's {@code stat} was of it) or, if the working directory was renamed and that path made to
     *       name it again, both forms reach the same directory. If that path names another directory or none,
     *       {@code TRUE}: step 4's {@code stat} was of {@code .}, so the JDK passes relative paths as they are.
     * </ol>
     *
     * <p>Comparing real paths ({@code toRealPath}) would not do: the JDK's {@code UnixPath.toRealPath} resolves the
     * default directory's path, not {@code .}, so it fails once the working directory is renamed, and it returns the
     * working directory's real path for a {@code user.dir} that names it through a symbolic link, against which the JDK
     * resolves. A {@code stat} failure in steps 1 to 4 is {@code null}; one in step 5 is {@code TRUE}, since that path
     * gave step 4's answer moments before if the JDK resolves against it.
     */
    static Boolean passRelative(byte[] processCwd, byte[] defaultDirectory, boolean chdirAllowed, Directories dirs) {
        if (processCwd == null || processCwd.length == 0 || processCwd[0] != '/') {
            return null;
        }
        if (chdirAllowed) {
            return false;
        }
        if (Arrays.equals(stripTrailingSlashes(processCwd), stripTrailingSlashes(defaultDirectory))) {
            return true;
        }
        Object workingDirectory;
        Object empty;
        try {
            workingDirectory = dirs.workingDirectory(processCwd);
            empty = dirs.empty();
        } catch (IOException | RuntimeException e) { // a RuntimeException: an InvalidPathException, say
            return null;
        }
        if (workingDirectory == null || empty == null) {
            return null;
        }
        if (!empty.equals(workingDirectory)) {
            return false;
        }
        Object defaultDir;
        try {
            defaultDir = dirs.defaultDirectory();
        } catch (IOException e) {
            return true;
        } catch (RuntimeException e) {
            return null;
        }
        return defaultDir == null ? null : !defaultDir.equals(workingDirectory);
    }

    /** Whether this JVM passes relative paths relative (see {@link Decision}). */
    static boolean passesRelative() {
        return DECISION.passRelative(GETCWD);
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
        return nativeBytes(p, Paths::passesRelative);
    }

    /** {@link #nativeBytes(Path)}, asking {@code passRelative} (only for a relative path) how to pass it. */
    static byte[] nativeBytes(Path p, BooleanSupplier passRelative) {
        requireDefaultFileSystem(p);
        byte[] absolute = stripTrailingSlashes(uriBytes(p));
        if (p.isAbsolute() || !passRelative.getAsBoolean()) {
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

    /**
     * Checks that {@code p} can be converted (see {@link #nativeBytes(Path)}) without determining how a
     * relative path is passed; throws {@link IllegalArgumentException} otherwise.
     */
    static void check(Path p) {
        requireDefaultFileSystem(p);
        uriBytes(p);
    }

    private static void requireDefaultFileSystem(Path p) {
        if (p.getFileSystem() != FileSystems.getDefault()) {
            throw new IllegalArgumentException("not a path on the default file system: " + p);
        }
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
