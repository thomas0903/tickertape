"""Rerun driver: parts, unit order, resume, retries, deadline. Run: python3 -m unittest discover -s benches"""
import datetime
import json
from pathlib import Path
import subprocess
import sys
import tempfile
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
        self.assertEqual(rerun.PARTS["tuned-short"][0].options(["--writer-cpu", "1"], True),
                         ["--writer-cpu", "1", "--quick"])

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
            raise rerun.Interrupted("SIGALRM")
        with tempfile.TemporaryFile("w") as log:
            old = rerun.signal.signal(rerun.signal.SIGALRM, interrupt)
            rerun.signal.setitimer(rerun.signal.ITIMER_REAL, 0.5)
            try:
                with self.assertRaises(rerun.Interrupted):
                    rerun.run_child([sys.executable, "-c", "import time; time.sleep(60)"], log, None)
            finally:
                rerun.signal.signal(rerun.signal.SIGALRM, old)


class ShortToolsTests(unittest.TestCase):
    def test_short_mode_is_restricted_to_the_named_tools(self):
        code = ("import sys; sys.path.insert(0, %r); import rerun, types; m = types.ModuleType('run');"
                "m.TOOLS = rerun.TOOLS; m.main = lambda: print(m.TOOLS, sys.argv[1:]); sys.modules['run'] = m;"
                "rerun.main(['--short-tools', 'yaque', '--', '/o', '--quick'])" % str(Path(rerun.__file__).parent))
        done = subprocess.run([sys.executable, "-c", code], text=True, stdout=subprocess.PIPE, check=True)
        self.assertEqual(done.stdout.strip(), "('yaque',) ['/o', '--quick']")


if __name__ == "__main__":
    unittest.main()
