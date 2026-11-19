package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.Charset;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystems;
import java.nio.file.Path;

/**
 * Path conversion for the C ABI, which takes NUL-terminated byte strings and passes them to the OS unchanged (ADR 0037
 * &rarr; Paths).
 *
 * <p>The JDK keeps a path's bytes as the operating system gave them but offers no public way to read them: only
 * {@link Path#toString()}, which decodes them with the file-system encoding (the {@code sun.jnu.encoding} property:
 * UTF-8 on macOS, the locale's charset on Linux). So the binding encodes that string back with the same charset, which
 * gives the JDK's own bytes for every name the charset can decode, and refuses a path whose bytes it cannot decode (on
 * Linux, a name that is not valid UTF-8 under a UTF-8 locale): decoding replaced them with U+FFFD, and encoding that
 * would name a different file, possibly an existing sibling. Such a path throws {@link IllegalArgumentException}
 * before anything reaches the library.
 */
final class Paths {
    private Paths() {}

    /** The charset the JDK uses for file names on this platform. */
    static final Charset FILE_NAMES = fileNameCharset();

    private static Charset fileNameCharset() {
        for (String property : new String[] {"sun.jnu.encoding", "native.encoding"}) {
            String name = System.getProperty(property);
            if (name != null) {
                try {
                    return Charset.forName(name);
                } catch (IllegalArgumentException e) {
                    // not supported here: try the next one
                }
            }
        }
        return StandardCharsets.UTF_8;
    }

    static void requireDefaultFileSystem(Path p) {
        if (p.getFileSystem() != FileSystems.getDefault()) {
            throw new IllegalArgumentException("not a path on the default file system: " + p);
        }
    }

    /**
     * The bytes the operating system knows {@code p} by, without the terminating NUL; throws
     * {@link IllegalArgumentException} for a path that is not on the default file system, contains NUL, or whose bytes
     * do not survive the conversion (see the class documentation).
     */
    static byte[] nativeBytes(Path p) {
        requireDefaultFileSystem(p);
        String s = p.toString();
        if (s.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("path contains a NUL character: " + p);
        }
        // U+FFFD is what decoding put in place of bytes it could not decode; a name may also contain the character
        // itself, which survives, so only the round trip decides (the JDK compares paths by their bytes).
        if (s.indexOf('�') >= 0 && !Path.of(s).equals(p)) {
            throw notConvertible(p);
        }
        try {
            ByteBuffer bytes = FILE_NAMES.newEncoder().onMalformedInput(CodingErrorAction.REPORT)
                    .onUnmappableCharacter(CodingErrorAction.REPORT).encode(CharBuffer.wrap(s));
            byte[] out = new byte[bytes.remaining()];
            bytes.get(out);
            return out;
        } catch (CharacterCodingException e) {
            IllegalArgumentException error = notConvertible(p);
            error.initCause(e);
            throw error;
        }
    }

    private static IllegalArgumentException notConvertible(Path p) {
        return new IllegalArgumentException("path " + p + " has a name that is not valid in the file-system encoding ("
                + FILE_NAMES + "); the JDK does not expose its bytes, and converting it would name a different file."
                + " Rename it, or run the JVM with a locale whose charset can decode it");
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
