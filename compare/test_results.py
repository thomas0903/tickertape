"""Result validation guards for failed or misleading measurements."""
import copy
import unittest
from run import validate_result


class ResultTests(unittest.TestCase):
    def setUp(self):
        self.result = {
            "workload": "W2", "count": 1100, "warmup": 100,
            "latency": {"count": 1000, "min_us": 0, "p50_us": 1, "p90_us": 2,
                        "p99_us": 3, "p99_9_us": 4, "p99_99_us": 5, "max_us": 6},
            "writer": {"count": 1100, "late_records": 100, "achieved_records_per_s": 10000},
        }

    def test_rejects_skipped_sends_wrong_warmup_and_invalid_histograms(self):
        validate_result(self.result)
        for section, key, value in [("latency", "count", 999), ("writer", "count", 1000),
                                    ("latency", "p99_us", 0), ("latency", "max_us", float("nan")),
                                    ("writer", "late_records", 1101),
                                    ("writer", "achieved_records_per_s", 0)]:
            with self.subTest(section=section, key=key):
                result = copy.deepcopy(self.result)
                result[section][key] = value
                with self.assertRaises(AssertionError):
                    validate_result(result)

    def test_payload_rate_excludes_envelope_and_requires_complete_scan(self):
        result = {"workload": "W3", "count": 2000, "payload": 64,
                  "replay": {"count": 2000, "records_per_s": 1000000, "payload_mb_per_s": 64}}
        validate_result(result)
        for key, value in [("count", 1999), ("payload_mb_per_s", 80)]:
            with self.subTest(key=key):
                bad = copy.deepcopy(result)
                bad["replay"][key] = value
                with self.assertRaises(AssertionError):
                    validate_result(bad)


if __name__ == "__main__":
    unittest.main()


class ClockRetryTests(unittest.TestCase):
    def run_with(self, failures):
        import json
        import tempfile
        from pathlib import Path
        import run
        calls = []

        def attempt(*args):
            out = args[6]
            calls.append(1)
            if len(calls) <= len(failures):
                with (out / "children.log").open("a") as log:
                    log.write(failures[len(calls) - 1] + "\n")
                raise RuntimeError("child exited 1; see children.log")
            return "ok"

        original = run.run_attempt
        run.run_attempt = attempt
        try:
            with tempfile.TemporaryDirectory() as tmp:
                out = Path(tmp)
                try:
                    result = run.run_one("yaque", "W2", 64, "default", 10000, 2, out, {}, False, None)
                except RuntimeError:
                    result = None
                retries = out / "retries.jsonl"
                logged = [json.loads(line) for line in retries.read_text().splitlines()] if retries.exists() else []
        finally:
            run.run_attempt = original
        return result, len(calls), logged

    def test_clock_steps_are_retried_and_recorded(self):
        result, calls, logged = self.run_with(["yaque: wall clock moved backwards"] * 2)
        self.assertEqual((result, calls), ("ok", 3))
        self.assertEqual([r["attempt"] for r in logged], [1, 2])
        self.assertEqual(logged[0]["label"], "yaque-W2-64-default-10000-2")

    def test_other_failures_and_too_many_clock_steps_stop_the_suite(self):
        self.assertEqual(self.run_with(["validation failed"])[:2], (None, 1))
        result, calls, logged = self.run_with(["wall clock moved backwards"] * 4)
        self.assertEqual((result, calls, len(logged)), (None, 4, 3))
