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


def child(java, records=1_200_000, **tool):
    return {"warmup": {"mode": "long" if java["long_warmup"] else "short", "records": records}, "tool": tool,
            "jvm_arguments": ["-XX:+UseParallelGC", *javaopts.jvm_flags(java, "x.log")]}


def chronicle_children(java, block=1 << 30, micro_touch=True, records=1_200_000):
    return {"writer": {"java": child(java, records, block_size=block, micro_touch=micro_touch)},
            "reader": {"java": child(java, records, block_size=block)}}


class SettingsTests(unittest.TestCase):
    def test_new_behaviour_is_the_default_and_each_part_switches_off_alone(self):
        self.assertEqual(settings(), javaopts.DEFAULTS)
        self.assertEqual(javaopts.DEFAULTS["chronicle_block_size"], 1 << 30)
        for argv, key, value in [(["--no-long-warmup"], "long_warmup", False),
                                 (["--no-micro-touch"], "micro_touch", False),
                                 (["--chronicle-block-size", "default"], "chronicle_block_size", "default"),
                                 (["--aeron-sparse-terms"], "aeron_sparse_terms", True),
                                 (["--no-aeron-pre-touch"], "aeron_pre_touch", False),
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
                         ["--jit-warmup", "1200000", "--warmup-mode", "long", "--sparse-terms", "off",
                          "--pre-touch", "on"])
        old_aeron = settings("--aeron-sparse-terms", "--no-aeron-pre-touch", "--no-jvm-logs")
        self.assertEqual(javaopts.adapter_arguments("aeron", old_aeron, False, True, None)[-4:],
                         ["--sparse-terms", "on", "--pre-touch", "off"])

    def test_micro_touch_alone_is_one_switch_set(self):
        # The cloud rerun's row between all-off and all-on.
        java = settings("--no-long-warmup", "--chronicle-block-size", "default")
        args = javaopts.adapter_arguments("chronicle", java, False, False, None)
        self.assertEqual((args[args.index("--micro-touch") + 1], args[args.index("--block-size") + 1],
                          args[args.index("--warmup-mode") + 1]), ("on", "default", "short"))

    def test_child_command_carries_the_jvm_flags_and_log_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            cmd = javaopts.wrap(["run.sh", "writer", "--count", "1"], "aeron", settings("--java-experiment", "epsilon"),
                                False, False, out, "label-writer")
            self.assertEqual(cmd[0], "run.sh")
            self.assertLess(cmd.index("--jvm=-XX:-UseParallelGC"), cmd.index("--jvm=-XX:+UseEpsilonGC"))
            log = next(c for c in cmd if c.startswith("--jvm=-Xlog:gc*,safepoint,handshake*,jit+compilation"))
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
        children = chronicle_children(java)
        writer, reader = {"count": 1, **children["writer"]}, {"p50_us": 1, **children["reader"]}
        block = javaopts.collect("chronicle", java, {"writer": writer, "reader": reader}, False, False, True)
        self.assertEqual((writer, reader), ({"count": 1}, {"p50_us": 1}))
        self.assertEqual(block["settings"], java)
        self.assertTrue(block["writer"]["tool"]["micro_touch"])
        self.assertIsNone(javaopts.collect("tickertape", java, {"writer": {"count": 1}}, False, False, True))

    def test_rejects_a_child_that_ran_with_other_settings(self):
        def fails(tool, java, children):
            with self.assertRaises(RuntimeError):
                javaopts.collect(tool, java, copy.deepcopy(children), False, False, True)

        java = settings()
        good = chronicle_children(java)
        javaopts.collect("chronicle", java, copy.deepcopy(good), False, False, True)
        for role, path, value in [("writer", ("warmup", "mode"), "short"), ("reader", ("warmup", "mode"), "short"),
                                  ("writer", ("warmup", "records"), 100_000),
                                  ("writer", ("tool", "micro_touch"), False),
                                  ("writer", ("tool", "block_size"), 64 << 20),
                                  ("reader", ("tool", "block_size"), 64 << 20)]:
            with self.subTest(role=role, path=path):
                bad = copy.deepcopy(good)
                bad[role]["java"][path[0]][path[1]] = value
                fails("chronicle", java, bad)
        # A default block size is Chronicle's, not whatever the child has.
        off = settings("--chronicle-block-size", "default")
        javaopts.collect("chronicle", off, chronicle_children(off, 64 << 20), False, False, True)
        fails("chronicle", off, chronicle_children(off, 1 << 30))
        # JVM flags, logs and experiments, in both directions.
        fails("chronicle", settings("--no-jvm-logs"), good)
        fails("chronicle", settings("--java-experiment", "epsilon"), good)
        fails("chronicle", java, chronicle_children(settings("--java-experiment", "always-pre-touch")))
        fails("chronicle", java, chronicle_children(settings("--no-jvm-logs")))
        # Short warm-up: none in a live run, 100,000 records otherwise.
        short = settings("--no-long-warmup")
        javaopts.collect("chronicle", short, chronicle_children(short, records=0), False, False, True)
        javaopts.collect("chronicle", short, chronicle_children(short, records=100_000), False, False, False)
        fails("chronicle", short, chronicle_children(short, records=100_000))

    def test_rejects_an_aeron_child_without_pre_touched_terms(self):
        java = settings()

        def children(sparse, pre_touch):
            return {"writer": {"java": child(java, sparse_terms=sparse, pre_touch=pre_touch)},
                    "reader": {"java": child(java, pre_touch=pre_touch)}}

        javaopts.collect("aeron", java, children(False, True), False, False, True)
        for bad in (children(True, True), children(False, False)):
            with self.assertRaises(RuntimeError):
                javaopts.collect("aeron", java, bad, False, False, True)


