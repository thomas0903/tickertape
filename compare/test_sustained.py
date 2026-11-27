"""Sustained matrix: planning, derived rates, resume after interruption, and result validation."""
import argparse
import copy
import json
from pathlib import Path
import tempfile
import unittest

import sustained


def args(**overrides):
    values = dict(outdir=Path("/unused"), quick=False, duration=None, warmup=None, repeats=None, tools=None,
                  workloads=None, payloads=None, rates=None, fractions=None, sync_every_ms=1000, tt_readers=None,
                  resume=False, dry_run=False, keep_going=False, no_long_warmup=False, no_micro_touch=False,
                  chronicle_block_size="1073741824", aeron_sparse_terms=False, no_aeron_pre_touch=False, no_jvm_logs=False,
                  java_experiment=[], send_start_latency=False)
    values.update(overrides)
    return argparse.Namespace(**values)


def histogram(count):
    return {"count": count, "min_us": 0.5, "p50_us": 1, "p90_us": 2, "p99_us": 3, "p99_9_us": 4,
            "p99_99_us": 5, "max_us": 6}


def with_ladder(stats, scale=1.0):
    """A histogram with the live reader's percentile ladder; `scale` shifts every value but the maximum."""
    stats = dict(stats, percentiles_us={key: min(6, (1 + i * 0.4) * scale)
                                        for i, key in enumerate(sustained.LADDER[:-1])})
    stats["percentiles_us"]["max"] = stats["max_us"]
    return stats


def send_start_result():
    result = paced_result()
    result["send_start_latency"] = True
    result["reader_result"] = with_ladder(histogram(10_000))
    result["reader_result"]["from_send_start"] = with_ladder(histogram(10_000), 0.9)
    return result


def paced_result(workload="SV", tool="plain-file"):
    result = {"workload": workload, "tool": tool, "count": 11_000, "warmup": 1_000,
              "writer": {"count": 11_000, "planned_count": 11_000, "achieved_records_per_s": 1000.0,
                         "late_records": 5, "periodic_sync": None},
              "reader_result": histogram(10_000)}
    if workload == "SD":
        result["writer"]["periodic_sync"] = {"count": 10, "latency": histogram(10)}
    return result


class PlanTests(unittest.TestCase):
    def test_probes_first_unsupported_cases_left_out_and_labels_unique(self):
        params = sustained.parameters(args())
        runs = sustained.plan(params)
        probes = [r for r in runs if r.workload == "probe"]
        self.assertEqual(runs[:len(probes)], probes)
        self.assertEqual(len(probes), 2 * 6)
        self.assertEqual(len({r.label for r in runs}), len(runs))
        sd_tools = {r.tool for r in runs if r.workload == "SD"}
        self.assertEqual(sd_tools, {"tickertape", "chronicle", "queue-file", "plain-file"})
        # 2 repeats x 2 payloads x (6 tools SV + 4 tools SD) x 2 rates, plus probes.
        self.assertEqual(len(runs), 12 + 2 * 2 * 10 * 2)
        self.assertEqual({r.rate_tag for r in runs if r.workload != "probe"}, {"f50", "f90"})
        self.assertIn("aeron/SD", sustained.unsupported(params))
        self.assertIn("queue-file/reader", sustained.unsupported(params))

    def test_explicit_rates_need_no_probe_and_tickertape_readers_multiply(self):
        params = sustained.parameters(args(rates="1000,5000", workloads="SV", tools="tickertape,aeron",
                                           tt_readers="safe,mapped", payloads="64", repeats=3))
        runs = sustained.plan(params)
        self.assertFalse(any(r.workload == "probe" for r in runs))
        self.assertEqual(len(runs), 3 * (2 + 1) * 2)
        self.assertEqual(runs[0].label, "SV-tickertape-safe-64-r1000-r1")
        with self.assertRaises(ValueError):
            sustained.parameters(args(workloads="SV"))  # derived rates without probes
        for bad in [dict(tools="kafka"), dict(payloads="128"), dict(rates="0"), dict(fractions="1.5"),
                    dict(rates="10", fractions="0.5"), dict(tools="aeron,aeron"), dict(duration=0)]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                sustained.parameters(args(**bad))

    def test_payload_of_256_bytes_is_opt_in(self):
        self.assertEqual(sustained.parameters(args())["payloads"], [64, 1024])
        params = sustained.parameters(args(payloads="256", rates="100000", workloads="SV", tools="tickertape,chronicle",
                                           tt_readers="mapped,safe", repeats=1))
        self.assertEqual([r.label for r in sustained.plan(params)],
                         ["SV-tickertape-mapped-256-r100000-r1", "SV-tickertape-safe-256-r100000-r1",
                          "SV-chronicle-default-256-r100000-r1"])

    def test_quick_defaults(self):
        params = sustained.parameters(args(quick=True))
        self.assertEqual((params["duration_s"], params["warmup_s"], params["repeats"], params["payloads"]),
                         (5, 1, 1, [64]))


