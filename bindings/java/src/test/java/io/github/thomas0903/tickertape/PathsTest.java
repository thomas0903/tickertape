package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.AccessDeniedException;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.InvalidPathException;
import java.nio.file.NoSuchFileException;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
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

    /**
     * The bytes of {@code rel}'s absolute path after those of the working directory and the separator: the relative
     * path's own bytes. Asserts that the absolute bytes start with the working directory's.
     */
    private static byte[] tailOfAbsolute(Path rel) {
        byte[] cwd = Paths.nativeBytes(Path.of("").toAbsolutePath());
        byte[] absolute = Paths.nativeBytes(rel.toAbsolutePath());
        assertEquals('/', cwd[0]);
        assertArrayEquals(cwd, Arrays.copyOf(absolute, cwd.length), "the absolute bytes start with the working directory's");
        int from = cwd.length == 1 ? 1 : cwd.length + 1;
        return Arrays.copyOfRange(absolute, from, absolute.length);
    }

    /** {@code tmp} relative to the working directory, if both are ASCII (the relative path then holds {@code ..}). */
    private Path tmpRelative() {
        Path cwd = Path.of("").toAbsolutePath();
        assumeTrue(StandardCharsets.US_ASCII.newEncoder().canEncode(cwd + tmp.toString()), "ASCII directories");
        Path rel = cwd.relativize(tmp);
        assertFalse(rel.isAbsolute(), rel.toString());
        return rel;
    }

    @Test
    void aRelativePathIsPassedRelative() {
        assertArrayEquals(utf8("rel/journal"), Paths.nativeBytes(Path.of("rel/journal")));
        assertArrayEquals(utf8("../rel/journal"), Paths.nativeBytes(Path.of("../rel/journal")));
        assertArrayEquals(utf8("rel/../journal"), Paths.nativeBytes(Path.of("rel/../journal")));
        assertArrayEquals(utf8("./journal"), Paths.nativeBytes(Path.of("./journal")));
        for (String text : List.of("rel/journal", "../rel/journal", "rel/../journal", "./journal")) {
            assertArrayEquals(tailOfAbsolute(Path.of(text)), Paths.nativeBytes(Path.of(text)), text);
        }
    }

    /** The empty path is the working directory, an existing directory whose URI ends in a slash. */
    @Test
    void theEmptyPathIsPassedAsADot() {
        assertTrue(Path.of("").toUri().getRawPath().endsWith("/"), Path.of("").toUri().toString());
        assertArrayEquals(utf8("."), Paths.nativeBytes(Path.of("")));
        assertArrayEquals(utf8("."), Paths.nativeBytes(Path.of(".")));
    }

    /** As {@link #anAccentedNameIsPassedAsTheJdkStoresIt}, relative: form C, or form D under macOS's property. */
    @Test
    void aRelativeAccentedNameIsPassedByItsOwnBytes() {
        String name = "journal-café-中";
        Path rel;
        try {
            rel = Path.of("rel", name);
        } catch (InvalidPathException e) {
            assumeTrue(false, "the file-system encoding cannot represent " + name + ": " + e.getMessage());
            return;
        }
        byte[] bytes = Paths.nativeBytes(rel);
        assertArrayEquals(tailOfAbsolute(rel), bytes);
        if (MAC || "UTF-8".equals(System.getProperty("sun.jnu.encoding"))) {
            assertArrayEquals(concat(utf8("rel/"), utf8(FORM_D ? "journal-cafe\u0301-\u4e2d" : name)), bytes);
        }
    }

    @Test
    void aRelativeNameContainingTheReplacementCharacterIsPassed() {
        Path rel = Path.of("rel", "j�");
        byte[] expected = {'r', 'e', 'l', '/', 'j', (byte) 0xEF, (byte) 0xBF, (byte) 0xBD};
        assertArrayEquals(expected, Paths.nativeBytes(rel));
        assertArrayEquals(tailOfAbsolute(rel), Paths.nativeBytes(rel));
    }

    @Test
    void aJournalOpensThroughARelativePath() throws Exception {
        Path rel = tmpRelative().resolve("journal-rel");
        assertArrayEquals(utf8(rel.toString()), Paths.nativeBytes(rel));
        journal(rel, 42);
        assertEquals(List.of(HexFormat.of().formatHex(utf8("journal-rel"))), namesOnDisk());
        assertEquals(42, firstPayload(tmp.resolve("journal-rel")));
        assertEquals(42, firstPayload(rel));
        assertEquals(1, Journal.verify(rel).records());
    }

    /**
     * A forked JVM whose working directory is four bytes short of the platform's path-length limit opens
     * {@code Path.of("journal")}, whose absolute path is over it, as the JDK does; the absolute path fails in the library
     * with a name-too-long error. A fork, so that this JVM's working directory does not change.
     */
    @Test
    void aShortRelativePathOpensFromAWorkingDirectoryNearThePathLengthLimit() throws Exception {
        String os = System.getProperty("os.name");
        assumeTrue(MAC || os.equals("Linux"), "a known path-length limit: " + os);
        int limit = MAC ? 1024 : 4096; // PATH_MAX, including the NUL
        int target = limit - 4;
        Path deep = tmp.toRealPath(); // getcwd() in the child returns the real path
        while (deep.toString().length() < target) {
            int rest = target - deep.toString().length(); // a "/" and a name
            int name = rest - 1 <= 200 ? rest - 1 : Math.min(200, rest - 3);
            deep = deep.resolve("d".repeat(name));
        }
        assertEquals(target, deep.toString().length());
        Files.createDirectories(deep);
        Path program = tmp.resolve("Deep.java");
        Files.writeString(program, """
                import io.github.thomas0903.tickertape.*;
                import java.nio.file.*;
                public class Deep {
                    public static void main(String[] args) throws Exception {
                        Path rel = Path.of("journal");
                        System.out.println("cwd " + Path.of("").toAbsolutePath().toString().length());
                        try (Writer w = Writer.open(rel, WriterOptions.defaults().segmentSize(65_536).prefault(false))) {
                            w.append(1, 0, 0, new byte[] {42});
                        }
                        try (Reader r = Reader.open(rel)) {
                            System.out.println("payload " + r.tryNext().getByte(0));
                        }
                        System.out.println("records " + Journal.verify(rel).records());
                        try {
                            Reader.open(rel.toAbsolutePath()).close();
                            System.out.println("absolute opened");
                        } catch (TickertapeException e) {
                            System.out.println("absolute " + e.getClass().getSimpleName());
                        }
                        // Through relative paths: the absolute ones are too long for the parent's clean-up.
                        try (var files = Files.list(rel)) {
                            for (Path f : files.toList()) {
                                Files.delete(f);
                            }
                        }
                        Files.delete(rel);
                        System.out.println("deleted");
                    }
                }
                """);
        TestSupport.Result r = TestSupport.run(List.of(TestSupport.javaLauncher(), "--enable-native-access=ALL-UNNAMED",
                "--illegal-native-access=deny", "-Dtickertape.library.path=" + TestSupport.LIBRARY.toAbsolutePath(),
                "--class-path", TestSupport.CLASSES.toAbsolutePath().toString(), program.toString()), deep, 120);
        assertEquals(0, r.exit(), "stdout:\n" + r.stdout() + "\nstderr:\n" + r.stderr());
        assertEquals(List.of("cwd " + target, "payload 42", "records 1", "absolute JournalIoException", "deleted"),
                TestSupport.lines(r.stdout()), r.stderr());
    }

    /** Directories with these identities; an {@link IOException} is thrown instead of returned. */
    private static Paths.Directories dirs(Object empty, Object defaultDirectory, Object workingDirectory) {
        return new Paths.Directories() {
            @Override
            public Object empty() throws IOException {
                return identity(empty);
            }

            @Override
            public Object defaultDirectory() throws IOException {
                return identity(defaultDirectory);
            }

            @Override
            public Object workingDirectory(byte[] processCwd) throws IOException {
                return identity(workingDirectory);
            }
        };
    }

    private static Object identity(Object value) throws IOException {
        if (value instanceof IOException e) {
            throw e;
        }
        return value;
    }

    /** Directories that must not be looked at: the byte strings decide. */
    private static final Paths.Directories UNASKED = new Paths.Directories() {
        @Override
        public Object empty() {
            throw new AssertionError("stat of the empty path");
        }

        @Override
        public Object defaultDirectory() {
            throw new AssertionError("stat of the default directory");
        }

        @Override
        public Object workingDirectory(byte[] processCwd) {
            throw new AssertionError("stat of the working directory");
        }
    };

    private static Boolean decide(String cwd, String defaultDirectory, Paths.Directories dirs) {
        return Paths.passRelative(utf8(cwd), utf8(defaultDirectory), false, dirs);
    }

    /**
     * The decision as a function of {@code getcwd}'s bytes now, the default directory's bytes, the JDK's
     * {@code chdirAllowed} property and the identities of the directories the JDK's {@code stat} of {@code Path.of("")},
     * the default directory's path and {@code getcwd}'s path reach: {@code TRUE} relative, {@code FALSE} absolute,
     * {@code null} undetermined (passed absolute, not kept).
     */
    @Test
    void aRelativePathIsPassedRelativeExactlyWhenTheJdkPassesItAsItIs() {
        // The byte strings are equal: the JVM's working directory is where it started.
        assertEquals(true, decide("/a/b", "/a/b", UNASKED));
        assertEquals(true, decide("/a/b", "/a/b/", UNASKED), "the default directory's URI ends in a slash");
        assertEquals(true, decide("/", "/", UNASKED));
        assertEquals(true, Paths.passRelative(new byte[] {'/', 'j', (byte) 0xFF}, new byte[] {'/', 'j', (byte) 0xFF},
                false, UNASKED));
        assertEquals(false, Paths.passRelative(utf8("/a"), utf8("/a"), true, UNASKED),
                "sun.nio.fs.chdirAllowed: the JDK resolves every path");
        // They differ: the JDK's own stat of the empty path decides.
        assertEquals(false, decide("/a", "/b", dirs("B", "B", "A")), "-Duser.dir=/b in /a");
        assertEquals(false, decide("/a", "/a/b", dirs("AB", "AB", "A")));
        assertEquals(false, decide("/a", "/link-to-a", dirs("A", "A", "A")),
                "-Duser.dir= naming the working directory through a symbolic link: the JDK resolves against it");
        assertEquals(false, decide("/a/café", "/a/cafe\u0301", dirs("C", "C", "C")),
                "the same name in another form, which the JDK resolves against");
        assertEquals(true, decide("/work/b", "/work/a", dirs("B", new NoSuchFileException("/work/a"), "B")),
                "started in /work/a, renamed to /work/b: the JDK's stat of . reaches it and /work/a is gone");
        assertEquals(true, decide("/work/b", "/work/a", dirs("B", "A2", "B")),
                "renamed, and /work/a made again: it names another directory");
        assertEquals(true, decide("/work/b", "/work/a", dirs("B", new AccessDeniedException("/work"), "B")));
        assertEquals(false, decide("/work/b", "/work/a", dirs("B", "B", "B")),
                "renamed, and /work/a made to name it again: both forms reach it");
        // Undetermined.
        assertEquals(null, Paths.passRelative(null, utf8("/a"), false, UNASKED), "getcwd failed");
        assertEquals(null, decide("(unreachable)/a", "/a", UNASKED), "Linux's mark for a directory outside the root");
        assertEquals(null, Paths.passRelative(new byte[0], utf8("/"), false, UNASKED));
        assertEquals(null, decide("/a", "/b", dirs(new NoSuchFileException("/b"), "B", "A")), "user.dir is gone");
        assertEquals(null, decide("/a", "/b", dirs("B", "B", new AccessDeniedException("/a"))));
        assertEquals(null, decide("/a", "/b", dirs(null, "B", "A")), "no file key");
        assertEquals(null, decide("/a", "/b", dirs("A", null, "A")), "no file key");
    }

    /** {@code sun.nio.fs.chdirAllowed} as the JDK parses it. */
    @Test
    void theChdirPropertyIsReadAsTheJdkReadsIt() {
        assertTrue(Paths.chdirAllowed(""));
        assertTrue(Paths.chdirAllowed("true"));
        assertTrue(Paths.chdirAllowed("TRUE"));
        assertFalse(Paths.chdirAllowed("false"));
        assertFalse(Paths.chdirAllowed("yes"));
    }

    /**
     * The real probes in this JVM, whose working directory is its default directory: the byte strings agree, and a
     * working directory that names another directory makes the JDK's stat of the empty path (of {@code .}) differ.
     */
    @Test
    void theJdkProbesReachTheDirectoriesTheyName() throws Exception {
        byte[] defaultDirectory = Paths.nativeBytes(Path.of("").toAbsolutePath());
        Paths.Directories jdk = Paths.JDK_DIRECTORIES;
        assertEquals(true, Paths.passRelative(Paths.processWorkingDirectory(1024), defaultDirectory, false, jdk));
        assertEquals(false, Paths.passRelative(Paths.nativeBytes(tmp.toRealPath()), defaultDirectory, false, jdk));
        assertEquals(jdk.empty(), jdk.workingDirectory(Paths.processWorkingDirectory(1024)));
        assertEquals(jdk.empty(), jdk.defaultDirectory());
        byte[] odd = concat(Paths.nativeBytes(tmp), utf8("/j%: é"));
        assertArrayEquals(odd, Paths.nativeBytes(Paths.fromBytes(odd)), "a path made from bytes keeps them");
        if (!MAC) {
            byte[] undecodable = concat(Paths.nativeBytes(tmp), new byte[] {'/', 'j', (byte) 0xFF});
            assertArrayEquals(undecodable, Paths.nativeBytes(Paths.fromBytes(undecodable)));
        }
    }

    /**
     * A determination that fails is not kept: that conversion passes the path absolute and the next one tries again;
     * once one succeeds it is kept, and the working directory is not asked for again. An absolute path never asks.
     */
    @Test
    void aFailedDeterminationIsNotKept() {
        Paths.Decision decision = new Paths.Decision();
        Path rel = Path.of("rel", "journal");
        byte[] absolute = Paths.nativeBytes(rel.toAbsolutePath());
        assertArrayEquals(absolute, Paths.nativeBytes(rel, () -> decision.passRelative(() -> null)),
                "getcwd failed (native access refused, say): absolute");
        assertEquals(null, decision.kept());
        assertArrayEquals(absolute, Paths.nativeBytes(rel, () -> decision.passRelative(() -> utf8("(unreachable)/a"))));
        assertEquals(null, decision.kept());
        assertArrayEquals(utf8("rel/journal"),
                Paths.nativeBytes(rel, () -> decision.passRelative(() -> Paths.processWorkingDirectory(1024))));
        assertEquals(true, decision.kept());
        assertArrayEquals(utf8("rel/journal"), Paths.nativeBytes(rel, () -> decision.passRelative(() -> {
            throw new AssertionError("asked again after a determination succeeded");
        })));
        assertArrayEquals(absolute, Paths.nativeBytes(rel.toAbsolutePath(), () -> {
            throw new AssertionError("asked for an absolute path");
        }));
    }

    /**
     * {@code getcwd}, called through the FFM linker, returns this JVM's working directory, which the build does not
     * move from the JDK's default directory; a buffer of one byte grows on {@code ERANGE} until it fits.
     */
    @Test
    void getcwdReturnsTheWorkingDirectoryAndGrowsItsBuffer() {
        byte[] defaultDirectory = Paths.nativeBytes(Path.of("").toAbsolutePath());
        assertArrayEquals(defaultDirectory, Paths.processWorkingDirectory(1024));
        assertArrayEquals(defaultDirectory, Paths.processWorkingDirectory(1));
        assertArrayEquals(defaultDirectory, Paths.processWorkingDirectory(defaultDirectory.length));
        assertTrue(Paths.passesRelative());
    }

    /**
     * A forked JVM started in one directory with {@code -Duser.dir=} naming another, each holding a journal with a
     * different payload and a marker file. The JDK resolves {@code Path.of("journal")} under {@code user.dir}, so its
     * own {@code Files.list} and {@code Files.exists} see that directory; the binding must open the journal there too,
     * not the one in the process's working directory. A fork, so that this JVM's directories do not change.
     */
    @Test
    void aRelativePathFollowsTheJdkWhenUserDirIsSetAtStartUp() throws Exception {
        Path started = Files.createDirectory(tmp.toRealPath().resolve("started"));
        Path userDir = Files.createDirectory(tmp.toRealPath().resolve("user-dir"));
        journal(started.resolve("journal"), 1);
        journal(userDir.resolve("journal"), 2);
        Files.createFile(started.resolve("marker-started"));
        Files.createFile(userDir.resolve("marker-user-dir"));
        Path program = tmp.resolve("UserDir.java");
        Files.writeString(program, """
                import io.github.thomas0903.tickertape.*;
                import java.nio.file.*;
                public class UserDir {
                    public static void main(String[] args) throws Exception {
                        Path rel = Path.of("journal");
                        try (var names = Files.list(Path.of(""))) {
                            System.out.println("listed " + names.map(p -> p.getFileName().toString()).sorted().toList());
                        }
                        System.out.println("exists " + Files.exists(Path.of("marker-user-dir"))
                                + " " + Files.exists(Path.of("marker-started")));
                        try (Reader r = Reader.open(rel)) {
                            System.out.println("payload " + r.tryNext().getByte(0));
                        }
                        try (Reader r = Reader.openMappedUnsafe(rel)) {
                            System.out.println("mapped " + r.tryNext().getByte(0));
                        }
                        System.out.println("records " + Journal.verify(rel).records());
                        try (Writer w = Writer.open(rel, WriterOptions.defaults().segmentSize(65_536).prefault(false))) {
                            w.append(1, 0, 0, new byte[] {3});
                        }
                        System.out.println("records " + Journal.verify(Path.of("journal")).records());
                    }
                }
                """);
        TestSupport.Result r = TestSupport.run(List.of(TestSupport.javaLauncher(), "-Duser.dir=" + userDir,
                "--enable-native-access=ALL-UNNAMED", "--illegal-native-access=deny",
                "-Dtickertape.library.path=" + TestSupport.LIBRARY.toAbsolutePath(),
                "--class-path", TestSupport.CLASSES.toAbsolutePath().toString(), program.toString()), started, 120);
        assertEquals(0, r.exit(), "stdout:\n" + r.stdout() + "\nstderr:\n" + r.stderr());
        assertEquals(List.of("listed [journal, marker-user-dir]", "exists true false", "payload 2", "mapped 2",
                "records 1", "records 2"), TestSupport.lines(r.stdout()), r.stderr());
        try (Reader reader = Reader.open(started.resolve("journal"))) {
            assertEquals(1, reader.tryNext().getByte(0));
            assertEquals(null, reader.tryNext(), "the journal in the process's working directory is unchanged");
        }
    }

    /**
     * A forked JVM started in {@code a} renames its own working directory to {@code b}, and moves another journal to
     * {@code a}, before its first binding call. The JDK passes {@code Path.of("journal")} to the operating system as it
     * is, so its own {@code Files.exists} sees {@code b}; the binding must open the journal there too, not the one now
     * at the path the JVM started in. A fork, so that this JVM's working directory does not move.
     */
    @Test
    void aRelativePathFollowsTheJdkWhenTheWorkingDirectoryIsRenamed() throws Exception {
        Path started = Files.createDirectory(tmp.toRealPath().resolve("a"));
        Path other = Files.createDirectory(tmp.toRealPath().resolve("other"));
        journal(started.resolve("journal"), 1);
        journal(other.resolve("journal"), 9);
        Files.createFile(started.resolve("marker-started"));
        Path program = tmp.resolve("Renamed.java");
        Files.writeString(program, """
                import io.github.thomas0903.tickertape.*;
                import java.nio.file.*;
                public class Renamed {
                    public static void main(String[] args) throws Exception {
                        Path here = Path.of("").toAbsolutePath();
                        Files.move(here, here.resolveSibling("b"));
                        Files.move(here.resolveSibling("other"), here);
                        Path rel = Path.of("journal");
                        System.out.println("exists " + Files.exists(Path.of("marker-started")));
                        try (Reader r = Reader.open(rel)) {
                            System.out.println("payload " + r.tryNext().getByte(0));
                        }
                        try (Reader r = Reader.openMappedUnsafe(rel)) {
                            System.out.println("mapped " + r.tryNext().getByte(0));
                        }
                        System.out.println("records " + Journal.verify(rel).records());
                        try (Writer w = Writer.open(rel, WriterOptions.defaults().segmentSize(65_536).prefault(false))) {
                            w.append(1, 0, 0, new byte[] {3});
                        }
                        System.out.println("records " + Journal.verify(Path.of("journal")).records());
                    }
                }
                """);
        TestSupport.Result r = TestSupport.run(List.of(TestSupport.javaLauncher(), "--enable-native-access=ALL-UNNAMED",
                "--illegal-native-access=deny", "-Dtickertape.library.path=" + TestSupport.LIBRARY.toAbsolutePath(),
                "--class-path", TestSupport.CLASSES.toAbsolutePath().toString(), program.toString()), started, 120);
        assertEquals(0, r.exit(), "stdout:\n" + r.stdout() + "\nstderr:\n" + r.stderr());
        assertEquals(List.of("exists true", "payload 1", "mapped 1", "records 1", "records 2"),
                TestSupport.lines(r.stdout()), r.stderr());
        assertEquals(2, Journal.verify(tmp.resolve("b").resolve("journal")).records());
        try (Reader reader = Reader.open(started.resolve("journal"))) {
            assertEquals(9, reader.tryNext().getByte(0));
            assertEquals(null, reader.tryNext(), "the journal now at the path the JVM started in is unchanged");
        }
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

        // Relative, the name keeps its own bytes, the tail of the absolute ones, and reaches the same journal.
        Path relBad = tmpRelative().resolve(bad.getFileName());
        byte[] relBytes = concat(utf8(tmpRelative().toString()), new byte[] {'/', 'j', (byte) 0xFF});
        assertArrayEquals(relBytes, Paths.nativeBytes(relBad));
        assertArrayEquals(tailOfAbsolute(relBad), Paths.nativeBytes(relBad));
        assertEquals(42, firstPayload(relBad));
        assertEquals(1, Journal.verify(relBad).records());
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
