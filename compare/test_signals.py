"""The runner leaves no child behind, however a run ends: normally, a tool's failure, its own timeout, SIGTERM, SIGINT,
SIGHUP, and SIGKILL (the guard). The children are stand-ins (standin.py): no build, no JDK, nothing measured.

The only time bound is BOUND_S, far above the runner's grace period (reaper.GRACE_S).
"""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

import reaper

HERE = Path(__file__).resolve().parent
BOUND_S = 30


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return False  # Another user's process now: ours is gone.
    return True


class Harness:
    """compare/run.py's short mode with stand-in children, in a process group of its own."""

    def __init__(self, case, tools, **env):
        self.tmp = Path(tempfile.mkdtemp(prefix="tt-signals-"))
        self.pids, self.out = self.tmp / "pids", self.tmp / "out"
        self.pids.mkdir()
        self.log = (self.tmp / "harness.log").open("w+")
        env = dict(os.environ, STANDIN_PIDS=str(self.pids), TMPDIR=str(self.tmp), **env)
        self.proc = subprocess.Popen([sys.executable, str(HERE / "standin.py"), "harness", str(self.out), "--quick",
                                      "--no-pin", "--tools", tools], env=env, stdout=self.log,
                                     stderr=subprocess.STDOUT, start_new_session=True)
        case.addCleanup(self.cleanup)

    def output(self):
        self.log.flush()
        return (self.tmp / "harness.log").read_text()

    def recorded(self):
        return {int(p.name): p.read_text() for p in self.pids.iterdir() if p.name.isdigit()}

    def wait_alive(self, count):
        """Waits until `count` stand-ins are hanging, all set up."""
        end = time.monotonic() + BOUND_S
        while len(list(self.pids.glob("*.alive"))) < count:
            if self.proc.poll() is not None:
                raise AssertionError(f"the runner ended early ({self.proc.returncode}):\n{self.output()}")
            if time.monotonic() > end:
                raise AssertionError(f"no {count} stand-ins hanging:\n{self.output()}")
            time.sleep(0.02)

    def running(self, count):
        """Waits until `count` recorded processes run (the guard included) and returns their kinds."""
        end = time.monotonic() + BOUND_S
        while True:
            live = sorted(kind for pid, kind in self.recorded().items() if alive(pid))
            if len(live) >= count or time.monotonic() > end:
                return live
            time.sleep(0.02)

    def wait(self):
        return self.proc.wait(timeout=BOUND_S)

    def survivors(self):
        """The stand-ins (and the guard) still running BOUND_S after the runner ended."""
        end = time.monotonic() + BOUND_S
        while True:
            left = {pid: kind for pid, kind in self.recorded().items() if alive(pid)}
            if not left or time.monotonic() > end:
                return left
            time.sleep(0.05)

    def cleanup(self):
        # Only the processes this test started and recorded, never by name.
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
        for pid in self.recorded():
            if alive(pid):
                os.kill(pid, signal.SIGKILL)
        self.log.close()
        subprocess.run(["rm", "-rf", str(self.tmp)], check=False)


