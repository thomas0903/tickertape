"""--tools in the short mode: the selection, its checks, the recorded selection, and the same output as a full run
for the selected tools. Stand-in adapters (standin.py): no build, no JDK, nothing measured."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

import run

HERE = Path(__file__).resolve().parent


def short_run(out, *options):
    pids = out.with_name(out.name + "-pids")
    pids.mkdir()
    env = dict(os.environ, STANDIN_PIDS=str(pids), TMPDIR=str(out.parent))
    return subprocess.run([sys.executable, str(HERE / "standin.py"), "harness", str(out), "--quick", "--no-pin",
                           *options], env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=300)


def results(out):
    """{file name: [result line without selected_tools]}, with this run's directories replaced."""
    files = {}
    for path in sorted(out.glob("*.jsonl")):
        lines = []
        for line in path.read_text().replace(str(out), "OUT").splitlines():
            result = json.loads(line)
            result.pop("selected_tools")
            lines.append(result)
        files[path.name] = lines
    return files


def commands(out, tools):
    """The adapter commands of `tools`, in order, without the scratch directories' random names."""
    lines = (out / "commands.txt").read_text().replace(str(out), "OUT").splitlines()
    lines = [re.sub(r"tt-compare-[^/]+", "SCRATCH", line) for line in lines]
    return [line for line in lines if any(f" adapter {tool} " in line or (tool == "tickertape" and " latency " in line)
                                          for tool in tools)]


class SelectionTests(unittest.TestCase):
    def test_syntax_and_checks_are_the_sustained_mode_s(self):
        self.assertEqual(run.select_tools(None), list(run.TOOLS))
        self.assertEqual(run.select_tools("yaque"), ["yaque"])
        self.assertEqual(run.select_tools("plain-file,chronicle,tickertape"), ["tickertape", "chronicle", "plain-file"])
        for bad in ("nosuch", "yaque,nosuch", "yaque,yaque", "Chronicle"):
            with self.subTest(tools=bad), self.assertRaisesRegex(ValueError, "invalid or repeated tool"):
                run.select_tools(bad)

    def test_an_unknown_tool_is_refused_before_anything_is_created(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp) / "out"
            done = short_run(out, "--tools", "yaque,nosuch")
            self.assertEqual(done.returncode, 2, done.stdout)
            self.assertIn("invalid or repeated tool: ['nosuch']", done.stdout)
            self.assertFalse(out.exists())


class OutputTests(unittest.TestCase):
    """One full run of every tool and two partial ones, compared."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = Path(tempfile.mkdtemp(prefix="tt-tools-"))
        cls.runs = {}
        for name, options in (("full", []), ("yaque", ["--tools", "yaque"]),
                              ("jvm", ["--tools", "aeron,chronicle"])):
            done = short_run(cls.tmp / name, *options)
            if done.returncode:
                raise AssertionError(f"{name}: {done.stdout}")
            cls.runs[name] = cls.tmp / name

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def test_a_selected_tool_gets_the_files_and_results_of_a_full_run(self):
        full = results(self.runs["full"])
        self.assertEqual((len(full), sum(map(len, full.values()))), (20, 112))  # compare/README.md
        for name, tools in (("yaque", ["yaque"]), ("jvm", ["chronicle", "aeron"])):
            with self.subTest(run=name):
                partial = results(self.runs[name])
                self.assertEqual(partial, {f: lines for f, lines in full.items() if f.split("-W")[0] in tools})

    def test_the_tools_keep_the_order_of_a_full_run(self):
        tools = ["chronicle", "aeron"]  # given as aeron,chronicle
        self.assertEqual(commands(self.runs["jvm"], tools), commands(self.runs["full"], tools))
        first = [line.split(" adapter ")[1].split()[0] for line in commands(self.runs["jvm"], tools)]
        self.assertEqual(first[:2], ["chronicle", "chronicle"])

    def test_every_result_and_the_environment_record_the_selection(self):
        for name, selected in (("full", list(run.TOOLS)), ("yaque", ["yaque"]), ("jvm", ["chronicle", "aeron"])):
            with self.subTest(run=name):
                out = self.runs[name]
                lines = [json.loads(line) for path in out.glob("*.jsonl") for line in path.read_text().splitlines()]
                self.assertTrue(lines)
                self.assertEqual({tuple(r["selected_tools"]) for r in lines}, {tuple(selected)})
                self.assertIn(f"selected_tools: {json.dumps(selected)}\n", (out / "environment.txt").read_text())

    def test_skipped_cases_are_those_of_the_selected_tools(self):
        full = json.loads((self.runs["full"] / "skipped.json").read_text())
        self.assertEqual(sorted(full), ["aeron", "queue-file"])
        self.assertEqual(json.loads((self.runs["yaque"] / "skipped.json").read_text()), {})
        self.assertEqual(json.loads((self.runs["jvm"] / "skipped.json").read_text()), {"aeron": full["aeron"]})


if __name__ == "__main__":
    unittest.main()
