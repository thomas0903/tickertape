package io.github.thomas0903.tickertape;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.charset.StandardCharsets;
import java.nio.file.FileSystems;
import java.nio.file.Path;

/** Path conversion for the C ABI, which takes NUL-terminated byte strings and passes them to the OS unchanged. */
final class Paths {
    private Paths() {}

    static void requireDefaultFileSystem(Path p) {
        if (p.getFileSystem() != FileSystems.getDefault()) {
            throw new IllegalArgumentException("not a path on the default file system: " + p);
        }
    }

    /** The path as a UTF-8, NUL-terminated string in {@code arena}. */
    static MemorySegment toC(Path p, Arena arena) {
        requireDefaultFileSystem(p);
        String s = p.toString();
        if (s.indexOf('\0') >= 0) {
            throw new IllegalArgumentException("path contains a NUL character: " + p);
        }
        return arena.allocateFrom(s, StandardCharsets.UTF_8);
    }
}
