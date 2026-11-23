package io.github.thomas0903.tickertape;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.PosixFilePermissions;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;
import java.util.Locale;
import java.util.Optional;

/**
 * Finds, loads and checks {@code libtickertape} (ADR 0037 -> Finding the library). Loading happens once per JVM, on
 * first use, in this order:
 *
 * <ol>
 *   <li>a path given to {@link Tickertape#loadLibrary(Path)} before first use;
 *   <li>the system property {@value #PATH_PROPERTY} (a file path);
 *   <li>a natives JAR on the class path, which carries the library for this platform under
 *       {@code META-INF/tickertape/native/<platform>/} with its SHA-256; it is extracted into a new private
 *       directory, verified, loaded, and deleted again;
 *   <li>each directory of {@code java.library.path};
 *   <li>the operating system's own search ({@code dlopen("libtickertape.so")}: {@code LD_LIBRARY_PATH}, the
 *       loader cache, {@code DYLD_LIBRARY_PATH}).
 * </ol>
 *
 * <p>A library that is not compatible with this binding's version is refused (see {@link #compatible}).
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
final class NativeLibrary {
    private NativeLibrary() {}

    /** The C ABI version of the header this binding is written against ({@code TICKERTAPE_ABI_VERSION}). */
    static final int ABI_VERSION = 1;
    /** {@code TICKERTAPE_VERSION_NUMBER} of that header: {@code MAJOR * 1000000 + MINOR * 1000 + PATCH}. */
    static final int VERSION_NUMBER = 1000000;
    /** The binding's own version: always the library version it is written against. */
    static final String VERSION = "1.0.0";

    static final String PATH_PROPERTY = "tickertape.library.path";
    static final String EXTRACT_DIR_PROPERTY = "tickertape.library.extractDir";
    static final String RESOURCE_ROOT = "META-INF/tickertape/native/";

    private static SymbolLookup lookup;
    /** {@link #lookup} once loading succeeded, read without the lock (calls such as realtimeNs run per record). */
    private static volatile SymbolLookup loaded;
    private static String source;
    private static Path requested;
    /** The file the library was loaded from, when it is still there (not for an extracted copy). */
    private static Path loadedPath;
    private static UnsatisfiedLinkError failure;

    /**
     * Whether a library with C ABI version {@code abi} and version number {@code lib} can be used by this binding:
     * the ABI version must be equal, and the library must be exactly this version before 1.0 (ABI 0 is unstable)
     * and at least this version from 1.0 (ADR 0028 -> Versioning).
     */
    static boolean compatible(int abi, int lib) {
        return abi == ABI_VERSION && (ABI_VERSION == 0 ? lib == VERSION_NUMBER : Integer.compareUnsigned(lib, VERSION_NUMBER) >= 0);
    }

    /** {@code MAJOR.MINOR.PATCH} of a version number, read as the unsigned 32-bit value the C ABI returns. */
    static String versionString(int number) {
        long n = Integer.toUnsignedLong(number);
        return (n / 1_000_000) + "." + (n / 1000 % 1000) + "." + (n % 1000);
    }

    /**
     * {@link Tickertape#loadLibrary(Path)}: records the request and loads, under one lock, so that two first calls
     * with different paths cannot cross over (each either loads its own path or is refused).
     */
    static synchronized void loadFrom(Path path) {
        request(path);
        load0();
    }

    private static void request(Path path) {
        Path p = path.toAbsolutePath().normalize();
        if (lookup != null || failure != null) {
            if (p.equals(requested) || p.equals(loadedPath)) {
                return;
            }
            throw new IllegalStateException("libtickertape is already loaded (" + describe() + "); "
                    + "Tickertape.loadLibrary must be called before any other use of the binding");
        }
        requested = p;
    }

    static synchronized String describe() {
        if (lookup != null) {
            return source;
        }
        return failure != null ? "not loaded: " + failure.getMessage() : "not loaded yet";
    }

    /** Loads the library if needed and returns its symbols; throws the same error on every call if loading failed. */
    static SymbolLookup lookup() {
        SymbolLookup l = loaded;
        return l != null ? l : load0();
    }

    private static synchronized SymbolLookup load0() {
        if (lookup != null) {
            return lookup;
        }
        if (failure != null) {
            UnsatisfiedLinkError again = new UnsatisfiedLinkError(failure.getMessage());
            again.initCause(failure);
            throw again;
        }
        try {
            load();
            loaded = lookup;
            return lookup;
        } catch (UnsatisfiedLinkError e) {
            failure = e;
            throw e;
        } catch (RuntimeException | IOException e) {
            UnsatisfiedLinkError error = new UnsatisfiedLinkError("cannot load libtickertape: " + e);
            error.initCause(e);
            failure = error;
            throw error;
        }
    }

    private static void load() throws IOException {
        MemoryLayout sizeT = Linker.nativeLinker().canonicalLayouts().get("size_t");
        if (ValueLayout.ADDRESS.byteSize() != 8 || sizeT == null || sizeT.byteSize() != 8) {
            throw new UnsatisfiedLinkError("the tickertape binding supports 64-bit platforms only (pointers and size_t"
                    + " of 8 bytes)");
        }
        List<String> tried = new ArrayList<>();
        if (requested != null) {
            open(requested, "Tickertape.loadLibrary(" + requested + ")");
            return;
        }
        String property = System.getProperty(PATH_PROPERTY);
        if (property != null && !property.isEmpty()) {
            Path p = Path.of(property).toAbsolutePath().normalize();
            if (!Files.isRegularFile(p)) {
                throw new UnsatisfiedLinkError("-D" + PATH_PROPERTY + "=" + property + " is not a file");
            }
            open(p, "-D" + PATH_PROPERTY + "=" + p);
            return;
        }
        tried.add("-D" + PATH_PROPERTY + " (not set)");

        String platform = platform();
        String fileName = System.mapLibraryName("tickertape");
        String resource = RESOURCE_ROOT + platform + "/" + fileName;
        ClassLoader loader = NativeLibrary.class.getClassLoader();
        URL url = loader == null ? ClassLoader.getSystemResource(resource) : loader.getResource(resource);
        if (url != null) {
            extractAndOpen(url, resource, fileName);
            return;
        }
        tried.add("a natives JAR with " + resource + " on the class path (none found)");

        String libraryPath = System.getProperty("java.library.path", "");
        for (String dir : libraryPath.split(java.io.File.pathSeparator)) {
            if (dir.isEmpty()) {
                continue;
            }
            Path candidate = Path.of(dir).resolve(fileName);
            if (Files.isRegularFile(candidate)) {
                open(candidate.toAbsolutePath(), "java.library.path: " + candidate.toAbsolutePath());
                return;
            }
        }
        tried.add("java.library.path (" + libraryPath + ")");

        try {
            SymbolLookup system = SymbolLookup.libraryLookup(fileName, Arena.global());
            check(system, "the system library search path (" + fileName + ")");
            return;
        } catch (IllegalArgumentException e) {
            tried.add("the system library search path (" + fileName + ": " + e.getMessage() + ")");
        }
        throw new UnsatisfiedLinkError("cannot find libtickertape for " + platform + "; tried " + String.join(", ", tried)
                + ". Build it with `cargo build --release -p tickertape-ffi` and pass -D" + PATH_PROPERTY
                + "=<path to " + fileName + ">, or add the natives JAR for this platform (see docs/java-api.md)");
    }

    /** {@code linux-x86_64}, {@code linux-aarch64}, {@code macos-aarch64}, {@code macos-x86_64}, or what the JVM says. */
    static String platform() {
        String os = System.getProperty("os.name", "").toLowerCase(Locale.ROOT);
        String arch = System.getProperty("os.arch", "").toLowerCase(Locale.ROOT);
        String o = os.startsWith("linux") ? "linux" : os.startsWith("mac") ? "macos" : os.replace(' ', '_');
        String a = switch (arch) {
            case "amd64", "x86_64" -> "x86_64";
            case "aarch64", "arm64" -> "aarch64";
            default -> arch;
        };
        return o + "-" + a;
    }

    private static void open(Path path, String how) {
        check(map(path, how), how);
        loadedPath = path.toAbsolutePath().normalize();
    }

    /** Loads the file (the dynamic loader maps it); throws if that fails, before any check of what it is. */
    private static SymbolLookup map(Path path, String how) {
        try {
            return SymbolLookup.libraryLookup(path, Arena.global());
        } catch (IllegalArgumentException e) {
            UnsatisfiedLinkError error = new UnsatisfiedLinkError("cannot load libtickertape from " + how + ": " + e.getMessage());
            error.initCause(e);
            throw error;
        }
    }

    /**
     * Extracts the natives JAR's library into a new directory that only this user can read (a random name from
     * {@link Files#createTempDirectory}), checks its size and SHA-256 against the JAR's {@code .sha256} file, loads it
     * and deletes the file and the directory again (the loaded mapping stays valid after the file is unlinked).
     */
    private static void extractAndOpen(URL url, String resource, String fileName) throws IOException {
        URL hashUrl = java.net.URI.create(url + ".sha256").toURL();
        String expected;
        try (InputStream in = hashUrl.openStream()) {
            expected = new String(in.readNBytes(256), StandardCharsets.US_ASCII).trim().split("\\s+")[0].toLowerCase(Locale.ROOT);
        } catch (IOException e) {
            throw new UnsatisfiedLinkError("the natives JAR has " + resource + " but no readable " + fileName
                    + ".sha256 next to it; refusing to load an unverified library");
        }
        if (!expected.matches("[0-9a-f]{64}")) {
            throw new UnsatisfiedLinkError("malformed SHA-256 in " + resource + ".sha256");
        }
        String base = System.getProperty(EXTRACT_DIR_PROPERTY);
        Path parent = base != null && !base.isEmpty() ? Path.of(base) : Path.of(System.getProperty("java.io.tmpdir"));
        Path dir;
        if (FileSystems.getDefault().supportedFileAttributeViews().contains("posix")) {
            dir = Files.createTempDirectory(parent, "tickertape-",
                    PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rwx------")));
        } else {
            dir = Files.createTempDirectory(parent, "tickertape-");
        }
        Path file = dir.resolve(fileName);
        Throwable failure = null;
        try {
            try (InputStream in = url.openStream();
                    OutputStream out = Files.newOutputStream(file, StandardOpenOption.CREATE_NEW, StandardOpenOption.WRITE)) {
                in.transferTo(out);
            }
            String actual = sha256(file);
            if (!actual.equals(expected)) {
                throw new UnsatisfiedLinkError("the library extracted from " + url + " has SHA-256 " + actual
                        + " but the natives JAR says " + expected + "; refusing to load it");
            }
            String how = "the natives JAR (" + url + ", SHA-256 " + actual + ")";
            SymbolLookup symbols;
            try {
                symbols = map(file, how);
            } catch (UnsatisfiedLinkError e) {
                // The hash matched, so the file is the right library: the usual cause is a directory that does not
                // allow executable mappings (a noexec temporary directory). Only for a failure to load the file: a
                // library that loads but fails the checks below gets no such hint.
                UnsatisfiedLinkError error = new UnsatisfiedLinkError(e.getMessage() + "; it was extracted into "
                        + parent + ", which may not allow loading libraries from it (for example a noexec file"
                        + " system): set -D" + EXTRACT_DIR_PROPERTY + "=<a directory that does>, or -D"
                        + PATH_PROPERTY + "=<path to the library>");
                error.initCause(e);
                throw error;
            }
            check(symbols, how);
            loadedPath = null; // the extracted copy is deleted below
        } catch (Throwable t) {
            failure = t;
            throw t;
        } finally {
            deleteAll(failure, file, dir);
        }
    }

    /**
     * Deletes {@code paths} in order, trying every one even if an earlier one fails. A failure is added to
     * {@code primary}, the exception already on its way out, as a suppressed exception, so it never replaces it; with
     * no such exception (the library is loaded and usable), it is reported as a warning through
     * {@link System.Logger} and loading succeeds.
     */
    static void deleteAll(Throwable primary, Path... paths) {
        IOException failed = null;
        for (Path p : paths) {
            try {
                Files.deleteIfExists(p);
            } catch (IOException e) {
                if (failed == null) {
                    failed = e;
                } else {
                    failed.addSuppressed(e);
                }
            }
        }
        if (failed == null) {
            return;
        }
        if (primary != null) {
            primary.addSuppressed(failed);
        } else {
            System.getLogger(NativeLibrary.class.getName()).log(System.Logger.Level.WARNING,
                    "libtickertape was loaded, but its extracted copy could not be deleted", failed);
        }
    }

    static String sha256(Path file) throws IOException {
        MessageDigest digest;
        try {
            digest = MessageDigest.getInstance("SHA-256");
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException(e);
        }
        try (InputStream in = Files.newInputStream(file)) {
            byte[] buf = new byte[1 << 16];
            for (int n; (n = in.read(buf)) > 0; ) {
                digest.update(buf, 0, n);
            }
        }
        return HexFormat.of().formatHex(digest.digest());
    }

    /** Checks the version and that every function the binding uses is present, then accepts the library. */
    private static void check(SymbolLookup symbols, String how) {
        Linker linker = Linker.nativeLinker();
        int abi;
        int number;
        String version;
        try {
            abi = (int) handle(linker, symbols, "tickertape_abi_version").invokeExact();
            number = (int) handle(linker, symbols, "tickertape_version_number").invokeExact();
            MemorySegment v = (MemorySegment) handle(linker, symbols, "tickertape_version").invokeExact();
            version = v.reinterpret(256).getString(0);
        } catch (UnsatisfiedLinkError e) {
            throw new UnsatisfiedLinkError("the library loaded from " + how + " is not libtickertape: " + e.getMessage());
        } catch (Throwable t) {
            throw new IllegalStateException(t);
        }
        if (!compatible(abi, number)) {
            String rule = ABI_VERSION == 0
                    ? "before 1.0 the ABI is unstable, so the library version must equal the binding's"
                    : "the ABI version must be equal and the library at least as new as the binding";
            throw new UnsatisfiedLinkError("libtickertape " + version + " (ABI " + abi + ") from " + how
                    + " is not compatible with this binding (" + VERSION + ", ABI " + ABI_VERSION + "): " + rule);
        }
        List<String> missing = new ArrayList<>();
        for (String name : Signatures.FUNCTIONS.keySet()) {
            if (symbols.find(name).isEmpty()) {
                missing.add(name);
            }
        }
        if (!missing.isEmpty()) {
            throw new UnsatisfiedLinkError("libtickertape " + version + " from " + how + " lacks " + missing);
        }
        lookup = symbols;
        source = "libtickertape " + version + " from " + how;
    }

    private static MethodHandle handle(Linker linker, SymbolLookup symbols, String name) {
        Optional<MemorySegment> symbol = symbols.find(name);
        if (symbol.isEmpty()) {
            throw new UnsatisfiedLinkError("no symbol " + name);
        }
        FunctionDescriptor descriptor = Signatures.FUNCTIONS.get(name);
        return linker.downcallHandle(symbol.get(), descriptor);
    }
}
