package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.fail;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;
import java.util.concurrent.TimeUnit;

/** Shared test helpers: locations of external tools, the cross-language fixture pattern, and process running. */
final class TestSupport {
    private TestSupport() {}

    static final Path REPO = Path.of(System.getProperty("tickertape.test.repoRoot", "../..")).toAbsolutePath().normalize();
    static final Path NATIVE_DIR = Path.of(System.getProperty("tickertape.test.nativeDir", REPO + "/target/release"));
    static final Path TOOLS_DIR = Path.of(System.getProperty("tickertape.test.toolsDir", NATIVE_DIR + "/java-tools"));
    static final boolean REQUIRE_TOOLS = Boolean.getBoolean("tickertape.test.requireTools");
    static final Path CLASSES = Path.of(System.getProperty("tickertape.test.classes", REPO + "/bindings/java/target/classes"));
    static final Path HEADER = REPO.resolve("crates/tickertape-ffi/include/tickertape.h");
    static final Path LIBRARY = Path.of(System.getProperty("tickertape.library.path"));

    /** 64 KiB segments (the smallest) and no background thread, as the C ABI's fixture uses. */
    static WriterOptions small() {
        return WriterOptions.defaults().segmentSize(65_536).prefault(false);
    }

    /** An external program, or a skipped test (a failure when {@code tickertape.test.requireTools} is true). */
    static Path tool(Path path) {
        boolean ok = Files.isExecutable(path);
        if (!ok && REQUIRE_TOOLS) {
            fail("required tool missing: " + path + " (run bindings/java/scripts/build-native.sh)");
        }
        assumeTrue(ok, "tool not built: " + path + " (run bindings/java/scripts/build-native.sh)");
        return path;
    }

    static Path cli() {
        return tool(NATIVE_DIR.resolve("tickertape"));
    }

    static Path cTest() {
        return tool(TOOLS_DIR.resolve("test_api"));
    }

    static Path cReader() {
        return tool(TOOLS_DIR.resolve("reader"));
    }

    static String python() {
        String python = System.getProperty("tickertape.test.python", "python3");
        try {
            Process p = new ProcessBuilder(python, "--version").redirectErrorStream(true).start();
            p.getInputStream().readAllBytes();
            if (p.waitFor() == 0) {
                return python;
            }
        } catch (IOException | InterruptedException e) {
            // fall through
        }
        if (REQUIRE_TOOLS) {
            fail(python + " not found");
        }
        assumeTrue(false, python + " not found");
        return null;
    }

    // ---- The cross-language fixture pattern, shared with crates/tickertape-ffi/tests (c_api.rs, test_api.c) ----

    static final long FIXTURE_RECORDS = 1500;
    static final long FIXTURE_CHECKPOINT_AT = 1000;

    static byte[] patternPayload(long seq) {
        int len = (int) (seq * 37 % 300);
        byte[] out = new byte[len];
        for (int i = 0; i < len; i++) {
            out[i] = (byte) ((seq + 7L * i) & 0xFF);
        }
        return out;
    }

    static int patternSchema(long seq) {
        return (int) (seq % 5 + 1);
    }

    static long patternVenue(long seq) {
        return 1_000_000 + seq;
    }

    static long patternLocal(long seq) {
        return 2_000_000 + 3 * seq;
    }

    static void checkPattern(RecordView r, long seq) {
        assertEquals(seq, r.seq());
        assertEquals(patternSchema(seq), r.schemaId(), "schema of " + seq);
        assertEquals(patternVenue(seq), r.venueTsNs(), "venue ts of " + seq);
        assertEquals(patternLocal(seq), r.localTsNs(), "local ts of " + seq);
        org.junit.jupiter.api.Assertions.assertArrayEquals(patternPayload(seq), r.payloadToArray(), "payload of " + seq);
    }

    /** The record as a JSONL line, in the format of the CLI's export and the Python decoder. */
    static String jsonl(RecordView r) {
        return "{\"seq\":" + Long.toUnsignedString(r.seq()) + ",\"schema_id\":" + Integer.toUnsignedString(r.schemaId())
                + ",\"venue_ts_ns\":" + Long.toUnsignedString(r.venueTsNs()) + ",\"local_ts_ns\":"
                + Long.toUnsignedString(r.localTsNs()) + ",\"payload_b64\":\""
                + Base64.getEncoder().encodeToString(r.payloadToArray()) + "\"}";
    }

    record Result(int exit, String stdout, String stderr) {}

    static Result run(List<String> command, long timeoutSeconds) throws IOException, InterruptedException {
        return run(command, null, timeoutSeconds);
    }

    /** As {@link #run(List, long)}, in working directory {@code directory} (null: this process's). */
    static Result run(List<String> command, Path directory, long timeoutSeconds) throws IOException, InterruptedException {
        Process p = new ProcessBuilder(command).directory(directory == null ? null : directory.toFile()).start();
        var out = new java.io.ByteArrayOutputStream();
        var err = new java.io.ByteArrayOutputStream();
        Thread t1 = Thread.ofPlatform().start(() -> copy(p.getInputStream(), out));
        Thread t2 = Thread.ofPlatform().start(() -> copy(p.getErrorStream(), err));
        if (!p.waitFor(timeoutSeconds, TimeUnit.SECONDS)) {
            p.destroyForcibly();
            fail(command + " did not finish within " + timeoutSeconds + " s");
        }
        t1.join();
        t2.join();
        return new Result(p.exitValue(), out.toString(StandardCharsets.UTF_8), err.toString(StandardCharsets.UTF_8));
    }

    static Result runOk(List<String> command, long timeoutSeconds) throws IOException, InterruptedException {
        Result r = run(command, timeoutSeconds);
        if (r.exit() != 0) {
            fail(command + " failed with " + r.exit() + "\nstdout:\n" + r.stdout() + "\nstderr:\n" + r.stderr());
        }
        return r;
    }

    private static void copy(java.io.InputStream in, java.io.OutputStream out) {
        try (in) {
            in.transferTo(out);
        } catch (IOException e) {
            throw new java.io.UncheckedIOException(e);
        }
    }

    /** The java launcher of the running JVM. */
    static String javaLauncher() {
        return Path.of(System.getProperty("java.home"), "bin", "java").toString();
    }

    static List<String> lines(String s) {
        List<String> out = new ArrayList<>();
        for (String line : s.split("\n")) {
            if (!line.isEmpty()) {
                out.add(line);
            }
        }
        return out;
    }
}
