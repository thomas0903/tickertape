"""Thread pinning: the choice of CPUs, the options passed on, keeping the CPUs free and checking the read-back."""
import argparse
import copy
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile
import time
import unittest

import affinity


def args(**overrides):
    values = dict(writer_cpu=None, reader_cpu=None, helper_cpus=None, no_pin=False)
    values.update(overrides)
    return argparse.Namespace(**values)


TREES = tempfile.TemporaryDirectory()


def tearDownModule():
    TREES.cleanup()


class FakeSys:
    """A sysfs tree: cores as lists of CPUs, and optionally the L3 groups."""
    def __init__(self, cores, l3=(), isolated="", online=None):
        root = Path(tempfile.mkdtemp(dir=TREES.name))
        cpus = sorted(c for core in cores for c in core)
        (root / "online").write_text(online or ",".join(map(str, cpus)) + "\n")
        (root / "isolated").write_text(isolated + "\n")
        for core in cores:
            for cpu in core:
                topology = root / f"cpu{cpu}/topology"
                topology.mkdir(parents=True)
                (topology / "thread_siblings_list").write_text(",".join(map(str, core)) + "\n")
                (topology / "physical_package_id").write_text("0\n")
                for group in l3:
                    if cpu in group:
                        cache = root / f"cpu{cpu}/cache/index3"
                        cache.mkdir(parents=True)
                        (cache / "level").write_text("3\n")
                        (cache / "shared_cpu_list").write_text(",".join(map(str, group)) + "\n")
        self.topology = affinity.Topology(root, allowed=set(cpus))

    def resolve(self, **overrides):
        return affinity.resolve(args(**overrides), system="Linux", topology=self.topology)


SMT = [[0, 4], [1, 5], [2, 6], [3, 7]]  # Four cores with two threads each, numbered as on most x86 machines.


class ChoiceTests(unittest.TestCase):
    def test_automatic_choice_avoids_cpu_zero_and_hyperthread_siblings(self):
        plan = FakeSys(SMT).resolve()
        self.assertEqual((plan["mode"], plan["writer_cpu"], plan["reader_cpu"]), ("pinned", 1, 2))
        self.assertEqual(plan["helper_cpus"], [0, 3, 4, 7])  # Not 5 and 6, the siblings of 1 and 2.
        self.assertEqual(plan["measured_cpus_sharing_a_physical_core"], [])
        # Siblings numbered side by side.
        plan = FakeSys([[0, 1], [2, 3], [4, 5]]).resolve()
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"], plan["helper_cpus"]), (2, 4, [0, 1]))

    def test_automatic_choice_keeps_writer_and_reader_on_one_l3_cache(self):
        cores = [[c] for c in range(6)]
        plan = FakeSys(cores, l3=([0, 1], [2, 3, 4, 5])).resolve()
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"]), (2, 3))
        # The reader goes next to a named writer.
        plan = FakeSys(cores, l3=([0, 1, 2], [3, 4, 5])).resolve(writer_cpu=4)
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"]), (4, 3))

    def test_automatic_choice_leaves_isolated_cpus_alone_but_the_caller_can_name_them(self):
        fake = FakeSys([[c] for c in range(6)], isolated="1,2")
        plan = fake.resolve()
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"], plan["helper_cpus"]), (3, 4, [0, 5]))
        plan = fake.resolve(writer_cpu=1, reader_cpu=2)
        self.assertEqual((plan["helper_cpus"], plan["isolated_cpus"]), ([0, 3, 4, 5], [1, 2]))
        self.assertEqual(plan["named_by_caller"], ["writer_cpu", "reader_cpu"])

    def test_named_cpus_are_used_exactly(self):
        plan = FakeSys(SMT).resolve(writer_cpu=3, reader_cpu=2, helper_cpus="0-1,5")
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"], plan["helper_cpus"]), (3, 2, [0, 1, 5]))
        self.assertEqual(plan["measured_cpus_sharing_a_physical_core"], [])
        # Allowed, but recorded: the writer's hyperthread sibling runs helper threads.
        plan = FakeSys(SMT).resolve(writer_cpu=3, reader_cpu=2, helper_cpus="0,7")
        self.assertEqual(plan["measured_cpus_sharing_a_physical_core"], [3])
        # Named helper CPUs' cores are not chosen for the measured threads.
        plan = FakeSys(SMT).resolve(helper_cpus="1")
        self.assertEqual((plan["writer_cpu"], plan["reader_cpu"]), (2, 3))

    def test_impossible_requests_are_errors(self):
        for cores, overrides in [
                ([[0, 1], [2, 3]], {}),                                   # two cores: no core for the helpers
                ([[0]], {}),
                (SMT, dict(writer_cpu=1, reader_cpu=1)),
                (SMT, dict(writer_cpu=1, helper_cpus="0-2")),
                (SMT, dict(writer_cpu=64)),
                (SMT, dict(helper_cpus="")),
                (SMT, dict(helper_cpus="3-1")),
                (SMT, dict(no_pin=True, writer_cpu=1))]:
            with self.subTest(cores=cores, overrides=overrides), self.assertRaises(ValueError):
                FakeSys(cores).resolve(**overrides)
        # Two cores work if the caller says where everything goes.
        self.assertEqual(FakeSys([[0, 1], [2, 3]]).resolve(writer_cpu=0, reader_cpu=2, helper_cpus="1,3")
                         ["measured_cpus_sharing_a_physical_core"], [0, 2])

    def test_unpinned_and_unsupported_say_so(self):
        self.assertEqual(affinity.resolve(args(no_pin=True), system="Linux")["summary"], "affinity: none (--no-pin)")
        self.assertEqual(affinity.resolve(args(), system="Darwin"),
                         {"mode": "unsupported", "summary": "affinity: unsupported on macOS"})
        with self.assertRaises(ValueError):
            affinity.resolve(args(writer_cpu=1), system="Darwin")

    def test_the_options_reach_the_adapters(self):
        plan = FakeSys(SMT).resolve()
        self.assertEqual(affinity.adapter_options(plan, "yaque", "reader"), ["--cpu", "2", "--helper-cpus", "0,3,4,7"])
        self.assertEqual(affinity.adapter_options(plan, "chronicle", "writer"), ["--cpu", "1"])
        self.assertEqual(affinity.latency_options(plan),
                         ["--writer-cpu", "1", "--reader-cpu", "2", "--helper-cpus", "0,3,4,7"])
        for other in (None, {"mode": "none"}, {"mode": "unsupported"}):
            self.assertEqual(affinity.adapter_options(other, "yaque", "reader"), [])
            self.assertEqual(affinity.latency_options(other), [])


