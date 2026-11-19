package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.io.IOException;
import java.nio.file.DirectoryNotEmptyException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.stream.Stream;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/**
 * Finding and loading libtickertape, in fresh JVMs: the natives JAR (extraction into a private directory, hash check,
 * clean-up), {@code java.library.path}, the system property, a missing or invalid library, and the module path.
 */
class LibraryLoadingTest {
    @TempDir
    Path tmp;

    private Path probe;

    @BeforeEach
    void writeProbe() throws IOException {
        probe = tmp.resolve("Probe.java");
        Files.writeString(probe, """
                import io.github.thomas0903.tickertape.Tickertape;
                public class Probe {
                    public static void main(String[] args) {
                        try {
                            System.out.println("version " + Tickertape.libraryVersion());
                            System.out.println("source " + Tickertape.librarySource());
                        } catch (UnsatisfiedLinkError e) {
                            System.out.println("ULE " + e.getMessage());
                            System.exit(3);
                        }
                    }
                }
                """);
    }

    private static String fileName() {
        return System.mapLibraryName("tickertape");
    }

    /** A natives JAR as the natives profile builds it, with the given hash file contents (null: none). */
    private Path nativesJar(String hash) throws IOException {
        return nativesJar(TestSupport.LIBRARY, hash);
    }

