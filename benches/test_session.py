"""Session runner: plan order, scenario sizing, estimates and resume. Run: python3 -m unittest discover -s benches"""
import json
from pathlib import Path
import signal
import tempfile
import unittest
from unittest import mock

import session

GIB = 1 << 30


def params(**overrides):
    args = session.parse_args(["--journal-dir", "/tmp", "--out", "/tmp/x", *overrides.pop("argv", [])])
    p = session.parameters(args)
    p.update(overrides)
    return p


class PlanTests(unittest.TestCase):
    def test_probes_come_first_and_labels_are_unique(self):
        runs = session.tt_plan(params())
        self.assertEqual([r.label for r in runs[:2]], ["tt-probe-64-r1", "tt-probe-1024-r1"])
        self.assertEqual(len({r.label for r in runs}), len(runs))
        # 2 probes + 2 repeats x 2 payloads x 6 other scenarios.
        self.assertEqual(len(runs), 2 + 2 * 2 * 6)
        quick = session.tt_plan(params(argv=["--quick"]))
        self.assertEqual([r.label for r in quick], ["tt-probe-64-r1", "tt-steady-50-64-r1", "tt-bursty-64-r1",
                                                    "tt-lagging-64-r1", "tt-visibility-only-64-r1"])
        # Scenarios that derive their rate always get the probe.
        only = session.tt_plan(params(argv=["--scenarios", "bursty", "--repeats", "1"]))
        self.assertEqual([r.scenario for r in only], ["probe", "probe", "bursty", "bursty"])
        with self.assertRaises(SystemExit):
            params(argv=["--scenarios", "nope"])


class ScenarioTests(unittest.TestCase):
    host = {"ram": 64 * GIB, "free": 1000 * GIB}

    def args_for(self, scenario, payload=64, rate=1_000_000, host=None, **p):
        run = session.TtRun(f"tt-{scenario}-{payload}-r1", scenario, payload, 1)
        args, seconds = session.scenario_args(run, None if scenario == "probe" else rate, host or self.host, params(**p))
        return dict(zip(args[::2], args[1::2])) if "--unpaced" not in args else args, seconds

    def test_rates_derive_from_the_probe(self):
        steady, seconds = self.args_for("steady-90")
        self.assertEqual(steady["--rate"], "900000")
        self.assertEqual(seconds, 610)
        self.assertEqual(steady["--readers"], "mapped,safe:wait=default,safe:durable:wait=block")
        self.assertEqual(steady["--checkpoint-every"], "1s")
        self.assertEqual(int(steady["--retain-bytes"]), 64 * GIB)
        bursty, _ = self.args_for("bursty")
        self.assertEqual((bursty["--burst-rate"], bursty["--burst-duty"], bursty["--burst-period"]),
                         ("2000000", "20", "1s"))
        probe, seconds = self.args_for("probe")
        self.assertIn("--unpaced", probe)
        self.assertEqual(seconds, 600)

    def test_lagging_reader_reads_at_a_quarter_of_the_rate_inside_a_small_window(self):
        lagging, _ = self.args_for("lagging", rate=200_000)
        self.assertEqual(lagging["--rate"], "100000")
        self.assertIn("safe:delay=40000ns", lagging["--readers"])
        window = int(lagging["--retain-bytes"])
        self.assertEqual(window % (16 << 20), 0)
        self.assertLessEqual(window, 100_000 * 104 * 10)

    def test_beyond_ram_runs_long_enough_to_append_one_and_a_half_times_ram(self):
        big, seconds = self.args_for("beyond-ram", payload=1024, rate=100_000)
        rate = int(big["--rate"])
        self.assertEqual(rate, 90_000)
        duration = int(big["--duration"].rstrip("s"))
        self.assertGreaterEqual(duration * rate * 1064, 1.5 * 64 * GIB)
        self.assertEqual(seconds, duration + 10)
        self.assertLessEqual(int(big["--retain-bytes"]), 1.5 * 64 * GIB)
        capped, _ = self.args_for("beyond-ram", payload=64, rate=1000, max_run_s=3600)
        self.assertEqual(capped["--duration"], "3600s")
        fast, _ = self.args_for("beyond-ram", payload=1024, rate=10_000_000)
        self.assertEqual(fast["--duration"], "600s")

    def test_visibility_only_has_no_checkpoints_and_fits_on_the_volume(self):
        small = {"ram": 8 * GIB, "free": 20 * GIB}
        vis, _ = self.args_for("visibility-only", rate=10_000_000, host=small)
        self.assertNotIn("--checkpoint-every", vis)
        self.assertNotIn("--retain-bytes", vis)
        self.assertLessEqual(int(vis["--rate"]) * 104 * 610, 0.5 * 20 * GIB)
        with self.assertRaises(session.Skip):
            self.args_for("steady-50", host={"ram": GIB, "free": GIB})


class CheckpointModeTests(unittest.TestCase):
    host = {"ram": 64 * GIB, "free": 1000 * GIB}

    def args_for(self, scenario, argv):
        run = session.TtRun(f"tt-{scenario}-64-r1", scenario, 64, 1)
        return session.scenario_args(run, None if scenario == "probe" else 1_000_000, self.host,
                                     params(argv=argv))[0]

    def test_background_mode_reaches_every_run_that_checkpoints(self):
        for scenario in session.SCENARIOS:
            background = self.args_for(scenario, ["--checkpoint-mode", "background"])
            blocking = self.args_for(scenario, [])
            self.assertNotIn("--checkpoint-mode", blocking)
            if "--checkpoint-every" in blocking:
                self.assertEqual(background, [*blocking, "--checkpoint-mode", "background"])
            else:
                self.assertEqual(background, blocking)

    def test_default_mode_keeps_the_parameters_of_older_sessions(self):
        self.assertNotIn("checkpoint_mode", params())
        self.assertEqual(params(argv=["--checkpoint-mode", "background"])["checkpoint_mode"], "background")


class EstimateAndResumeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        self.params = params(argv=["--quick"])
        self.runs = session.tt_plan(self.params)
        self.host = {"ram": 16 * GIB, "free": 500 * GIB}

    def tearDown(self):
        self.tmp.cleanup()

    def finish(self, label, achieved=1_000_000.0):
        path = self.out / "tickertape" / label / "result.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"writer": {"achieved_records_per_s": achieved}}))

    def test_estimate_shrinks_as_runs_finish(self):
        full = session.estimate_tt(self.runs, self.out, self.host, self.params, {})
        self.assertEqual(full, 10 + 4 * 11 + 5 * session.TT_OVERHEAD_S)
        self.finish("tt-probe-64-r1")
        self.assertEqual(session.estimate_tt(self.runs, self.out, self.host, self.params, {}),
                         full - 10 - session.TT_OVERHEAD_S)

    def test_finished_and_skipped_runs_are_not_repeated(self):
        calls = []

        def runner(run, args, seconds, commands):
            calls.append(run.label)
            if run.label != "tt-bursty-64-r1":
                self.finish(run.label)
            return "done" if run.label != "tt-bursty-64-r1" else "failed"
        manifest = {"runs": {}}
        tiny = {"ram": GIB, "free": 100 * GIB}
        counts = session.execute_tt(self.runs, self.out, self.out, tiny, self.params, manifest, runner)
        self.assertEqual(calls, [r.label for r in self.runs])
        self.assertEqual(counts["failed"], 1)
        calls.clear()
        counts = session.execute_tt(self.runs, self.out, self.out, tiny, self.params, manifest, runner)
        self.assertEqual(calls, ["tt-bursty-64-r1"])
        self.assertEqual(counts["already"], 4)

    def test_runs_without_a_probe_result_fail_visibly(self):
        manifest = {"runs": {}}
        counts = session.execute_tt(self.runs[1:2], self.out, self.out, self.host, self.params, manifest,
                                    lambda *a: self.fail("must not run"))
        self.assertEqual(counts["failed"], 1)
        self.assertIn("probe", manifest["runs"]["tt-steady-50-64-r1"]["error"])

    def test_session_refuses_a_different_matrix_in_the_same_output(self):
        (self.out / "manifest.json").write_text(json.dumps({"parameters": {"other": 1}, "runs": {}}))
        with self.assertRaises(SystemExit):
            session.main(["--journal-dir", tempfile.gettempdir(), "--out", str(self.out), "--quick", "--dry-run",
                          "--only", "tickertape"])
        (self.out / "manifest.json").unlink()
        (self.out / "stray").write_text("x")
        with self.assertRaises(SystemExit):
            session.main(["--journal-dir", tempfile.gettempdir(), "--out", str(self.out), "--quick", "--dry-run",
                          "--only", "tickertape"])

    def test_scratch_directory_is_owned_and_reset(self):
        journal = self.out / "fs"
        journal.mkdir()
        scratch = session.reset_scratch(journal, self.out / "o")
        (scratch / "leftover").write_text("stale")
        again = session.reset_scratch(journal, self.out / "o")
        self.assertEqual(scratch, again)
        self.assertFalse((again / "leftover").exists())
        (again / "owner.txt").write_text("someone else")
        with self.assertRaises(SystemExit):
            session.reset_scratch(journal, self.out / "o")


class CompareOnlyTests(unittest.TestCase):
    def test_compare_only_session_creates_its_log_directory(self):
        """--only compare on a fresh output directory opens logs/compare.log without a tickertape run before it."""
        class Runner:  # Stands in for compare/run.py's process.
            pid = 0

            def __init__(self, command, stdout, **kwargs):
                stdout.write("comparison ran\n")

            def wait(self):
                return 0

            def poll(self):
                return 0
        handlers = {sig: signal.getsignal(sig) for sig in (signal.SIGTERM, signal.SIGHUP)}
        self.addCleanup(lambda: [signal.signal(sig, h) for sig, h in handlers.items()])
        with tempfile.TemporaryDirectory() as tmp:
            journal, out = Path(tmp) / "fs", Path(tmp) / "out"
            journal.mkdir()
            # Every other subprocess of the session is replaced too, since Popen is patched for the whole module.
            with mock.patch.object(session, "compare_estimate", return_value=0), \
                    mock.patch.object(session, "capture_environment"), \
                    mock.patch.object(session, "capture", return_value=""), \
                    mock.patch.object(session, "host_facts", return_value={"ram": 16 * GIB, "free": 500 * GIB}), \
                    mock.patch.object(session.subprocess, "Popen", Runner):
                status = session.main(["--journal-dir", str(journal), "--out", str(out), "--quick", "--only", "compare",
                                       "--yes"])
            self.assertEqual(status, 0)
            self.assertEqual((out / "logs" / "compare.log").read_text(), "comparison ran\n")
            self.assertEqual(json.loads((out / "manifest.json").read_text())["compare"]["status"], "done")


if __name__ == "__main__":
    unittest.main()