class EndingTests(unittest.TestCase):
    def assert_marked_incomplete(self, harness):
        self.assertFalse(harness.out.exists(), "an unfinished run kept the name of a finished one")
        marked = harness.out.with_name("out.incomplete-1")
        self.assertTrue((marked / "children.log").exists(), harness.output())
        self.assertFalse((marked / "skipped.json").exists())  # Written last, by a finished run only.
        return marked

    def test_each_signal_ends_every_child_and_marks_the_run_incomplete(self):
        for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            with self.subTest(signal=sig.name):
                harness = Harness(self, "yaque", STANDIN_HANG="visible_latency")
                harness.wait_alive(2)
                self.assertEqual(harness.running(3), ["guard", "yaque reader", "yaque writer"])
                os.kill(harness.proc.pid, sig)
                self.assertEqual(harness.wait(), 128 + sig, harness.output())
                self.assertEqual(harness.survivors(), {})
                marked = self.assert_marked_incomplete(harness)
                self.assertTrue((marked / "yaque-W1-cold.jsonl").exists())  # Results so far stay.
                self.assertIn(f"terminated by {sig.name}", harness.output())

    def test_children_that_ignore_sigterm_and_what_they_started_are_killed(self):
        harness = Harness(self, "plain-file", STANDIN_HANG="visible_latency", STANDIN_IGNORE_TERM="1",
                          STANDIN_GRANDCHILD="1")
        harness.wait_alive(2)
        self.assertEqual(harness.running(5), ["grandchild of plain-file reader visible_latency",
                                              "grandchild of plain-file writer visible_latency", "guard",
                                              "plain-file reader", "plain-file writer"])
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assert_marked_incomplete(harness)

    def test_tt_latency_and_the_processes_it_started_end_with_the_run(self):
        harness = Harness(self, "tickertape", STANDIN_HANG="latency")
        harness.wait_alive(1)
        self.assertEqual(harness.running(4), ["grandchild of tt-latency reader", "grandchild of tt-latency writer",
                                              "guard", "tt-latency"])
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assert_marked_incomplete(harness)

    def test_a_failed_tool_ends_the_other_child(self):
        harness = Harness(self, "yaque", STANDIN_HANG="visible_latency", STANDIN_FAIL="visible_latency:reader")
        self.assertEqual(harness.wait(), 1, harness.output())
        self.assertIn("compare: yaque-W2-64", harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assert_marked_incomplete(harness)

    def test_the_runner_s_own_timeout_ends_the_children(self):
        harness = Harness(self, "yaque", STANDIN_HANG="visible_latency", STANDIN_SLACK_S="1")
        self.assertEqual(harness.wait(), 1, harness.output())
        self.assertIn("timed out", harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assert_marked_incomplete(harness)

    def test_after_sigkill_of_the_runner_the_guard_ends_the_children_and_marks_the_run(self):
        harness = Harness(self, "yaque", STANDIN_HANG="visible_latency", STANDIN_GRANDCHILD="1")
        harness.wait_alive(2)
        self.assertEqual(len(harness.running(5)), 5)
        os.kill(harness.proc.pid, signal.SIGKILL)
        self.assertEqual(harness.wait(), -signal.SIGKILL)
        self.assertEqual(harness.survivors(), {})  # The guard has ended too.
        self.assert_marked_incomplete(harness)

    def test_a_finished_run_leaves_nothing_running_and_is_not_marked(self):
        harness = Harness(self, "yaque,plain-file")
        self.assertEqual(harness.wait(), 0, harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assertTrue((harness.out / "skipped.json").exists())
        self.assertEqual([p.name for p in harness.tmp.iterdir() if "incomplete" in p.name], [])


def jvm_adapters_built():
    java = Path(os.environ.get("JAVA_HOME", "/opt/homebrew/opt/openjdk@21")) / "bin/java"
    return java.exists() and all((HERE / t / "target/classpath.txt").exists() for t in ("chronicle", "aeron"))


@unittest.skipUnless(jvm_adapters_built() or os.environ.get("TT_COMPARE_REQUIRE_JVM") == "1",
                     "needs a JDK 21 and the built Java adapters (see compare/README.md, Run); CI has neither")
class JvmTests(unittest.TestCase):
    """The real Chronicle Queue and Aeron adapters: a writer and a reader JVM (Aeron's writer embeds its media driver),
    terminated while W2 runs."""

    def children(self, pid):
        ps = subprocess.run(["ps", "-A", "-o", "pid=,ppid="], text=True, stdout=subprocess.PIPE, check=True).stdout
        return [int(p) for p, parent in (line.split() for line in ps.splitlines()) if int(parent) == pid]

    def test_sigterm_during_w2_ends_both_jvms(self):
        for tool in ("chronicle", "aeron"):
            with self.subTest(tool=tool):
                env = {"STANDIN_REAL": tool}
                if "JAVA_HOME" not in os.environ:
                    env["JAVA_HOME"] = "/opt/homebrew/opt/openjdk@21"
                harness = Harness(self, tool, **env)
                log = harness.out / "children.log"
                end = time.monotonic() + 300
                # W2 has started, and the runner has its guard and both JVMs.
                while not (log.exists() and f"{tool}-W2-64" in log.read_text()) or \
                        len(self.children(harness.proc.pid)) < 3:
                    self.assertIsNone(harness.proc.poll(), harness.output())
                    self.assertLess(time.monotonic(), end, harness.output())
                    time.sleep(0.05)
                jvms = [p for p in self.children(harness.proc.pid) if p not in harness.recorded()]  # Not the guard.
                self.assertEqual(len(jvms), 2)
                os.kill(harness.proc.pid, signal.SIGTERM)
                self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
                end = time.monotonic() + BOUND_S
                while any(alive(p) for p in jvms):
                    self.assertLess(time.monotonic(), end, f"JVMs left: {[p for p in jvms if alive(p)]}")
                    time.sleep(0.05)
                self.assertFalse(harness.out.exists())
                self.assertTrue(harness.out.with_name("out.incomplete-1").exists())


class ReaperTests(unittest.TestCase):
    def setUp(self):
        saved = {sig: signal.getsignal(sig) for sig in reaper.SIGNALS}
        self.addCleanup(lambda: [signal.signal(sig, handler) for sig, handler in saved.items()])

    def test_a_signal_waits_until_a_child_is_started_and_tracked(self):
        reaper.install()
        with self.assertRaises(reaper.Terminated) as raised:
            with reaper.deferred():
                os.kill(os.getpid(), signal.SIGTERM)
                for _ in range(100):
                    time.sleep(0.001)  # The handler runs here, but holds the signal back.
                finished = True
        self.assertTrue(finished)
        self.assertEqual(raised.exception.code, 128 + signal.SIGTERM)

    def test_a_second_signal_does_not_cut_the_cleanup_short(self):
        reaper.install()
        cleaned = False
        with self.assertRaises(reaper.Terminated):
            try:
                os.kill(os.getpid(), signal.SIGTERM)
                time.sleep(1)
            finally:
                os.kill(os.getpid(), signal.SIGTERM)
                time.sleep(0.05)
                cleaned = True
        self.assertTrue(cleaned)

    def test_signals_ignored_at_start_stay_ignored(self):
        signal.signal(signal.SIGHUP, signal.SIG_IGN)  # nohup
        reaper.install()
        self.assertEqual(signal.getsignal(signal.SIGHUP), signal.SIG_IGN)
        self.assertIsNot(signal.getsignal(signal.SIGTERM), signal.SIG_DFL)

    def test_incomplete_directories_are_numbered_like_the_rerun_driver_s(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "unit"
            self.assertIsNone(reaper.mark_incomplete(out))
            out.mkdir()
            self.assertIsNone(reaper.mark_incomplete(out))  # Empty: nothing to mistake for a result.
            for n in (1, 2):
                out.mkdir(exist_ok=True)
                (out / "environment.txt").write_text("")
                self.assertEqual(reaper.mark_incomplete(out), Path(tmp) / f"unit.incomplete-{n}")


if __name__ == "__main__":
    unittest.main()
