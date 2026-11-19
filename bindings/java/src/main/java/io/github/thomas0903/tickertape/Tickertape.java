package io.github.thomas0903.tickertape;

import java.nio.file.Path;
import java.util.Objects;

/**
 * The library itself: its version, where it was loaded from, and a clock.
 *
 * <p><b>Finding libtickertape.</b> The binding loads the native library on first use, in this order: a path given to
 * {@link #loadLibrary(Path)}; the system property {@code tickertape.library.path}; a natives JAR for this platform on
 * the class path (extracted into a new private temporary directory, checked against its SHA-256, and deleted again
 * once loaded; {@code tickertape.library.extractDir} chooses the parent directory); the directories of
 * {@code java.library.path}; and finally the operating system's own search path. The first library found must be
 * compatible (see below), or loading fails with an {@link UnsatisfiedLinkError} that says why; the binding never falls
 * through to a later candidate after a failure, so the library you configured is the one you get.
 *
 * <p><b>Versions.</b> The binding's version is the version of the C library it is written against. Before 1.0 the C
 * ABI is unstable (ABI version 0), so the binding requires exactly that library version. From 1.0 it requires the same
 * ABI version and a library at least as new as itself. See {@code docs/java-api.md} &rarr; Versioning.
 *
 * <p id="paths"><b>Paths.</b> Every method that takes the {@link Path} of a journal, or of a retention destination,
 * passes the C library the bytes the JDK itself uses for that path, so a name in any encoding or Unicode normalization
 * form reaches the operating system as the JDK holds it. The binding reads those bytes from {@link Path#toUri()}, whose
 * {@code file:} URI carries them percent-encoded. Three consequences:
 *
 * <ul>
 *   <li>An absolute path is passed as its bytes. A relative path is passed relative, with its own bytes (the empty
 *       path as {@code .}), and the library resolves it against the process's working directory. That is the
 *       directory the JVM started in, against which the JDK resolves relative paths too: Java code cannot change it,
 *       and setting the {@code user.dir} property, which the JDK does not support, moves neither (only native code
 *       that calls {@code chdir} does). Trailing slashes are removed (except from the root).
 *   <li>{@code toUri()} may {@code stat} the path once (to tell whether it is a directory), and for a relative path
 *       the working directory once more. This happens when a journal is opened or inspected, or a retention
 *       destination is set, never per record.
 *   <li>Only the default file system is supported; a path of any other ({@code FileSystems.newFileSystem}, a zip file
 *       system) throws {@link IllegalArgumentException}.
 * </ul>
 *
 * <p>See {@code docs/java-api.md} &rarr; Conventions &rarr; Paths.
 *
 * <p>The JVM must allow this module native access: {@code --enable-native-access=io.github.thomas0903.tickertape}
 * on the module path, {@code --enable-native-access=ALL-UNNAMED} on the class path.
 */
@SuppressWarnings("restricted") // calls restricted java.lang.foreign methods; needs native access
public final class Tickertape {
    private Tickertape() {}

    /** The binding's version, which is also the C library version it requires (before 1.0, exactly). */
    public static String bindingVersion() {
        return NativeLibrary.VERSION;
    }

    /**
     * Loads libtickertape from {@code path} instead of searching for it. Must be called before any other use of the
     * binding; calling it again with the same path does nothing.
     *
     * @throws IllegalStateException if a library is already loaded (or failed to load) from elsewhere
     * @throws UnsatisfiedLinkError if the library cannot be loaded or is not compatible
     */
    public static void loadLibrary(Path path) {
        Objects.requireNonNull(path, "path");
        NativeLibrary.loadFrom(path);
    }

    /** Where the library was loaded from, its version, or why loading failed; for diagnostics. Does not load it. */
    public static String librarySource() {
        return NativeLibrary.describe();
    }

    /** The loaded library's version, such as {@code "0.4.0"}. */
    public static String libraryVersion() {
        NativeLibrary.lookup();
        return Native.version().reinterpret(Long.MAX_VALUE).getString(0);
    }

    /** The loaded library's version number ({@code MAJOR * 1000000 + MINOR * 1000 + PATCH}). */
    public static int libraryVersionNumber() {
        NativeLibrary.lookup();
        return Native.versionNumber();
    }

    /** The loaded library's C ABI version (0 before 1.0: unstable). */
    public static int abiVersion() {
        NativeLibrary.lookup();
        return Native.abiVersion();
    }

    /**
     * The system wall-clock time in nanoseconds since the Unix epoch ({@code CLOCK_REALTIME}: it can jump), or 0 if
     * the clock is before the epoch. A convenience for record timestamps, which the library never interprets.
     */
    public static long realtimeNs() {
        NativeLibrary.lookup();
        return Native.realtimeNs();
    }
}
