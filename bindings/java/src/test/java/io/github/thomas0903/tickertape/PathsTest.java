package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.function.Consumer;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Paths reach the C library as the bytes the operating system knows them by (ADR 0037 &rarr; Paths): a name the JVM
 * cannot decode is refused before anything reaches the library, so a lossy conversion can never select a sibling.
 */
class PathsTest {
    @TempDir
    Path tmp;

    private static void journal(Path dir) {
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[] {42});
        }
    }

    /** The entry of {@code tmp} whose name the JVM shows as {@code shown}, as the directory listing returns it. */
    private Path listed(String shown) throws Exception {
        try (Stream<Path> entries = Files.list(tmp)) {
            List<Path> matches = entries.filter(p -> p.getFileName().toString().equals(shown)).toList();
            return matches.isEmpty() ? null : matches.get(0);
        }
    }

    /**
     * On Linux a name can hold bytes that are not valid in the file-system encoding (here 0xFF under UTF-8). The JVM
     * shows it as {@code j\uFFFD}, the name of a different, valid directory; before the check, every call converted it
     * to that sibling's name. Skipped where the file system refuses such a name (macOS) or the JVM can decode every
     * byte (a single-byte locale).
     */
    @Test
    void aNameTheJvmCannotDecodeIsRefusedInsteadOfSelectingASibling() throws Exception {
        TestSupport.Result made = TestSupport.run(List.of("sh", "-c", "mkdir -- \"$1/$(printf 'j\\377')\"", "sh",
                tmp.toString()), 30);
        assumeTrue(made.exit() == 0, "the file system refuses a name that is not valid UTF-8: " + made.stderr());
        Path bad = listed("j\uFFFD");
        assumeTrue(bad != null, "the JVM decodes the name without loss (file-system encoding " + Paths.FILE_NAMES + ")");
        Path sibling = tmp.resolve("j\uFFFD"); // the bytes EF BF BD: a valid name that contains U+FFFD
        assertTrue(!sibling.equals(bad), "the JVM keeps the two names apart");
        journal(sibling);

        List<Consumer<Path>> calls = List.of(
                p -> Reader.open(p).close(),
                p -> Reader.openMappedUnsafe(p).close(),
                p -> Writer.open(p).close(),
                Journal::verify,
                Journal::listSegments,
                Journal::readCheckpoint,
                p -> Journal.applyRetention(p, Retention.delete().maxSegments(0), true),
                Retention::moveTo);
        for (Consumer<Path> call : calls) {
            IllegalArgumentException e = assertThrows(IllegalArgumentException.class, () -> call.accept(bad));
            assertTrue(e.getMessage().contains("file-system encoding"), e.getMessage());
        }
        try (Stream<Path> left = Files.list(bad)) {
            assertEquals(List.of(), left.toList(), "nothing was created in the undecodable directory");
        }
        // The sibling, whose name holds U+FFFD itself, works, by its own path.
        try (Reader r = Reader.open(sibling)) {
            assertEquals(42, r.tryNext().getByte(0));
        }
        assertEquals(1, Journal.listSegments(sibling).size());
    }

    /** Names the JVM can decode keep working, also as a directory listing returns them (on macOS, NFC or NFD). */
    @Test
    void nonAsciiNamesWork() throws Exception {
        String name = "journal-\u00e9\u4e2d";
        assumeTrue(Paths.FILE_NAMES.newEncoder().canEncode(name), "the file-system encoding cannot represent " + name);
        Path dir = tmp.resolve(name);
        journal(dir);
        Path asListed = listed(name);
        assertTrue(asListed != null, "listed");
        for (Path p : new Path[] {dir, asListed}) {
            try (Reader r = Reader.open(p)) {
                assertEquals(42, r.tryNext().getByte(0));
            }
            assertEquals(1, Journal.verify(p).records());
        }
    }

    /**
     * With {@code jdk.nio.path.useNormalizationFormD} on macOS the JDK encodes a path's text in form D (and shows it in
     * form C), so the binding must encode form D too; without it, the text as it is. The JDK reads the property at
     * start-up, so the conversion is tested with the setting passed in.
     */
    @Test
    void anAccentedNameIsEncodedAsTheJdkDoesUnderEitherNormalizationSetting() throws Exception {
        String composed = "journal-caf\u00e9"; // as Path.toString() shows it in either setting
        String decomposed = "journal-cafe\u0301";
        byte[] nfc = composed.getBytes(StandardCharsets.UTF_8);
        byte[] nfd = decomposed.getBytes(StandardCharsets.UTF_8);
        assertFalse(java.util.Arrays.equals(nfc, nfd));

        assertArrayEquals(nfd, Paths.encode(composed, StandardCharsets.UTF_8, true));
        assertArrayEquals(nfd, Paths.encode(decomposed, StandardCharsets.UTF_8, true));
        assertArrayEquals(nfc, Paths.encode(composed, StandardCharsets.UTF_8, false));
        assertArrayEquals(nfd, Paths.encode(decomposed, StandardCharsets.UTF_8, false));
        byte[] ascii = "journal-a".getBytes(StandardCharsets.UTF_8);
        assertArrayEquals(ascii, Paths.encode("journal-a", StandardCharsets.UTF_8, true));
    }

    /** The property is on when empty or {@code true} (any case), as the JDK reads it, and only on macOS. */
    @Test
    void theNormalizationPropertyIsReadAsTheJdkReadsIt() {
        assertTrue(Paths.normalizationProperty(""));
        assertTrue(Paths.normalizationProperty("true"));
        assertTrue(Paths.normalizationProperty("TRUE"));
        assertFalse(Paths.normalizationProperty(null));
        assertFalse(Paths.normalizationProperty("false"));
        assertFalse(Paths.normalizationProperty("yes"));
        boolean mac = System.getProperty("os.name").startsWith("Mac");
        boolean set = Paths.normalizationProperty(System.getProperty("jdk.nio.path.useNormalizationFormD"));
        assertEquals(mac && set, Paths.NORMALIZE_TO_NFD);
    }

    /** End to end in this JVM: the bytes the binding uses for an accented path are those of the setting in effect. */
    @Test
    void anAccentedPathConvertsWithTheSettingInEffect() throws Exception {
        String name = "journal-caf\u00e9";
        assumeTrue(Paths.FILE_NAMES.newEncoder().canEncode(name), "the file-system encoding cannot represent " + name);
        Path p = tmp.resolve(name);
        byte[] expected = Paths.encode(p.toString(), Paths.FILE_NAMES, Paths.NORMALIZE_TO_NFD);
        assertArrayEquals(expected, Paths.nativeBytes(p));
    }
}
