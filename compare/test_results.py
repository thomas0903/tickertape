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
