package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.io.IOException;
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
}