    private Path nativesJar(Path library, String hash) throws IOException {
        Path jar = tmp.resolve("natives.jar");
        String base = NativeLibrary.RESOURCE_ROOT + NativeLibrary.platform() + "/";
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry(base + fileName()));
            Files.copy(library, out);
            out.closeEntry();
            if (hash != null) {
                out.putNextEntry(new JarEntry(base + fileName() + ".sha256"));
                out.write((hash + "  " + fileName() + "\n").getBytes(StandardCharsets.US_ASCII));
                out.closeEntry();
            }
        }
        return jar;
    }

    private TestSupport.Result probe(List<String> jvmArgs, String classPath) throws Exception {
        List<String> cmd = new ArrayList<>(List.of(TestSupport.javaLauncher()));
        cmd.addAll(jvmArgs);
        if (classPath != null) {
            cmd.addAll(List.of("--class-path", classPath));
        }
        cmd.add(probe.toString());
        return TestSupport.run(cmd, 120);
    }

    private static String classes() {
        return TestSupport.CLASSES.toString();
    }

    private Path privateTmp() throws IOException {
        return Files.createDirectories(tmp.resolve("jvm-tmp"));
    }

    private static List<String> base(Path javaTmp) {
        return List.of("--enable-native-access=ALL-UNNAMED", "--illegal-native-access=deny",
                "-Djava.io.tmpdir=" + javaTmp, "-Djava.library.path=");
    }

    @Test
    void extractsFromTheNativesJarVerifiesAndCleansUp() throws Exception {
        Path javaTmp = privateTmp();
        Path jar = nativesJar(NativeLibrary.sha256(TestSupport.LIBRARY));
        TestSupport.Result r = probe(base(javaTmp), classes() + java.io.File.pathSeparator + jar);
        assertEquals(0, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("version " + NativeLibrary.VERSION), r.stdout());
        assertTrue(r.stdout().contains("the natives JAR"), r.stdout());
        assertEquals("", r.stderr(), "no warnings");
        try (Stream<Path> left = Files.list(javaTmp)) {
            assertEquals(List.of(), left.toList(), "the extracted library and its directory are deleted after loading");
        }
    }

    @Test
    void refusesALibraryWhoseHashDoesNotMatch() throws Exception {
        Path jar = nativesJar("0".repeat(64));
        TestSupport.Result r = probe(base(privateTmp()), classes() + java.io.File.pathSeparator + jar);
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("refusing to load it"), r.stdout());
    }

    @Test
    void aVerifiedLibraryThatFailsToLoadNamesTheExtractDirectoryProperty() throws Exception {
        // Stands in for a noexec temporary directory: the hash matches, but loading the extracted file fails.
        Path bogus = Files.writeString(tmp.resolve("bogus"), "not a library");
        Path jar = nativesJar(bogus, NativeLibrary.sha256(bogus));
        TestSupport.Result r = probe(base(privateTmp()), classes() + java.io.File.pathSeparator + jar);
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("-D" + NativeLibrary.EXTRACT_DIR_PROPERTY + "="), r.stdout());
    }

    @Test
    void refusesALibraryWithoutAHash() throws Exception {
        Path jar = nativesJar(null);
        TestSupport.Result r = probe(base(privateTmp()), classes() + java.io.File.pathSeparator + jar);
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("unverified"), r.stdout());
    }

    @Test
    void findsTheLibraryOnJavaLibraryPath() throws Exception {
        List<String> args = new ArrayList<>(base(privateTmp()));
        args.set(3, "-Djava.library.path=" + TestSupport.LIBRARY.getParent());
        TestSupport.Result r = probe(args, classes());
        assertEquals(0, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("source libtickertape " + NativeLibrary.VERSION + " from java.library.path"), r.stdout());
    }

    @Test
    void explainsWhatItTriedWhenNothingIsFound() throws Exception {
        TestSupport.Result r = probe(base(privateTmp()), classes());
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("cannot find libtickertape"), r.stdout());
        assertTrue(r.stdout().contains("tickertape.library.path"), r.stdout());
    }

    @Test
    void refusesAFileThatIsNotTheLibrary() throws Exception {
        Path bogus = Files.writeString(tmp.resolve(fileName()), "not a library");
        List<String> args = new ArrayList<>(base(privateTmp()));
        args.add("-Dtickertape.library.path=" + bogus);
        TestSupport.Result r = probe(args, classes());
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("cannot load libtickertape"), r.stdout());
    }

    @Test
    void worksOnTheModulePathWithItsOwnNativeAccessFlag() throws Exception {
        List<String> args = List.of("--module-path", classes(), "--add-modules", "io.github.thomas0903.tickertape",
                "--enable-native-access=io.github.thomas0903.tickertape",
                "-Dtickertape.library.path=" + TestSupport.LIBRARY);
        TestSupport.Result r = probe(args, null);
        assertEquals(0, r.exit(), r.stdout() + r.stderr());
        assertEquals("", r.stderr(), "no restricted-method warnings");
        // Without the flag the JDK warns (and future JDKs will refuse): the flag is what users must pass.
        TestSupport.Result unflagged = probe(List.of("--module-path", classes(), "--add-modules",
                "io.github.thomas0903.tickertape", "-Dtickertape.library.path=" + TestSupport.LIBRARY), null);
        assertTrue(unflagged.stderr().contains("restricted method"), unflagged.stderr());
    }

    @Test
    void loadLibraryAcceptsTheSamePathAndRefusesAnother() {
        Tickertape.loadLibrary(TestSupport.LIBRARY);
        assertThrows(IllegalStateException.class, () -> Tickertape.loadLibrary(tmp.resolve("other").resolve(fileName())));
        assertTrue(Tickertape.librarySource().startsWith("libtickertape " + NativeLibrary.VERSION));
    }

    @Test
    void theNativesJarWinsOverJavaLibraryPath() throws Exception {
        List<String> args = new ArrayList<>(base(privateTmp()));
        args.set(3, "-Djava.library.path=" + TestSupport.LIBRARY.getParent());
        Path jar = nativesJar(NativeLibrary.sha256(TestSupport.LIBRARY));
        TestSupport.Result r = probe(args, classes() + java.io.File.pathSeparator + jar);
        assertEquals(0, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("source libtickertape " + NativeLibrary.VERSION + " from the natives JAR"),
                r.stdout());
    }

    /** A C compiler, or a skipped test (a failure when {@code tickertape.test.requireTools} is true). */
    private static String cc() {
        String cc = System.getenv().getOrDefault("CC", "cc");
        try {
            Process p = new ProcessBuilder(cc, "--version").redirectErrorStream(true).start();
            p.getInputStream().readAllBytes();
            if (p.waitFor() == 0) {
                return cc;
            }
        } catch (IOException | InterruptedException e) {
            // fall through
        }
        if (TestSupport.REQUIRE_TOOLS) {
            fail("no C compiler (" + cc + ")");
        }
        assumeTrue(false, "no C compiler (" + cc + ")");
        return null;
    }

    @Test
    void aLibraryOfAnotherVersionGetsNoHintAboutTheExtractDirectory() throws Exception {
        Path source = Files.writeString(tmp.resolve("other.c"), """
                int tickertape_abi_version(void) { return 0; }
                int tickertape_version_number(void) { return 9000; }
                const char *tickertape_version(void) { return "0.9.0"; }
                """);
        Path other = Files.createDirectories(tmp.resolve("other")).resolve(fileName());
        TestSupport.runOk(List.of(cc(), "-shared", "-fPIC", "-o", other.toString(), source.toString()), 120);
        Path jar = nativesJar(other, NativeLibrary.sha256(other));
        TestSupport.Result r = probe(base(privateTmp()), classes() + java.io.File.pathSeparator + jar);
        assertEquals(3, r.exit(), r.stdout() + r.stderr());
        assertTrue(r.stdout().contains("libtickertape 0.9.0 (ABI 0)") && r.stdout().contains("not compatible"),
                r.stdout());
        assertTrue(!r.stdout().contains(NativeLibrary.EXTRACT_DIR_PROPERTY) && !r.stdout().contains("noexec"),
                "the extract-directory hint is for a file that fails to load, not for a version mismatch: " + r.stdout());
    }

    /**
     * Two first calls of {@code loadLibrary} with different paths, at once: each either loads its own path or is
     * refused, never returns having loaded the other's. The window was between recording the request and loading, so
     * this is a race the test can only provoke (a barrier, several fresh JVMs), not force.
     */
    @Test
    void concurrentFirstLoadLibraryCallsDoNotCrossOver() throws Exception {
        Path copy = Files.createDirectories(tmp.resolve("copy")).resolve(fileName());
        Files.copy(TestSupport.LIBRARY, copy);
        Path race = tmp.resolve("Race.java");
        Files.writeString(race, """
                import io.github.thomas0903.tickertape.Tickertape;
                import java.nio.file.Path;
                import java.util.concurrent.CyclicBarrier;
                public class Race {
                    public static void main(String[] args) throws Exception {
                        CyclicBarrier barrier = new CyclicBarrier(2);
                        String[] outcome = new String[2];
                        Thread[] threads = new Thread[2];
                        for (int i = 0; i < 2; i++) {
                            int k = i;
                            threads[i] = Thread.ofPlatform().start(() -> {
                                try {
                                    barrier.await();
                                    Tickertape.loadLibrary(Path.of(args[k]));
                                    outcome[k] = "loaded";
                                } catch (IllegalStateException e) {
                                    outcome[k] = "refused";
                                } catch (Exception e) {
                                    outcome[k] = "error " + e;
                                }
                            });
                        }
                        for (Thread t : threads) {
                            t.join();
                        }
                        System.out.println("0 " + outcome[0]);
                        System.out.println("1 " + outcome[1]);
                        System.out.println("source " + Tickertape.librarySource());
                    }
                }
                """);
        String[] paths = {TestSupport.LIBRARY.toAbsolutePath().normalize().toString(), copy.toAbsolutePath().toString()};
        for (int run = 0; run < 4; run++) {
            List<String> cmd = new ArrayList<>(List.of(TestSupport.javaLauncher()));
            cmd.addAll(base(privateTmp()));
            cmd.addAll(List.of("--class-path", classes(), race.toString(), paths[0], paths[1]));
            TestSupport.Result r = TestSupport.runOk(cmd, 120);
            List<String> lines = TestSupport.lines(r.stdout());
            String source = lines.get(2);
            int loaded = 0;
            for (int k = 0; k < 2; k++) {
                if (lines.get(k).equals(k + " loaded")) {
                    loaded++;
                    assertTrue(source.contains(paths[k]), "thread " + k + " returned from loadLibrary(" + paths[k]
                            + ") but the library came from elsewhere: " + r.stdout());
                } else {
                    assertEquals(k + " refused", lines.get(k), r.stdout());
                }
            }
            assertEquals(1, loaded, r.stdout());
        }
    }

    @Test
    void aFailedCleanUpNeitherHidesTheErrorNorSkipsTheRest() throws Exception {
        Path full = Files.createDirectories(tmp.resolve("full"));
        Files.writeString(full.resolve("left"), "x"); // deleting a non-empty directory fails
        Path empty = Files.createDirectories(tmp.resolve("empty"));
        UnsatisfiedLinkError primary = new UnsatisfiedLinkError("the real failure");
        NativeLibrary.deleteAll(primary, full, empty);
        assertTrue(Files.notExists(empty), "the paths after a failed one are deleted too");
        assertEquals(1, primary.getSuppressed().length);
        assertTrue(primary.getSuppressed()[0] instanceof DirectoryNotEmptyException, primary.getSuppressed()[0].toString());
        Path file = Files.writeString(tmp.resolve("file"), "x");
        NativeLibrary.deleteAll(null, full, file); // after a successful load: a warning, not an exception
        assertTrue(Files.notExists(file));
    }
}