class RecordTests(unittest.TestCase):
    def setUp(self):
        self.plan = FakeSys(SMT).resolve()
        helpers = [{"name": "helper", "cpus_allowed": [0, 3, 4, 7]}]
        self.writer = {"count": 1, "affinity": {"cpus_allowed": [1], "last_cpu": 1, "other_threads": helpers}}
        self.reader = {"count": 1, "affinity": {"cpus_allowed": [2], "last_cpu": 2, "other_threads": []}}

    def test_the_read_back_affinity_moves_from_the_child_results_into_the_section(self):
        section = affinity.record(self.plan, "yaque", self.writer, self.reader)
        self.assertEqual((self.writer, self.reader), ({"count": 1}, {"count": 1}))
        self.assertEqual((section["writer"]["cpus_allowed"], section["reader"]["last_cpu"]), ([1], 2))
        self.assertIsNone(section["moved_threads"])  # Nothing watched these processes.

    def test_aeron_without_enough_helper_cpus_is_marked(self):
        few = FakeSys([[0], [1], [2], [3]]).resolve()
        self.assertEqual(few["helper_cpus"], [0, 3])
        self.assertIn("2 helper CPUs", affinity.warnings(few, ["yaque", "aeron"])[0])
        self.assertEqual(affinity.warnings(few, ["yaque"]), [])
        self.assertEqual(affinity.warnings(self.plan, ["aeron"]), [])
        section = affinity.record(dict(self.plan, helper_cpus=[0, 3]), "aeron", self.writer, self.reader)
        self.assertEqual(len(section["warnings"]), 1)

    def test_a_keeper_that_failed_or_saw_nothing_fails_the_run(self):
        class Failing(affinity.Keeper):
            def sweep(self, pid, role):
                raise PermissionError("refused")

        with tempfile.TemporaryDirectory() as control:
            with Failing(self.plan, control) as keeper:
                keeper.watch(os.getpid(), "writer")
                keeper.thread.join(30)
                with self.assertRaisesRegex(RuntimeError, "refused"):
                    affinity.record(self.plan, "yaque", copy.deepcopy(self.writer), None, keeper)
            with affinity.Keeper(self.plan, control) as keeper:  # No writer.tid: the thread it has to spare is unknown.
                keeper.watch(os.getpid(), "writer")
                with self.assertRaisesRegex(RuntimeError, "never saw"):
                    affinity.record(self.plan, "yaque", copy.deepcopy(self.writer), None, keeper)
            # It ended too early.
            with self.assertRaisesRegex(RuntimeError, "failed"):
                affinity.record(self.plan, "yaque", copy.deepcopy(self.writer), None, keeper)

    def test_a_thread_that_was_elsewhere_fails_the_run(self):
        for change in [lambda w, r: w.update(cpus_allowed=[0, 1]), lambda w, r: w.update(last_cpu=3),
                       lambda w, r: r.update(cpus_allowed=[1]),
                       lambda w, r: r.update(unsupported="no /proc", cpus_allowed=None),
                       lambda w, r: w["other_threads"].append({"name": "x", "cpus_allowed": [1]}),
                       # The other process's measured CPU counts too.
                       lambda w, r: w["other_threads"].append({"name": "x", "cpus_allowed": [0, 2]})]:
            writer, reader = copy.deepcopy(self.writer), copy.deepcopy(self.reader)
            change(writer["affinity"], reader["affinity"])
            with self.assertRaises(AssertionError):
                affinity.record(self.plan, "yaque", writer, reader)
            # The same read-back is only recorded, not judged, in an unpinned run.
            writer, reader = copy.deepcopy(self.writer), copy.deepcopy(self.reader)
            change(writer["affinity"], reader["affinity"])
            self.assertEqual(affinity.record({"mode": "none"}, "yaque", writer, reader)["mode"], "none")


