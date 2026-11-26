"""The runner leaves no child behind, however a run ends: normally, a tool's failure, its own timeout, SIGTERM, SIGINT,
SIGHUP, and SIGKILL (the guard). The children are stand-ins (standin.py): no build, no JDK, nothing measured.

The only time bound is BOUND_S, far above the runner's grace period (reaper.GRACE_S).
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

import reaper
import run

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

    def __init__(self, case, tools, pinning=("--no-pin",), pipe=False, **env):
        self.tmp = Path(tempfile.mkdtemp(prefix="tt-signals-"))
        self.pids, self.out = self.tmp / "pids", self.tmp / "out"
        self.pids.mkdir()
        self.log = (self.tmp / "harness.log").open("w+")
        env = dict(os.environ, STANDIN_PIDS=str(self.pids), TMPDIR=str(self.tmp), **env)
        # pipe: stdout and stderr go to a pipe the test can close (a terminal that went away).
        self.proc = subprocess.Popen([sys.executable, str(HERE / "standin.py"), "harness", str(self.out), "--quick",
                                      *pinning, "--tools", tools], env=env,
                                     stdout=subprocess.PIPE if pipe else self.log, stderr=subprocess.STDOUT,
                                     start_new_session=True)
        case.addCleanup(self.cleanup)

    def ids(self, kind):
        """Session, process group and stdout of each stand-in of this kind."""
        return [json.loads((self.pids / f"{pid}.ids").read_text())
                for pid, k in self.recorded().items() if k == kind]

    def pid_of(self, kind):
        return next(pid for pid, k in self.recorded().items() if k == kind)

    def got_sigterm(self):
        """The group members (grandchildren) that noted a SIGTERM before they ended."""
        return sorted(k for pid, k in self.recorded().items() if (self.pids / f"{pid}.term").exists())

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
        # Only the processes this test started and recorded, never by name, and only while the PID still runs one of
        # them: a stand-in that ended long ago may have left its number to an unrelated process.
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait()
        if self.proc.stdout:
            self.proc.stdout.close()
        for pid in self.recorded():
            command = subprocess.run(["ps", "-o", "command=", "-p", str(pid)], text=True,
                                     stdout=subprocess.PIPE).stdout
            if str(HERE / "standin.py") in command or str(HERE / "reaper.py") in command:
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
        # As at v1.0.0, tt-latency and its processes are in the runner's session; they are a group of their own.
        latency = harness.pid_of("tt-latency")
        for kind in ("tt-latency", "grandchild of tt-latency writer", "grandchild of tt-latency reader"):
            self.assertEqual([(i["sid"], i["pgid"]) for i in harness.ids(kind)], [(harness.proc.pid, latency)], kind)
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assertEqual(harness.got_sigterm(), ["grandchild of tt-latency reader", "grandchild of tt-latency writer"])
        self.assert_marked_incomplete(harness)

    def test_sigterm_reaches_every_process_of_a_child_s_group(self):
        harness = Harness(self, "yaque", STANDIN_HANG="visible_latency", STANDIN_GRANDCHILD="1")
        harness.wait_alive(2)
        self.assertEqual(len(harness.running(5)), 5)
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
        self.assertEqual(harness.survivors(), {})
        self.assertEqual(harness.got_sigterm(), ["grandchild of yaque reader visible_latency",
                                                 "grandchild of yaque writer visible_latency"])

    def test_with_its_output_gone_the_runner_still_cleans_up_and_exits_with_the_signal(self):
        harness = Harness(self, "yaque", pipe=True, STANDIN_HANG="visible_latency")
        harness.wait_alive(2)
        harness.proc.stdout.close()  # The terminal went away; every write fails from now on.
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM)
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
        self.assertEqual(harness.got_sigterm(), ["grandchild of yaque reader visible_latency",
                                                 "grandchild of yaque writer visible_latency"])
        self.assert_marked_incomplete(harness)

    def test_the_guard_leaves_alone_a_directory_the_driver_started_again(self):
        # The driver's retry: run.py is killed, the driver sets the unit's directory aside and makes a new one at
        # once, while the old guard still waits for children that ignore SIGTERM.
        harness = Harness(self, "yaque", STANDIN_HANG="visible_latency", STANDIN_IGNORE_TERM="1")
        harness.wait_alive(2)
        guard = harness.pid_of("guard")
        os.kill(harness.proc.pid, signal.SIGKILL)
        harness.wait()
        harness.out.rename(harness.out.with_name("out.incomplete-1"))
        harness.out.mkdir()
        (harness.out / "environment.txt").write_text("the new run\n")
        end = time.monotonic() + BOUND_S
        while alive(guard):
            self.assertLess(time.monotonic(), end)
            time.sleep(0.05)
        self.assertEqual((harness.out / "environment.txt").read_text(), "the new run\n")
        self.assertFalse(harness.out.with_name("out.incomplete-2").exists())
        self.assertEqual(harness.survivors(), {})

    @unittest.skipUnless(hasattr(os, "sched_getaffinity") and len(os.sched_getaffinity(0)) >= 4
                         and {0, 1, 2, 3} <= os.sched_getaffinity(0), "Linux with CPUs 0-3")
    def test_in_a_pinned_run_the_guard_runs_on_the_helper_cpus(self):
        harness = Harness(self, "yaque", ("--writer-cpu", "1", "--reader-cpu", "2", "--helper-cpus", "0,3"),
                          STANDIN_HANG="append_cold")
        harness.wait_alive(1)
        self.assertEqual(os.sched_getaffinity(harness.pid_of("guard")), {0, 3})
        self.assertEqual(os.sched_getaffinity(harness.proc.pid), {0, 3})
        os.kill(harness.proc.pid, signal.SIGTERM)
        self.assertEqual(harness.wait(), 128 + signal.SIGTERM, harness.output())
        self.assertEqual(harness.survivors(), {})

    def test_tt_latency_runs_in_the_runner_s_session_and_reports_through_a_pipe(self):
        harness = Harness(self, "tickertape")
        self.assertEqual(harness.wait(), 0, harness.output())
        latency = [(pid, json.loads((harness.pids / f"{pid}.ids").read_text()))
                   for pid, kind in harness.recorded().items() if kind == "tt-latency"]
        self.assertEqual(len(latency), 16)  # Two readers, two rates, two payloads, two repeats.
        for pid, ids in latency:
            self.assertEqual(ids, {"sid": harness.proc.pid, "pgid": pid, "stdout": "pipe"})
        # The adapters lead a session of their own, as they did at v1.0.0.
        for pid, kind in harness.recorded().items():
            if kind.startswith("tickertape "):
                ids = json.loads((harness.pids / f"{pid}.ids").read_text())
                self.assertEqual((ids["sid"], ids["pgid"], ids["stdout"]), (pid, pid, "file"), kind)
        self.assertEqual(len((harness.out / "tickertape-W2.jsonl").read_text().splitlines()), 16)

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

    @unittest.skipUnless(Path("/proc/self/stat").exists(), "Linux: start times from /proc")
    def test_the_guard_does_not_signal_a_group_whose_number_another_process_got(self):
        self.assertTrue(reaper.same_leader(os.getpid(), reaper.start_time(os.getpid())))
        self.assertFalse(reaper.same_leader(os.getpid(), reaper.start_time(os.getpid()) + 1))
        stranger = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(300)"], start_new_session=True)
        try:
            guard = reaper.Guard(grace=1)
            guard._send(f"add {stranger.pid} {reaper.start_time(stranger.pid) - 1}")  # The leader it recorded.
            guard.pipe.close()  # The runner dies without a release.
            guard.proc.wait(timeout=BOUND_S)
            self.assertIsNone(stranger.poll())
            guard = reaper.Guard(grace=1)
            guard.add(stranger.pid)  # The same number with the right start time is signalled.
            guard.pipe.close()
            guard.proc.wait(timeout=BOUND_S)
            self.assertEqual(stranger.wait(timeout=BOUND_S), -signal.SIGTERM)
        finally:
            if stranger.poll() is None:
                stranger.kill()
                stranger.wait()

    def test_the_guard_marks_only_the_directory_it_was_given(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "unit"
            out.mkdir()
            (out / "environment.txt").write_text("")
            old = reaper.identity(out)
            out.rename(Path(tmp) / "unit.incomplete-1")
            out.mkdir()
            (out / "environment.txt").write_text("")
            self.assertIsNone(reaper.mark_incomplete(out, old))
            self.assertEqual(reaper.mark_incomplete(out, reaper.identity(out)), Path(tmp) / "unit.incomplete-2")

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


class RunnerTests(unittest.TestCase):
    def test_python_older_than_3_11_is_refused_before_anything_runs(self):
        with mock.patch.object(run.sys, "version_info", (3, 10, 14)), \
                mock.patch.object(run.sys, "argv", ["run.py", "/nonexistent/out"]), \
                mock.patch.object(run.reaper, "install") as install:
            with self.assertRaisesRegex(SystemExit, "Python 3.11"):
                run.main()
        install.assert_not_called()


if __name__ == "__main__":
    unittest.main()
