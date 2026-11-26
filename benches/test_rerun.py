"""Rerun driver: parts, unit order, resume, retries, deadline. Run: python3 -m unittest discover -s benches"""
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest import mock

import rerun


def args(**overrides):
    return SimpleNamespace(**{"tuned_options": [], "quick": False, "deadline": None, **overrides})


class PlanTests(unittest.TestCase):
    def test_unit_names_are_unique_and_tuned_parts_stand_alone(self):
        for part, units in rerun.PARTS.items():
            self.assertEqual(len({u.name for u in units}), len(units), part)
            self.assertIn({u.config == "tuned" for u in units}, ({True}, {False}), part)

    def test_short_part_interleaves_configurations_and_reverses_them_in_the_second_round(self):
        chronicle = [(u.name[-6:], u.config) for u in rerun.PARTS["short"] if u.tool == "chronicle"]
        self.assertEqual(chronicle, [("round1", "pinned"), ("round1", "unpinned"), ("round1", "jvm-old"),
                                     ("round1", "microtouch-only"), ("round2", "microtouch-only"),
                                     ("round2", "jvm-old"), ("round2", "pinned")])
        first = {u.name[-6:]: u.tool for u in reversed(rerun.PARTS["short"])}
        self.assertNotEqual(first["round1"], first["round2"])

    def test_options(self):
        micro = rerun.CONFIGS["microtouch-only"]
        self.assertNotIn("--no-micro-touch", micro)
        self.assertEqual(set(rerun.OLD_JVM) - set(micro), {"--no-micro-touch"})
        unit = rerun.PARTS["sustained-90"][0]
        self.assertEqual(unit.options([], False)[-6:],
                         ["--workloads", "probe,SD", "--rate-fractions", "0.9", "--repeats", "1"])
        unit = rerun.PARTS["tuned-short"][0]
        self.assertEqual(unit.options(["--writer-cpu", "1"], True),
                         ["--writer-cpu", "1", "--tools", unit.tool, "--quick"])

    def test_sustained_run_counts(self):
        unit = {u.tool: u for u in rerun.PARTS["sustained"]}
        self.assertEqual(unit["tickertape"].sustained_runs(False), 6)  # (probe, SV, SD) x two payloads
        self.assertEqual(unit["aeron"].sustained_runs(False), 4)       # no SD
        self.assertEqual(unit["aeron"].sustained_runs(True), 2)


class ExecuteTests(unittest.TestCase):
    def run_plan(self, results, manifest=None, **options):
        manifest = manifest if manifest is not None else {"units": {}}
        calls = []

        def runner(unit, directory, scratch, a, m, save):
            calls.append(unit.name)
            status = results[unit.name].pop(0)
            entry = m["units"].setdefault(f"{directory.parent.name}/{unit.name}", {"attempts": []})
            entry["attempts"].append({"status": status})
            entry["status"] = status
            return status
        plan = [("p-ext4", u) for u in rerun.PARTS["sustained-journals"]]
        left = rerun.execute(plan, Path("/out"), Path("/s"), args(**options), manifest, lambda: None, runner=runner)
        return calls, left, manifest

    def test_failed_units_run_once_more_at_the_end_and_resume_skips_finished_ones(self):
        a, b = [u.name for u in rerun.PARTS["sustained-journals"]]
        calls, left, manifest = self.run_plan({a: ["failed", "done"], b: ["done"]})
        self.assertEqual((calls, left), ([a, b, a], []))
        calls, left, _ = self.run_plan({}, manifest)
        self.assertEqual((calls, left), ([], []))
        calls, left, manifest = self.run_plan({a: ["failed", "failed"], b: ["done"]})
        self.assertEqual((calls, left), ([a, b, a], [f"p-ext4/{a}"]))
        self.assertEqual(self.run_plan({}, manifest)[0], [])  # two failures are the result

    def test_deadline(self):
        soon = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=5)
        calls, left, manifest = self.run_plan({}, deadline=soon)
        self.assertEqual((calls, len(left)), ([], 2))
        self.assertTrue(all(e["status"] == "not started: deadline" for e in manifest["units"].values()))