class JavaSettingsTests(unittest.TestCase):
    def test_settings_are_part_of_the_matrix_and_of_every_child_command(self):
        params = sustained.parameters(args(no_micro_touch=True))
        self.assertEqual(params["java"], {**sustained.javaopts.DEFAULTS, "micro_touch": False})
        probe = sustained.Run("probe-chronicle", "chronicle", "probe", 64, "default", "unpaced", 1)
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            cmd = sustained.adapter_command(probe, "writer", "d", "c", 10, 0, None, params, out)
            # The probe warms up too, with the long warm-up.
            self.assertEqual(cmd[cmd.index("--warmup-mode") + 1], "long")
            self.assertEqual(cmd[cmd.index("--jit-warmup") + 1], "1200000")
            self.assertEqual(cmd[cmd.index("--micro-touch") + 1], "off")
            self.assertTrue(any(c.startswith("--jvm=-Xlog:gc*") for c in cmd))
            rust = sustained.adapter_command(sustained.Run("x", "yaque", "probe", 64, "default", "unpaced", 1),
                                             "writer", "d", "c", 10, 0, None, params, out)
            self.assertFalse(any(c.startswith("--jvm=") or c == "--warmup-mode" for c in rust))
            # A matrix started with other JVM settings is not continued.
            sustained.prepare_output(out / "m", params, resume=False)
            with self.assertRaises(ValueError):
                sustained.prepare_output(out / "m", sustained.parameters(args()), resume=True)


class SendStartTests(unittest.TestCase):
    def test_paced_children_get_the_option_and_probes_do_not(self):
        off = sustained.parameters(args(rates="1000", workloads="probe,SV", tools="tickertape,chronicle"))
        on = sustained.parameters(args(rates="1000", workloads="probe,SV", tools="tickertape,chronicle",
                                       send_start_latency=True))
        self.assertEqual((off["send_start_latency"], on["send_start_latency"]), (False, True))
        with tempfile.TemporaryDirectory() as tmp:
            for run in sustained.plan(on):
                for role in ("writer", "reader"):
                    with self.subTest(run=run.label, role=role):
                        cmd = sustained.adapter_command(run, role, "d", "c", 10, 0, 1000, on, Path(tmp))
                        if run.workload == "probe":
                            self.assertNotIn("--send-start", cmd)
                        else:
                            self.assertEqual(cmd[cmd.index("--send-start") + 1], "on")
                        self.assertNotIn("--send-start",
                                         sustained.adapter_command(run, role, "d", "c", 10, 0, 1000, off, Path(tmp)))

    def test_a_matrix_from_before_the_option_resumes_without_it_only(self):
        params = sustained.parameters(args(rates="1000", workloads="SV", tools="yaque"))
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "m"
            sustained.prepare_output(out, params, resume=False)
            matrix = json.loads((out / "matrix.json").read_text())
            del matrix["parameters"]["send_start_latency"]
            (out / "matrix.json").write_text(json.dumps(matrix))
            self.assertFalse(sustained.prepare_output(out, params, resume=True))
            with self.assertRaises(ValueError):
                sustained.prepare_output(out, dict(params, send_start_latency=True), resume=True)

    def test_validation(self):
        sustained.validate_sustained(send_start_result())
        missing = send_start_result()
        del missing["reader_result"]["from_send_start"]
        above = send_start_result()
        above["reader_result"]["from_send_start"]["percentiles_us"]["99.9"] = 5.9
        fewer = send_start_result()
        fewer["reader_result"]["from_send_start"]["count"] = 9_999
        disordered = send_start_result()
        disordered["reader_result"]["percentiles_us"]["99.97"] = 0.1
        other_max = send_start_result()
        other_max["reader_result"]["percentiles_us"]["max"] = 7
        for name, result in [("missing", missing), ("above", above), ("fewer", fewer), ("disordered", disordered),
                             ("other maximum", other_max)]:
            with self.subTest(name), self.assertRaises((AssertionError, KeyError)):
                sustained.validate_sustained(result)