class AttributionTests(unittest.TestCase):
    """Logs as the JVM writes them; times are System.nanoTime() values, wall clock = nanoTime + WALL."""
    WALL = 1_000_000_000_000

    @staticmethod
    def line(time, tags, message):
        return f"[2026-11-25T10:34:31.983+0000][{time}ns][{tags:<15}] {message}\n"

    def safepoint(self, end, total, name="Cleanup"):
        return self.line(end, "safepoint", f'Safepoint "{name}", Time since last: 5 ns, Reaching safepoint: 100 ns, '
                         f"Cleanup: 1 ns, At safepoint: 10 ns, Leaving safepoint: 1 ns, Total: {total} ns")

    def deopt(self, time):
        return self.line(time, "deoptimization", "cid=  83 osr level=4 T.main([Ljava/lang/String;)V trap_bci=21 osr_bci=18 "
                         "unstable_if reinterpret pc=0x1 relative_pc=0x1")

    def summary(self, log, *episodes, slew=0, **options):
        """episodes: (due, delay of the worst record) or (first time, worst record's time, last time, worst delay)."""
        rows = []
        for episode in episodes:
            first, worst, last, delay = episode if len(episode) == 4 else (episode[0] + episode[1],) * 3 + (episode[1],)
            rows.append([0, self.WALL + first, 1, delay, self.WALL + last, self.WALL + worst])
        events = {"role": "writer", "anchors": [[self.WALL, 0], [self.WALL + 10**10 + slew, 10**10]], "start_nano": 0,
                  "measured_from_nano": 10**8, "end_nano": 10**10, "episodes_dropped": 0, "episodes": rows}
        return attribute.summarize(events, log, **options)

    def test_each_kind_of_event_explains_the_delay_it_fills(self):
        log = (self.deopt(50_000_000) + self.deopt(200_000_000)
               + self.safepoint(300_000_000, 1_000_000, "ParallelGCFailedAllocation")
               + self.safepoint(400_000_000, 1_000_000)
               + self.line(500_000_000, "handshake", 'Handshake "Deoptimize", Targeted threads: 10, Executed by requesting '
                           "thread: 10, Total completion time: 1000000 ns"))
        summary = self.summary(log, (50_000_000, 1_000), (200_000_000, 20_000), (299_100_000, 1_000_000),
                               (399_200_000, 1_000_000), (499_300_000, 1_000_000), (900_000_000, 1_000_000))
        self.assertEqual(summary["counts"], {"gc": 1, "safepoint": 1, "handshake": 1, "deoptimisation": 1, "neither": 1})
        self.assertEqual(summary["episodes"], 5)  # The first one is before the window.
        self.assertEqual(summary["worst_ns"]["neither"], 1_000_000)
        self.assertEqual(len(summary["deopts"]), 1)
        self.assertFalse(summary["clock_warning"])

    def test_safepoints_elsewhere_in_a_long_episode_do_not_explain_its_worst_delay(self):
        # A 1 s episode, worst delay 1 ms at 1.0 s; four 150 us safepoints 200 ms apart.
        log = "".join(self.safepoint(1_000_000_000 + n * 200_000_000 + 100_000_000, 150_000) for n in range(4))
        summary = self.summary(log, (1_000_000_000, 1_900_000_000, 2_000_000_000, 1_000_000))
        self.assertEqual(summary["counts"]["neither"], 1)
        # A safepoint that only touches the window's edge counts with the part inside: 100 us of a 1 ms delay.
        edge = self.safepoint(1_899_200_000, 5_000_000)
        self.assertEqual(self.summary(edge, (1_000_000_000, 1_900_000_000, 2_000_000_000, 1_000_000),
                                      slack=0)["counts"]["neither"], 1)

    def test_a_deoptimisation_elsewhere_in_a_long_episode_does_not_explain_its_worst_delay(self):
        # The worst delay is at 1.1 s, the deoptimisation at 1.9 s of an episode until 2.0 s.
        summary = self.summary(self.deopt(1_900_000_000), (1_000_000_000, 1_100_000_000, 2_000_000_000, 1_000_000))
        self.assertEqual(summary["counts"], {"gc": 0, "safepoint": 0, "handshake": 0, "deoptimisation": 0, "neither": 1})
        # The same inside the window counts, but not for a delay longer than a deoptimisation costs.
        inside = (1_000_000_000, 1_900_200_000, 2_000_000_000, 1_000_000)
        self.assertEqual(self.summary(self.deopt(1_900_000_000), inside)["counts"]["deoptimisation"], 1)
        long = (1_000_000_000, 1_900_200_000, 2_000_000_000, 50_000_000)
        self.assertEqual(self.summary(self.deopt(1_900_000_000), long)["counts"]["neither"], 1)

    def test_a_record_due_just_after_an_event_counts_as_delayed_by_it(self):
        episode = (200_010_000, 30_000)
        self.assertEqual(self.summary(self.deopt(200_000_000), episode)["counts"]["deoptimisation"], 1)
        self.assertEqual(self.summary(self.deopt(200_000_000), episode, slack=0)["counts"]["neither"], 1)

    def test_wall_clock_slew_is_interpolated_and_warned_about(self):
        # The wall clock gained 2 ms over 10 s, 1 ms by 5 s: the episode's wall time is 1 ms ahead of its nanoTime.
        at = 5 * 10**9
        log = self.safepoint(at, 100_000)
        summary = self.summary(log, (at + 1_000_000 - 100_000, 100_000), slew=2_000_000, slack=1_000)
        self.assertEqual(summary["counts"]["safepoint"], 1)
        self.assertTrue(summary["clock_warning"])
        self.assertFalse(self.summary(log, (at, 1_000), slew=10_000)["clock_warning"])

    def test_dropped_log_messages_are_reported(self):
        self.assertEqual(attribute.dropped_lines("[x][12ns][] 17 messages dropped due to async logging\n"), 17)
        self.assertEqual(attribute.dropped_lines(self.deopt(1)), 0)


if __name__ == "__main__":
    unittest.main()