# Starts a thread from its pinned thread, and another one after announcing the end of its measurement.
CHILD = """
import os, sys, threading, time
control, cpu = sys.argv[1], int(sys.argv[2])
def announce(event):
    open(control + "/writer.tmp", "w").write(str(threading.get_native_id()))
    os.rename(control + "/writer.tmp", control + "/writer." + event)
def start():
    threading.Thread(target=time.sleep, args=(60,), daemon=True).start()
announce("tid")
os.sched_setaffinity(0, {cpu})
start()
sys.stdin.readline()
announce("measured")
start()
print("late", flush=True)
time.sleep(60)
"""


@unittest.skipUnless(platform.system() == "Linux", "Linux only")
class KeeperTests(unittest.TestCase):
    def test_threads_are_moved_to_the_helper_cpus_until_the_measurement_is_over(self):
        cpus = sorted(os.sched_getaffinity(0))
        if len(cpus) < 3:
            self.skipTest("needs three CPUs")
        plan = {"mode": "pinned", "writer_cpu": cpus[0], "reader_cpu": cpus[1], "helper_cpus": cpus[2:]}
        affinity.check(plan, ["yaque"])
        self.assertEqual(sorted(os.sched_getaffinity(0)), cpus)  # check() leaves the runner where it was.
        with tempfile.TemporaryDirectory() as control, affinity.Keeper(plan, control) as keeper, \
                subprocess.Popen([sys.executable, "-c", CHILD, control, str(cpus[0])], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE, text=True) as child:
            def masks():
                return {tid: os.sched_getaffinity(int(tid)) for tid in os.listdir(f"/proc/{child.pid}/task")}

            def wait(condition):
                deadline = time.monotonic() + 30
                while not condition() and time.monotonic() < deadline:
                    time.sleep(0.01)
            try:
                keeper.watch(child.pid, "writer")
                wait(lambda: keeper.moved)
                self.assertEqual([(key[0], n) for key, n in keeper.moved.items()], [("writer", 1)])
                before = masks()
                self.assertEqual(before.pop(str(child.pid)), {cpus[0]})  # The measured thread stays.
                self.assertEqual(list(before.values()), [set(cpus[2:])])
                child.stdin.write("\n")
                child.stdin.flush()
                self.assertEqual(child.stdout.readline(), "late\n")
                wait(lambda: not keeper.processes)
                after = masks()
                self.assertEqual(sorted(map(sorted, after.values())), sorted([[cpus[0]], [cpus[0]], cpus[2:]]))
                self.assertEqual(sum(keeper.moved.values()), 1)  # Not counted as a thread that competed.
                keeper.check()
            finally:
                child.kill()


if __name__ == "__main__":
    unittest.main()