class RateTests(unittest.TestCase):
    def test_rates_derive_from_the_matching_probe(self):
        params = sustained.parameters(args(payloads="64", tools="yaque"))
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            run = next(r for r in sustained.plan(params) if r.rate_tag == "f90")
            with self.assertRaises(RuntimeError):
                sustained.resolve_rate(run, params, out)
            sustained.write_atomic(out / "runs" / "probe-yaque-default-64-unpaced-r1.json",
                                   json.dumps({"writer": {"achieved_records_per_s": 123_456.7}}))
            self.assertEqual(sustained.resolve_rate(run, params, out), 111_111)
            half = next(r for r in sustained.plan(params) if r.rate_tag == "f50")
            self.assertEqual(sustained.resolve_rate(half, params, out), 61_728)
            explicit = sustained.Run("x", "yaque", "SV", 64, "default", "r777", 1)
            self.assertEqual(sustained.resolve_rate(explicit, params, out), 777)


class ResumeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.out = Path(self.tmp.name)
        self.params = sustained.parameters(args(rates="1000", tools="plain-file,yaque", payloads="64", repeats=1,
                                                workloads="SV,SD"))
        self.runs = sustained.plan(self.params)
        self.calls = []

    def tearDown(self):
        self.tmp.cleanup()

    def attempt(self, fail=()):
        def fn(run, params, out, env, commands):
            self.calls.append(run.label)
            if run.label in fail:
                raise RuntimeError("child exited 1")
            result = paced_result(run.workload, run.tool)
            result["label"] = run.label
            return result
        return fn

    def test_finished_runs_are_skipped_on_restart_and_failures_retried(self):
        sustained.prepare_output(self.out, self.params, resume=False)
        labels = [r.label for r in self.runs]
        self.assertEqual(labels, ["SV-plain-file-default-64-r1000-r1", "SD-plain-file-default-64-r1000-r1",
                                  "SV-yaque-default-64-r1000-r1"])
        # First session: the second run fails; --keep-going records it and continues.
        counts = sustained.execute(self.runs, self.params, self.out, {}, True, self.attempt(fail={labels[1]}),
                                   log=lambda *a, **k: None)
        self.assertEqual((counts["done"], counts["failed"]), (2, 1))
        self.assertTrue((self.out / "runs" / f"{labels[0]}.json").exists())
        self.assertFalse((self.out / "runs" / f"{labels[1]}.json").exists())
        failure = json.loads((self.out / "failures.jsonl").read_text())
        self.assertEqual(failure["label"], labels[1])
        # Restart: only the failed run executes again.
        self.calls.clear()
        self.assertFalse(sustained.prepare_output(self.out, self.params, resume=True))
        counts = sustained.execute(self.runs, self.params, self.out, {}, False, self.attempt(),
                                   log=lambda *a, **k: None)
        self.assertEqual(self.calls, [labels[1]])
        self.assertEqual((counts["done"], counts["already"]), (1, 2))
        self.assertEqual(sustained.collect(self.out), 3)
        self.assertEqual(sustained.estimate_seconds(self.runs, self.params, self.out), 0)

    def test_without_keep_going_the_first_failure_stops_and_leaves_no_result(self):
        sustained.prepare_output(self.out, self.params, resume=False)
        with self.assertRaises(RuntimeError):
            sustained.execute(self.runs, self.params, self.out, {}, False, self.attempt(fail={self.runs[0].label}),
                              log=lambda *a, **k: None)
        self.assertEqual(self.calls, [self.runs[0].label])
        self.assertEqual(list((self.out / "runs").glob("*.json")) if (self.out / "runs").exists() else [], [])

    def test_skipped_runs_count_as_finished(self):
        sustained.prepare_output(self.out, self.params, resume=False)
        skip = self.runs[0]

        def fn(run, params, out, env, commands):
            self.calls.append(run.label)
            return {"skipped": "disk"} if run == skip else self.attempt()(run, params, out, env, commands)
        sustained.execute(self.runs, self.params, self.out, {}, False, fn, log=lambda *a, **k: None)
        self.calls.clear()
        sustained.execute(self.runs, self.params, self.out, {}, False, fn, log=lambda *a, **k: None)
        self.assertEqual(self.calls, [])
        self.assertEqual(json.loads((self.out / "skipped" / f"{skip.label}.json").read_text())["reason"], "disk")

    def test_output_directory_must_match_the_matrix(self):
        (self.out / "stray").write_text("x")
        with self.assertRaises(ValueError):
            sustained.prepare_output(self.out, self.params, resume=True)  # no matrix.json
        (self.out / "stray").unlink()
        self.assertTrue(sustained.prepare_output(self.out, self.params, resume=False))
        with self.assertRaises(ValueError):
            sustained.prepare_output(self.out, self.params, resume=False)  # not empty, no --resume
        other = dict(self.params, duration_s=60)
        with self.assertRaises(ValueError):
            sustained.prepare_output(self.out, other, resume=True)

    def test_a_matrix_resumes_only_with_the_same_pinning(self):
        none = {"mode": "none", "summary": "affinity: none (--no-pin)"}
        pinned = {"mode": "pinned", "writer_cpu": 2, "reader_cpu": 3, "helper_cpus": [0, 1]}
        # Started before pinning existed: unpinned.
        sustained.prepare_output(self.out, self.params, resume=False)
        self.assertFalse(sustained.prepare_output(self.out, dict(self.params, affinity=none), resume=True))
        with self.assertRaises(ValueError):
            sustained.prepare_output(self.out, dict(self.params, affinity=pinned), resume=True)
        command = sustained.adapter_command(self.runs[0], "reader", "d", "c", 1, 0, 1, dict(self.params, affinity=pinned))
        self.assertEqual(command[-4:], ["--cpu", "3", "--helper-cpus", "0,1"])
        command = sustained.adapter_command(self.runs[0], "reader", "d", "c", 1, 0, 1, dict(self.params, affinity=none))
        self.assertNotIn("--cpu", command)

    def test_estimate_counts_only_unfinished_runs(self):
        full = sustained.estimate_seconds(self.runs, self.params, self.out)
        self.assertEqual(full, 3 * (600 + 10 + sustained.DEFAULT_OVERHEAD_S + sustained.SYNC_S))
        sustained.write_atomic(self.out / "runs" / f"{self.runs[0].label}.json", "{}")
        self.assertEqual(sustained.estimate_seconds(self.runs, self.params, self.out), full * 2 / 3)
        self.assertEqual(sustained.format_seconds(3725), "1h02m05s")


