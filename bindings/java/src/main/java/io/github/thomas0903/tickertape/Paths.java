package io.github.thomas0903.tickertape;

import java.io.ByteArrayOutputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
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
 * already end in one). The binding decodes {@code %XX} to that byte and every other character to its ASCII byte,
 * strips trailing slashes except for the root, and passes the result. So a name that is not valid in the file-system
 * encoding, a name in either Unicode normalization form, and a path that carries a trailing slash all reach the library
 * as the JDK holds them.
 *
 * <p>The exact URI form is the default provider's behaviour, not part of the {@code Path.toUri} specification;
 * {@code PathsTest} pins it, and anything other than the expected form (a raw path that does not start with {@code /},
 * a character above U+007F, a malformed escape, an encoded NUL) is refused with {@link IllegalArgumentException} naming
 * the path rather than guessed at.
 *
 * <p>What callers see is documented once, on {@link Tickertape} &rarr; Paths: the library receives the absolute path (a
 * relative path is resolved against the JVM's working directory, as the JDK resolves it), {@code toUri()} may
 * {@code stat} the path once per conversion, and only the default file system is supported.
 */
final class Paths {
    private Paths() {}

    /**
     * The bytes the operating system knows {@code p} by, made absolute, without trailing slashes (except for the root)
     * and without the terminating NUL; throws {@link IllegalArgumentException} for a path that is not on the default
     * file system or whose URI is not in the expected form (see the class documentation).
     */
    static byte[] nativeBytes(Path p) {
        if (p.getFileSystem() != FileSystems.getDefault()) {
            throw new IllegalArgumentException("not a path on the default file system: " + p);
        }
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
        byte[] bytes = out.toByteArray();
        int length = bytes.length;
        while (length > 1 && bytes[length - 1] == '/') {
            length--;
        }
        return length == bytes.length ? bytes : Arrays.copyOf(bytes, length);
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