class UnitTests(unittest.TestCase):
    def test_an_unfinished_short_unit_is_set_aside_and_a_sustained_one_resumed(self):
        commands = []
        with tempfile.TemporaryDirectory() as tmp, \
                mock.patch.object(rerun, "run_child", lambda cmd, log, env: commands.append(cmd) or 0), \
                mock.patch.object(rerun.os, "sync"):
            out = Path(tmp)
            for unit, marker in ((rerun.PARTS["short"][0], "environment.txt"),
                                 (rerun.PARTS["sustained"][0], "matrix.json")):
                directory = out / "p-ext4" / unit.name
                directory.mkdir(parents=True)
                (directory / marker).write_text("{}")
                manifest = {"units": {}}
                status = rerun.run_unit(unit, directory, out / "scratch", args(), manifest, lambda: None)
                self.assertEqual(status, "done")
            short = out / "p-ext4" / rerun.PARTS["short"][0].name
            self.assertTrue(short.with_name(short.name + ".incomplete-1").joinpath("environment.txt").exists())
            self.assertNotIn("--resume", commands[0])
            self.assertEqual(commands[1][-1], "--resume")
            self.assertEqual(json.loads(json.dumps(manifest))["units"][f"p-ext4/{unit.name}"]["status"], "done")

    def test_interrupted_child_group_ends(self):
        def interrupt(signum, frame):
            raise rerun.reaper.Terminated(signum)
        with tempfile.TemporaryFile("w") as log:
            old = signal.signal(signal.SIGALRM, interrupt)
            signal.setitimer(signal.ITIMER_REAL, 0.5)
            try:
                with self.assertRaises(rerun.reaper.Terminated):
                    rerun.run_child([sys.executable, "-c", "import time; time.sleep(60)"], log, None)
            finally:
                signal.signal(signal.SIGALRM, old)

    def test_every_process_of_an_interrupted_unit_s_group_gets_the_grace_period(self):
        # A unit (run.py with a build step, say) that leaves a process in its group which takes a second to end
        # after SIGTERM, while the unit itself ends at once.
        with tempfile.TemporaryDirectory() as tmp, tempfile.TemporaryFile("w") as log:
            noted, pidfile = Path(tmp) / "member.term", Path(tmp) / "member.pid"
            member = (f"import os, pathlib, signal, time\n"
                      f"def term(signum, frame):\n"
                      f"    time.sleep(1); pathlib.Path({str(noted)!r}).write_text(''); os._exit(0)\n"
                      f"signal.signal(signal.SIGTERM, term)\n"
                      f"pathlib.Path({str(pidfile)!r}).write_text(str(os.getpid()))\n"
                      f"time.sleep(300)\n")
            unit = (f"import pathlib, subprocess, sys, time; subprocess.Popen([sys.executable, '-c', {member!r}]); "
                    f"p = pathlib.Path({str(pidfile)!r})\n"
                    f"while not p.exists() or not p.read_text(): time.sleep(0.01)\n"
                    f"time.sleep(300)")

            def interrupt(signum, frame):
                if pidfile.exists() and pidfile.read_text():
                    raise rerun.reaper.Terminated(signum)
                signal.setitimer(signal.ITIMER_REAL, 0.1)  # The member is not set up yet.
            old = signal.signal(signal.SIGALRM, interrupt)
            signal.setitimer(signal.ITIMER_REAL, 0.2)
            try:
                with self.assertRaises(rerun.reaper.Terminated):
                    rerun.run_child([sys.executable, "-c", unit], log, None)
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)
                signal.signal(signal.SIGALRM, old)
                if pidfile.exists() and pidfile.read_text() and not noted.exists():
                    try:
                        os.kill(int(pidfile.read_text()), signal.SIGKILL)  # Only if this test failed.
                    except ProcessLookupError:
                        pass
            self.assertTrue(noted.exists(), "the unit's member was killed without its grace period")


    def test_after_sigkill_of_the_driver_its_guard_ends_the_unit(self):
        with tempfile.TemporaryDirectory() as tmp:
            pidfile = Path(tmp) / "unit.pid"
            unit = (f"import os, pathlib, time; pathlib.Path({str(pidfile)!r}).write_text(str(os.getpid())); "
                    "time.sleep(300)")
            driver = (f"import sys; sys.path.insert(0, {str(Path(rerun.__file__).parent)!r}); import rerun; "
                      f"rerun.run_child([sys.executable, '-c', {unit!r}], sys.stderr, None)")
            proc = subprocess.Popen([sys.executable, "-c", driver], start_new_session=True)
            try:
                end = time.monotonic() + 30
                while not pidfile.exists() or not pidfile.read_text():
                    self.assertLess(time.monotonic(), end, "the unit did not start")
                    time.sleep(0.02)
                pid = int(pidfile.read_text())
                os.kill(proc.pid, signal.SIGKILL)
                proc.wait()
                end = time.monotonic() + 60  # The guard waits up to rerun.UNIT_GRACE_S after SIGTERM.
                while True:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        break
                    self.assertLess(time.monotonic(), end, "the unit outlived its driver")
                    time.sleep(0.05)
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
                # A unit left running (this test failed) would hold the test runner's stderr for 300 s.
                if pidfile.exists() and pidfile.read_text():
                    try:
                        os.kill(int(pidfile.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass


class ShortToolsTests(unittest.TestCase):
    def test_a_short_unit_runs_one_tool_with_run_py_tools(self):
        unit = rerun.PARTS["short"][0]
        cmd = rerun.command(unit, Path("/o/u"), [], True, False)
        self.assertEqual(cmd[1:], [str(rerun.COMPARE / "run.py"), "/o/u", "--tools", unit.tool, "--quick"])

    def test_the_old_short_tools_form_still_runs_the_same_command(self):
        # The commands recorded by the v1.0.0 rerun: `rerun.py --short-tools TOOL -- OUTDIR OPTIONS`.
        with mock.patch.object(rerun.os, "execv") as execv:
            rerun.main(["--short-tools", "yaque", "--", "/o", "--quick"])
        self.assertEqual(execv.call_args.args[1][1:],
                         [str(rerun.COMPARE / "run.py"), "/o", "--quick", "--tools", "yaque"])


if __name__ == "__main__":
    unittest.main()