class ValidationTests(unittest.TestCase):
    def test_accepts_complete_runs(self):
        sustained.validate_sustained(paced_result())
        sustained.validate_sustained(paced_result("SD"))
        writer_only = paced_result(tool="queue-file")
        writer_only["reader_result"] = None
        sustained.validate_sustained(writer_only)
        probe = {"workload": "probe", "tool": "tickertape", "count": 10**9, "warmup": 0,
                 "writer": {"count": 5000, "planned_count": 10**9, "achieved_records_per_s": 2e6, "late_records": None},
                 "reader_result": {"count": 5000, "latency": histogram(5000)}}
        sustained.validate_sustained(probe)
        short = copy.deepcopy(probe)
        short["reader_result"]["count"] = 4999
        with self.assertRaises(AssertionError):
            sustained.validate_sustained(short)

    def test_rejects_dropped_records_missing_syncs_and_invalid_statistics(self):
        cases = [
            ("reader_result", "count", 9_999),             # a dropped record
            ("writer", "count", 10_999),                   # a paced writer that skipped a send
            ("writer", "late_records", 11_001),
            ("writer", "achieved_records_per_s", 0),
            ("reader_result", "p99_us", 0.1),              # percentiles out of order
            ("reader_result", "max_us", float("nan")),
        ]
        for section, key, value in cases:
            with self.subTest(key=key, value=value):
                result = paced_result()
                result[section][key] = value
                with self.assertRaises(AssertionError):
                    sustained.validate_sustained(result)
        no_sync = paced_result("SD")
        no_sync["writer"]["periodic_sync"]["count"] = 0
        with self.assertRaises(AssertionError):
            sustained.validate_sustained(no_sync)
        missing_reader = paced_result()
        missing_reader["reader_result"] = None
        with self.assertRaises(AssertionError):
            sustained.validate_sustained(missing_reader)


if __name__ == "__main__":
    unittest.main()
