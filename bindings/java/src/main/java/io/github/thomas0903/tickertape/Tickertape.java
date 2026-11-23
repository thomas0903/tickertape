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
 * <p><b>Versions.</b> The binding's version is the version of the C library it is written against. From 1.0 (ABI version
 * 1) it requires the same ABI version and a library at least as new as itself. Releases before 1.0 had the unstable
 * ABI version 0 and required exactly their own library version. See {@code docs/java-api.md} &rarr; Versioning.
 *
 * <p id="paths"><b>Paths.</b> Every method that takes the {@link Path} of a journal, or of a retention destination,
 * passes the C library the bytes the JDK itself uses for that path, so a name in any encoding or Unicode normalization
 * form reaches the operating system as the JDK holds it. The binding reads those bytes from {@link Path#toUri()}, whose
 * {@code file:} URI carries them percent-encoded. Three consequences:
 *
 * <ul>
 *   <li>An absolute path is passed as its bytes. A relative path is passed the way the JDK passes it to the
 *       operating system for its own file operations: relative, with its own bytes (the empty path as {@code .}), when
 *       the JDK does so, which is when it started with its default directory ({@code user.dir}) equal to the process's
 *       working directory, and as its absolute bytes under the default directory otherwise ({@code -Duser.dir=}
 *       naming another directory), to which the platform's path-length limit then applies. The JDK does not expose
 *       that decision, so the binding infers it the first time it converts a relative path: relative if the working
 *       directory ({@code getcwd}) has the default directory's bytes, or if the JDK's own {@code stat} of
 *       {@code Path.of("")} reaches the working directory while the default directory's path does not (the working
 *       directory was renamed after start-up); absolute otherwise, or if the inference fails, in which case the next
 *       conversion tries again. A process that changes its working directory through native code ({@code chdir}) is
 *       outside what the binding follows, as it is for the JDK. Trailing slashes are removed (except from the root).
 *   <li>{@code toUri()} may {@code stat} the path once (to tell whether it is a directory), and for a relative path
 *       the working directory once more; until the inference above has succeeded, a relative path also costs a
 *       {@code getcwd} and up to three more {@code stat}s. This happens when a journal is opened or inspected, or a
 *       retention destination is set, never per record.
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

    /** The loaded library's C ABI version (1 from 1.0; 0 before: unstable). */
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
