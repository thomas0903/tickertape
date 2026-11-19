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
import java.nio.file.InvalidPathException;
import java.nio.file.Path;
import java.text.Normalizer;

/**
 * Path conversion for the C ABI, which takes NUL-terminated byte strings and passes them to the OS unchanged (ADR 0037
 * &rarr; Paths).
 *
 * <p>The JDK keeps a path's bytes as the operating system gave them but offers no public way to read them: only
 * {@link Path#toString()}, which decodes them with the file-system encoding (the {@code sun.jnu.encoding} property:
 * UTF-8 on macOS, the locale's charset on Linux). So the binding passes a path only when its text provably stands for
 * its bytes, and refuses it otherwise: it builds a path from the text ({@code Path.of(p.toString())}, which encodes the
 * text the way the JDK encodes any path's text) and compares it with {@code p}; the JDK compares paths by their bytes.
 * If the two are equal, {@code p}'s bytes are exactly those of the text encoded the JDK's way, so the binding encodes
 * the text that same way and passes the result. If not, {@code p} holds other bytes than its text stands for, and
 * converting the text would name a different file, possibly an existing sibling; such a path throws
 * {@link IllegalArgumentException} before anything reaches the library. The round trip is made for every path, on every
 * platform; it fails in two known cases:
 *
 * <ul>
 *   <li>a name the charset cannot decode (on Linux, a name that is not valid UTF-8 under a UTF-8 locale): decoding
 *       replaced its bytes with U+FFFD. A name that contains U+FFFD itself round-trips and is accepted.
 *   <li>on macOS with {@code -Djdk.nio.path.useNormalizationFormD} (or {@code =true}), a path whose bytes are not in
 *       Unicode normalization form D, such as an entry stored in form C that a directory listing returned. With that
 *       property the JDK converts a path's text to form D before encoding it and shows it in form C, so a path made
 *       from a string holds form D bytes; a listed entry keeps the bytes it has on disk, shows the same text, and
 *       would be sent as form D, which on a volume that tells the forms apart names another file or none. Build such
 *       a path from a string, or run without the property. When the round trip holds, the binding normalizes to form
 *       D too, as the JDK did.
 * </ul>
 *
 * <p>Both settings, the charset and the macOS normalization property, are read once, when this class is initialized
 * (the first path conversion), as the JDK reads its own at start-up. Changing either property later is not supported.
 */
final class Paths {
    private Paths() {}

    /** The charset the JDK uses for file names on this platform, read once (see the class documentation). */
    static final Charset FILE_NAMES = fileNameCharset();

    /**
     * Whether the JDK normalizes path text to form D before encoding it: on macOS, when the
     * {@code jdk.nio.path.useNormalizationFormD} property is empty or {@code true} (the JDK's own test), read once.
     */
    static final boolean NORMALIZE_TO_NFD = System.getProperty("os.name", "").startsWith("Mac")
            && normalizationProperty(System.getProperty("jdk.nio.path.useNormalizationFormD"));

    static boolean normalizationProperty(String value) {
        return value != null && (value.isEmpty() || Boolean.parseBoolean(value));
    }

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

    /** What the binding does with a path's text, from the facts that decide it (see the class documentation). */
    enum Conversion {
        /** Pass the text encoded with the file-name charset. */
        ENCODE,
        /** Pass the text normalized to form D, then encoded with the file-name charset. */
        ENCODE_NFD,
        /** Refuse: the round trip fails and the text holds U+FFFD, in place of bytes the charset could not decode. */
        REFUSE_UNDECODABLE,
        /** Refuse: the form-D property is set and the round trip fails; the path's bytes are not the form D ones. */
        REFUSE_NOT_FORM_D
    }

    /**
     * The decision for a path whose text is {@code text}: {@code nfd} is whether the JDK normalizes path text to form D
     * ({@link #NORMALIZE_TO_NFD}), {@code roundTrips} whether a path made from {@code text} equals the path
     * ({@link #roundTrips}). A path is passed only when the round trip holds, encoded as the JDK encodes its text.
     */
    static Conversion conversion(String text, boolean nfd, boolean roundTrips) {
        if (roundTrips) {
            return nfd ? Conversion.ENCODE_NFD : Conversion.ENCODE;
        }
        return nfd && text.indexOf('\uFFFD') < 0 ? Conversion.REFUSE_NOT_FORM_D : Conversion.REFUSE_UNDECODABLE;
    }

    /**
     * Whether a path made from {@code text} has the same bytes as {@code p} (the JDK compares paths by their bytes); a
     * text that the JDK cannot encode back does not round-trip.
     */
    static boolean roundTrips(String text, Path p) {
        try {
            return Path.of(text).equals(p);
        } catch (InvalidPathException e) {
            return false;
        }
    }

    /**
     * The bytes the operating system knows {@code p} by, without the terminating NUL; throws
     * {@link IllegalArgumentException} for a path that is not on the default file system, contains NUL, or whose text
     * does not stand for its bytes (see the class documentation).
     */
    static byte[] nativeBytes(Path p) {
        requireDefaultFileSystem(p);
        String s = p.toString();
        if (s.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("path contains a NUL character: " + p);
        }
        Conversion c = conversion(s, NORMALIZE_TO_NFD, roundTrips(s, p));
        switch (c) {
            case REFUSE_UNDECODABLE -> throw notConvertible(p);
            case REFUSE_NOT_FORM_D -> throw notFormD(p);
            default -> {}
        }
        try {
            return encode(s, FILE_NAMES, c == Conversion.ENCODE_NFD);
        } catch (CharacterCodingException e) { // not expected: Path.of encoded the same text
            IllegalArgumentException error = notConvertible(p);
            error.initCause(e);
            throw error;
        }
    }

    /**
     * Encodes a path's text as the JDK does: normalized to form D first if {@code nfd} (the JDK does it only for text
     * with a character above U+0080, which normalization leaves unchanged otherwise), then encoded with
     * {@code charset}, refusing what it cannot encode.
     */
    static byte[] encode(String text, Charset charset, boolean nfd) throws CharacterCodingException {
        String s = nfd ? Normalizer.normalize(text, Normalizer.Form.NFD) : text;
        ByteBuffer bytes = charset.newEncoder().onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT).encode(CharBuffer.wrap(s));
        byte[] out = new byte[bytes.remaining()];
        bytes.get(out);
        return out;
    }

    private static IllegalArgumentException notConvertible(Path p) {
        return new IllegalArgumentException("path " + p + " has a name that is not valid in the file-system encoding ("
                + FILE_NAMES + "); the JDK does not expose its bytes, and converting it would name a different file."
                + " Rename it, or run the JVM with a locale whose charset can decode it");
    }

    private static IllegalArgumentException notFormD(Path p) {
        return new IllegalArgumentException("path " + p + " does not hold the bytes that a path made from its text"
                + " would: with -Djdk.nio.path.useNormalizationFormD the JDK encodes a path's text in Unicode form D,"
                + " but this path (from a directory listing, say) holds other bytes, such as form C, which the JDK does"
                + " not expose, and converting it would name a different file. Build the path from a string"
                + " (Path.of), or run the JVM without that property");
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
