"""Settings of the JVM tools: defaults, switches, recorded settings, and the attribution of late sends."""
import argparse
import copy
from pathlib import Path
import tempfile
import unittest

import attribute
import javaopts


def settings(*argv):
    parser = argparse.ArgumentParser()
    javaopts.add_arguments(parser)
    return javaopts.settings(parser.parse_args(argv))


def child(java, **tool):
    return {"warmup": {"mode": "long" if java["long_warmup"] else "short"}, "tool": tool}


class SettingsTests(unittest.TestCase):
    def test_new_behaviour_is_the_default_and_each_part_switches_off_alone(self):
        self.assertEqual(settings(), javaopts.DEFAULTS)
        self.assertEqual(javaopts.DEFAULTS["chronicle_block_size"], 1 << 30)
        for argv, key, value in [(["--no-long-warmup"], "long_warmup", False),
                                 (["--no-micro-touch"], "micro_touch", False),
                                 (["--chronicle-block-size", "default"], "chronicle_block_size", "default"),
                                 (["--no-jvm-logs"], "jvm_logs", False),
                                 (["--java-experiment", "epsilon"], "experiments", ["epsilon"])]:
            with self.subTest(argv=argv):
                self.assertEqual(settings(*argv), {**javaopts.DEFAULTS, key: value})
        for bad in ("0", "1000", "1G"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                settings("--chronicle-block-size", bad)

    def test_adapter_arguments_for_the_new_and_the_old_defaults(self):
        new = javaopts.adapter_arguments("chronicle", settings(), False, False, "e.json")
        self.assertEqual(new, ["--jit-warmup", "1200000", "--warmup-mode", "long", "--micro-touch", "on",
                               "--block-size", "1073741824", "--events", "e.json"])
        self.assertGreater(javaopts.LONG_WARMUP_RECORDS, 4096 * 256)
        old = settings("--no-long-warmup", "--no-micro-touch", "--chronicle-block-size", "default", "--no-jvm-logs")
        self.assertEqual(javaopts.adapter_arguments("chronicle", old, False, False, None),
                         ["--jit-warmup", "100000", "--warmup-mode", "short", "--micro-touch", "off",
                          "--block-size", "default"])
        self.assertEqual(javaopts.warmup_records(old, True, False), 1000)
        self.assertEqual(javaopts.warmup_records(old, False, True), 1000)
        # Chronicle's settings are not passed to other JVM tools; the warm-up is.
        self.assertEqual(javaopts.adapter_arguments("aeron", settings("--no-jvm-logs"), False, True, None),
                         ["--jit-warmup", "1200000", "--warmup-mode", "long"])

    def test_child_command_carries_the_jvm_flags_and_log_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            cmd = javaopts.wrap(["run.sh", "writer", "--count", "1"], "aeron", settings("--java-experiment", "epsilon"),
                                False, False, out, "label-writer")
            self.assertEqual(cmd[0], "run.sh")
            self.assertLess(cmd.index("--jvm=-XX:-UseParallelGC"), cmd.index("--jvm=-XX:+UseEpsilonGC"))
            log = next(c for c in cmd if c.startswith("--jvm=-Xlog:gc*,safepoint,jit+compilation"))
            self.assertIn(str(out / "jvm-logs" / "label-writer.log"), log)
            self.assertIn("--jvm=-Xlog:async", cmd)
            self.assertLess(cmd.index(log), cmd.index("writer"))  # run.sh takes --jvm= before the role.
            self.assertEqual(cmd[cmd.index("--events") + 1], str(out / "jvm-logs" / "label-writer.events.json"))
            self.assertTrue((out / "jvm-logs").is_dir())
            off = javaopts.wrap(["run.sh", "writer"], "aeron", settings("--no-jvm-logs"), False, False, out, "x")
            self.assertEqual(off[:2], ["run.sh", "writer"])
            self.assertNotIn("--events", off)


class RecordedSettingsTests(unittest.TestCase):
    def test_settings_move_out_of_the_measurements_into_one_block(self):
        java = settings()
        writer = {"count": 1, "java": child(java, block_size=1 << 30, micro_touch=True)}
        reader = {"p50_us": 1, "java": child(java, block_size=1 << 30)}
        block = javaopts.collect("chronicle", java, {"writer": writer, "reader": reader})
        self.assertEqual((writer, reader), ({"count": 1}, {"p50_us": 1}))
        self.assertEqual(block["settings"], java)
        self.assertTrue(block["writer"]["tool"]["micro_touch"])
        self.assertIsNone(javaopts.collect("tickertape", java, {"writer": {"count": 1}, "reader": None}))

    def test_rejects_a_child_that_ran_with_other_settings(self):
        java = settings()
        good = {"writer": {"java": child(java, block_size=1 << 30, micro_touch=True)},
                "reader": {"java": child(java, block_size=1 << 30)}}
        javaopts.collect("chronicle", java, copy.deepcopy(good))
        for role, path, value in [("writer", ("warmup", "mode"), "short"), ("reader", ("warmup", "mode"), "short"),
                                  ("writer", ("tool", "micro_touch"), False),
                                  ("writer", ("tool", "block_size"), 64 << 20),
                                  ("reader", ("tool", "block_size"), 64 << 20)]:
            with self.subTest(role=role, path=path):
                bad = copy.deepcopy(good)
                bad[role]["java"][path[0]][path[1]] = value
                with self.assertRaises(AssertionError):
                    javaopts.collect("chronicle", java, bad)


class AttributionTests(unittest.TestCase):
    LOG = """\
[2026-11-25T10:34:31.983+0000][500ns][deoptimization ] cid=  1     level=4 T.before(I)I trap_bci=1 unstable_if reinterpret pc=0x1 relative_pc=0x1
[2026-11-25T10:34:31.996+0000][20000ns][deoptimization ] cid=  83 osr level=4 T.main([Ljava/lang/String;)V trap_bci=21 osr_bci=18 unstable_if reinterpret pc=0x1 relative_pc=0x1
[2026-11-25T10:34:31.996+0000][20100ns][jit,compilation]   83 %     4       T::main @ 18 (77 bytes)   made not entrant
[2026-11-25T10:34:32.003+0000][30000ns][safepoint      ] Safepoint "ParallelGCFailedAllocation", Time since last: 5 ns, Reaching safepoint: 100 ns, Cleanup: 1 ns, At safepoint: 4000 ns, Leaving safepoint: 1 ns, Total: 5000 ns
[2026-11-25T10:34:32.003+0000][40000ns][safepoint      ] Safepoint "Cleanup", Time since last: 5 ns, Reaching safepoint: 100 ns, Cleanup: 1 ns, At safepoint: 10 ns, Leaving safepoint: 1 ns, Total: 2000 ns
"""

    def test_episodes_overlap_gc_safepoints_deoptimisations_or_neither(self):
        # Wall clock = nanoTime + 1,000,000. Rows: first_seq, start_wall, records, max_ns, end_wall.
        wall = 1_000_000
        events = {"role": "writer", "anchor_wall_ns": wall, "anchor_nano": 0, "start_nano": 0,
                  "measured_from_nano": 10_000, "end_nano": 60_000, "episodes_dropped": 0,
                  "episodes": [[1, wall + 2_000, 1, 1_000, wall + 2_000],      # before the window
                               [2, wall + 21_000, 3, 1_500, wall + 22_000],    # deoptimisation at 20,000
                               [3, wall + 31_000, 1, 2_000, wall + 31_000],    # GC 25,000-30,000
                               [4, wall + 40_500, 1, 1_500, wall + 40_500],    # safepoint 38,000-40,000
                               [5, wall + 55_000, 1, 1_200, wall + 55_000]]}
        summary = attribute.summarize(events, self.LOG, slack=0)
        self.assertEqual(summary["counts"], {"gc": 1, "safepoint": 1, "deoptimisation": 1, "neither": 1})
        self.assertEqual(summary["episodes"], 4)
        self.assertEqual([d[1] for d in summary["deopts"]], ["T.main([Ljava/lang/String;)V"])
        self.assertEqual(summary["log_lines"], 4)
        self.assertEqual(summary["worst_ns"]["neither"], 1_200)
        # A record due just after an event still counts as delayed by it.
        self.assertEqual(attribute.summarize(events, self.LOG, slack=14_000)["counts"]["neither"], 0)
        # A backlog far longer than the events inside it is not theirs.
        events["episodes"] = [[2, wall + 59_000, 9, 50_000, wall + 59_000], [3, wall + 12_000, 9, 50_000_000, wall + 59_000]]
        self.assertEqual(attribute.summarize(events, self.LOG)["counts"],
                         {"gc": 0, "safepoint": 0, "deoptimisation": 1, "neither": 1})

    def test_dropped_log_messages_are_reported(self):
        self.assertEqual(attribute.dropped_lines("[x][12ns][] 17 messages dropped due to async logging\n"), 17)
        self.assertEqual(attribute.dropped_lines(self.LOG), 0)


if __name__ == "__main__":
    unittest.main()
