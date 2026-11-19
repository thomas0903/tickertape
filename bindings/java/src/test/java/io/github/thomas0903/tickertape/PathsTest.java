package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.io.ByteArrayOutputStream;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.InvalidPathException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import java.util.stream.Stream;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Paths reach the C library as the bytes the JDK itself uses for them, read from {@link Path#toUri()} (ADR 0037 &rarr;
 * Paths). Each test asserts the exact bytes {@link Paths#nativeBytes} returns. The URI form these rely on is the default
 * provider's behaviour, not the specification's, so these tests are what detects a JDK that changes it. The build runs
 * this class a second time with {@code -Djdk.nio.path.useNormalizationFormD=true} (see {@code pom.xml}).
 */
class PathsTest {
    @TempDir
    Path tmp;

    private static final boolean MAC = System.getProperty("os.name").startsWith("Mac");
    /** Whether this JVM runs with macOS's form-D property (the JDK's test: empty or {@code true}). */
    private static final boolean FORM_D = MAC && formDProperty(System.getProperty("jdk.nio.path.useNormalizationFormD"));

    private static boolean formDProperty(String value) {
        return value != null && (value.isEmpty() || Boolean.parseBoolean(value));
    }

    private static void journal(Path dir, int payload) {
        try (Writer w = Writer.open(dir, TestSupport.small())) {
            w.append(1, 0, 0, new byte[] {(byte) payload});
        }
    }

    private static int firstPayload(Path dir) {
        try (Reader r = Reader.open(dir)) {
            return r.tryNext().getByte(0);
        }
    }

    private static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    private static byte[] concat(byte[]... parts) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (byte[] part : parts) {
            out.writeBytes(part);
        }
        return out.toByteArray();
    }

    /** The bytes of {@code tmp} followed by {@code /} and {@code leaf}. */
    private byte[] inTmp(byte[] leaf) {
        return concat(Paths.nativeBytes(tmp), new byte[] {'/'}, leaf);
    }

    /** The entry of {@code tmp} whose name the JVM shows as {@code shown}, as the directory listing returns it. */
    private Path listed(String shown) throws Exception {
        try (Stream<Path> entries = Files.list(tmp)) {
            List<Path> matches = entries.filter(p -> p.getFileName().toString().equals(shown)).toList();
            return matches.isEmpty() ? null : matches.get(0);
        }
    }

    /** The names in {@code tmp} as the file system stores them, read by the shell, not the JVM, in hex. */
    private List<String> namesOnDisk() throws Exception {
        TestSupport.Result r = TestSupport.runOk(List.of("sh", "-c",
                "cd -- \"$1\" && for f in * .[!.]*; do [ -e \"$f\" ] && printf '%s' \"$f\" | od -An -v -tx1 | tr -d ' \\n'"
                        + " && echo; done; true", "sh", tmp.toString()), 30);
        List<String> names = new ArrayList<>();
        for (String line : r.stdout().split("\n")) {
            if (!line.isBlank()) {
                names.add(line.trim());
            }
        }
        return names;
    }

    @Test
    void theTemporaryDirectoryIsAbsoluteAsciiAndHasNoTrailingSlash() {
        String text = tmp.toAbsolutePath().toString();
        assumeTrue(StandardCharsets.US_ASCII.newEncoder().canEncode(text), "an ASCII temporary directory: " + text);
        assertArrayEquals(text.getBytes(StandardCharsets.US_ASCII), Paths.nativeBytes(tmp));
    }

    @Test
    void anAsciiAbsolutePathIsPassedAsItIs() {
        Path p = tmp.resolve("journal-a");
        assertArrayEquals(inTmp(utf8("journal-a")), Paths.nativeBytes(p));
        journal(p, 42);
        assertEquals(42, firstPayload(p));
    }

    @Test
    void aRelativePathBecomesAbsoluteAgainstTheWorkingDirectory() {
        byte[] cwd = Paths.nativeBytes(Path.of(System.getProperty("user.dir")));
        assertEquals('/', cwd[0]);
        assertArrayEquals(concat(cwd, utf8("/rel/journal")), Paths.nativeBytes(Path.of("rel/journal")));
        assertArrayEquals(cwd, Paths.nativeBytes(Path.of("")));
    }

    /** {@code toUri()} appends a slash to an existing directory; the bytes passed do not depend on it. */
    @Test
    void anExistingDirectoryAndAMissingPathHaveNoTrailingSlash() throws Exception {
        Path existing = Files.createDirectory(tmp.resolve("exists"));
        assertTrue(existing.toUri().getRawPath().endsWith("/exists/"), existing.toUri().toString());
        assertArrayEquals(inTmp(utf8("exists")), Paths.nativeBytes(existing));
        Path missing = tmp.resolve("missing");
        assertFalse(missing.toUri().getRawPath().endsWith("/"), missing.toUri().toString());
        assertArrayEquals(inTmp(utf8("missing")), Paths.nativeBytes(missing));
    }

    /**
     * A path made from {@code file:///tmp/journal%2F} keeps the trailing slash in its bytes ({@code toString()} shows
     * it, and {@code Path.of(p.toString())} would strip it). The binding passes the directory without it.
     */
    @Test
    void aPathWithATrailingSlashIsPassed() {
        Path p = Path.of(URI.create("file:///tmp/journal%2F"));
        assertEquals("/tmp/journal/", p.toString());
        assertArrayEquals(utf8("/tmp/journal"), Paths.nativeBytes(p));

        Path inTmp = Path.of(URI.create(tmp.toUri() + "journal%2F"));
        assertTrue(inTmp.toString().endsWith("/journal/"), inTmp.toString());
        assertArrayEquals(inTmp(utf8("journal")), Paths.nativeBytes(inTmp));
        journal(inTmp, 42);
        assertEquals(42, firstPayload(tmp.resolve("journal")));
    }

    @Test
    void reservedUriCharactersArePassedAsTheyAre() throws Exception {
        String name = "a b%c#d?e;f=g&h+i~j'k@l";
        Path p = tmp.resolve(name);
        assertArrayEquals(inTmp(utf8(name)), Paths.nativeBytes(p));
        journal(p, 42);
        assertEquals(List.of(HexFormat.of().formatHex(utf8(name))), namesOnDisk());
        assertEquals(42, firstPayload(p));
    }

    @Test
    void theRootIsPassedAsASlash() {
        assertArrayEquals(utf8("/"), Paths.nativeBytes(Path.of("/")));
        assertArrayEquals(utf8("/"), Paths.nativeBytes(Path.of(URI.create("file:///"))));
    }

    /** A name that holds U+FFFD itself (the bytes EF BF BD) is valid UTF-8 and passed as it is. */
    @Test
    void aNameContainingTheReplacementCharacterIsPassed() throws Exception {
        String name = "j�";
        Path p = tmp.resolve(name);
        assertArrayEquals(inTmp(new byte[] {'j', (byte) 0xEF, (byte) 0xBF, (byte) 0xBD}), Paths.nativeBytes(p));
        journal(p, 42);
        assertEquals(List.of("6aefbfbd"), namesOnDisk());
        assertEquals(42, firstPayload(p));
    }

    /**
     * An accented name made from a string: the bytes must be those the JDK itself uses, which the test reads back from
     * the file system, by the shell, after the JDK created the directory. They are form C, or form D under macOS's
     * {@code jdk.nio.path.useNormalizationFormD} (the build runs this class with and without it). The journal the
     * binding then opens there is in that directory: nothing else appears in {@code tmp}.
     */
    @Test
    void anAccentedNameIsPassedAsTheJdkStoresIt() throws Exception {
        String name = "journal-café-中";
        byte[] nfc = utf8(name);
        byte[] nfd = utf8("journal-café-中");
        Path dir;
        try {
            dir = Files.createDirectory(tmp.resolve(name));
        } catch (InvalidPathException e) {
            assumeTrue(false, "the file-system encoding cannot represent " + name + ": " + e.getMessage());
            return;
        }
        List<String> onDisk = namesOnDisk();
        assertEquals(1, onDisk.size(), onDisk.toString());
        byte[] jdkBytes = HexFormat.of().parseHex(onDisk.get(0));
        if (MAC) { // APFS stores the name as given; elsewhere the locale may not be UTF-8
            assertArrayEquals(FORM_D ? nfd : nfc, jdkBytes);
        }
        assertArrayEquals(inTmp(jdkBytes), Paths.nativeBytes(dir));

        journal(dir, 42);
        assertEquals(onDisk, namesOnDisk(), "the journal went into the directory the JDK created");
        try (Stream<Path> files = Files.list(dir)) {
            assertFalse(files.toList().isEmpty(), "the journal's files are in it");
        }
        Path asListed = listed(dir.getFileName().toString());
        assertNotNull(asListed, "listed");
        assertArrayEquals(inTmp(jdkBytes), Paths.nativeBytes(asListed));
        assertEquals(42, firstPayload(asListed));
        assertEquals(1, Journal.verify(asListed).records());
    }

    /**
     * On macOS a name stored in form C keeps those bytes when a directory listing returns it, under either setting.
     * The binding passes them as they are; under the form-D property a path made from the same string holds form D
     * bytes instead, and is passed as that (APFS does not tell the two forms apart, so both open the same journal).
     */
    @Test
    void aListedNameInFormCIsPassedAsItIsUnderEitherSetting() throws Exception {
        assumeTrue(MAC, "the property exists only on macOS");
        String name = "journal-café";
        TestSupport.runOk(List.of("sh", "-c", "mkdir -- \"$1/$(printf 'journal-caf\\303\\251')\"", "sh",
                tmp.toString()), 30);
        Path asListed = listed(name);
        assertNotNull(asListed, "listed");
        assertArrayEquals(inTmp(utf8(name)), Paths.nativeBytes(asListed));
        journal(asListed, 42);
        assertEquals(List.of(HexFormat.of().formatHex(utf8(name))), namesOnDisk());
        assertEquals(42, firstPayload(asListed));

        Path fromString = tmp.resolve(name);
        assertArrayEquals(inTmp(utf8(FORM_D ? "journal-café" : name)), Paths.nativeBytes(fromString));
    }

    /**
     * On Linux a name can hold bytes that are not valid in the file-system encoding (here 0xFF under UTF-8). The JVM
     * shows it as {@code j�}, which is also the text of a different, valid name (EF BF BD). The binding passes the
     * name's own bytes, so every call reaches the journal with that name, not its decoded twin. Skipped where the file
     * system refuses such a name (macOS) or the JVM decodes every byte (a single-byte locale).
     */
    @Test
    void aNameTheJvmCannotDecodeIsPassedByItsOwnBytes() throws Exception {
        TestSupport.Result made = TestSupport.run(List.of("sh", "-c", "mkdir -- \"$1/$(printf 'j\\377')\"", "sh",
                tmp.toString()), 30);
        assumeTrue(made.exit() == 0, "the file system refuses a name that is not valid UTF-8: " + made.stderr());
        Path bad = listed("j�");
        assumeTrue(bad != null, "the JVM decodes the name without loss");
        Path twin = tmp.resolve("j�");
        assertFalse(twin.equals(bad), "the JVM keeps the two names apart");
        assertArrayEquals(inTmp(new byte[] {'j', (byte) 0xFF}), Paths.nativeBytes(bad));
        assertArrayEquals(inTmp(new byte[] {'j', (byte) 0xEF, (byte) 0xBF, (byte) 0xBD}), Paths.nativeBytes(twin));

        journal(twin, 7);
        journal(bad, 42);
        assertEquals(42, firstPayload(bad));
        assertEquals(7, firstPayload(twin));
        try (Reader r = Reader.openMappedUnsafe(bad)) {
            assertEquals(42, r.tryNext().getByte(0));
        }
        assertEquals(1, Journal.verify(bad).records());
        assertEquals(1, Journal.listSegments(bad).size());
        Journal.readCheckpoint(bad);
        Journal.applyRetention(bad, Retention.delete().maxSegments(0), true);
        Retention.moveTo(bad);
        try (Stream<Path> files = Files.list(bad)) {
            assertFalse(files.toList().isEmpty(), "the journal's files are in the undecodable directory");
        }
    }

    @Test
    void aPathOfAnotherFileSystemIsRefused() throws Exception {
        try (FileSystem zip = FileSystems.newFileSystem(tmp.resolve("j.zip"), Map.of("create", "true"))) {
            Path inZip = zip.getPath("/journal");
            List<Consumer<Path>> calls = List.of(
                    Paths::nativeBytes,
                    p -> Reader.open(p).close(),
                    p -> Reader.openMappedUnsafe(p).close(),
                    p -> Writer.open(p).close(),
                    Journal::verify,
                    Journal::listSegments,
                    Journal::readCheckpoint,
                    p -> Journal.applyRetention(p, Retention.delete().maxSegments(0), true),
                    Retention::moveTo);
            for (Consumer<Path> call : calls) {
                IllegalArgumentException e = assertThrows(IllegalArgumentException.class, () -> call.accept(inZip));
                assertTrue(e.getMessage().contains("default file system"), e.getMessage());
            }
        }
    }
}
