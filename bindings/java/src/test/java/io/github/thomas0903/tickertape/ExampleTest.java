package io.github.thomas0903.tickertape;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.io.TempDir;

/** Runs {@code examples/Example.java} with the source launcher: a follower process, a recorder, then replays. */
class ExampleTest {
    @TempDir
    Path tmp;

    static List<String> launcher(String... args) {
        List<String> cmd = new ArrayList<>(List.of(TestSupport.javaLauncher(),
                "--enable-native-access=ALL-UNNAMED", "--illegal-native-access=deny",
                "-Dtickertape.library.path=" + TestSupport.LIBRARY,
                "--class-path", TestSupport.CLASSES.toString(),
                TestSupport.REPO.resolve("bindings/java/examples/Example.java").toString()));
        cmd.addAll(List.of(args));
        return cmd;
    }

    @Test
    void recordFollowLiveAndReplay() throws Exception {
        Path dir = tmp.resolve("example");
        Process follower = new ProcessBuilder(launcher("follow", dir.toString(), "30"))
                .redirectError(ProcessBuilder.Redirect.INHERIT).start();
        Thread.sleep(300); // the follower starts before the journal exists
        TestSupport.Result recorded = TestSupport.runOk(launcher("record", dir.toString(), "30", "5"), 120);
        assertEquals(30, TestSupport.lines(recorded.stdout()).size());
        assertTrue(follower.waitFor(120, TimeUnit.SECONDS), "the follower did not finish");
        assertEquals(0, follower.exitValue());
        List<String> followed = TestSupport.lines(new String(follower.getInputStream().readAllBytes(), StandardCharsets.UTF_8));
        assertEquals(30, followed.size(), followed.toString());
        assertTrue(followed.get(0).startsWith("0 schema=1 local_ts_ns="), followed.get(0));
        assertTrue(followed.get(29).endsWith("payload=tick 29 price 100.29"), followed.get(29));
        assertFalse(recorded.stderr().contains("WARNING"), recorded.stderr());

        List<String> replay = TestSupport.lines(TestSupport.runOk(launcher("replay", dir.toString(), "10"), 120).stdout());
        assertEquals(followed.subList(10, 30), replay);

        // The C reader sees the same journal (when it is built).
        if (java.nio.file.Files.isExecutable(TestSupport.TOOLS_DIR.resolve("reader"))) {
            List<String> c = TestSupport.lines(TestSupport.runOk(
                    List.of(TestSupport.TOOLS_DIR.resolve("reader").toString(), dir.toString(), "replay"), 60).stdout());
            assertEquals(followed, c);
        }
    }
}
