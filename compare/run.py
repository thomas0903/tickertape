#!/usr/bin/env python3
"""Sequential comparison matrix; stdout/stderr of build and children are retained.

A failed/missing tool is an error, never silently omitted or replaced with a zero.
Only TemporaryDirectory-owned run data is removed; results are created exclusively.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import shlex
import signal
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
TOOLS = ("tickertape", "chronicle", "aeron", "yaque", "queue-file", "plain-file")
VERSIONS = {"tickertape": "0.2.0", "chronicle": "2026.8", "aeron": "1.54.0",
            "yaque": "0.6.6", "queue-file": "2.0.0", "plain-file": "1 (suite)"}
GUARANTEES = {
    "tickertape": {"kind": "journal", "process_crash": "validated published prefix; only checkpoint promises durability",
                   "persistent_file": True, "arbitrary_replay": True, "independent_readers": True},
    "chronicle": {"kind": "journal", "process_crash": "completed documents in mapped files; no per-record fsync",
                  "persistent_file": True, "arbitrary_replay": True, "independent_readers": True},
    "aeron": {"kind": "IPC transport, no persistence", "process_crash": "no durable recovery",
              "persistent_file": False, "arbitrary_replay": False, "independent_readers": True},
    "yaque": {"kind": "SPSC consuming queue", "process_crash": "recovery API required after unhandled kill; not tested here",
              "persistent_file": True, "arbitrary_replay": False, "independent_readers": False},
    "queue-file": {"kind": "FIFO queue (non-consuming iteration in W3)", "process_crash": "atomic header commits; sync_writes=false; kill not tested here",
                   "persistent_file": True, "arbitrary_replay": False, "independent_readers": False},
    "plain-file": {"kind": "length-prefixed file", "process_crash": "completed writes remain in OS cache; incomplete tail not recovered",
                   "persistent_file": True, "arbitrary_replay": "sequential scan only", "independent_readers": True},
}


def execute(command, *, timeout=180, **kwargs):
    return subprocess.run([str(x) for x in command], check=True, timeout=timeout, **kwargs)


def capture(command):
    try:
        r = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           text=True, timeout=15, check=False)
        return r.stdout.rstrip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unavailable: {e}"


def environment(out, env, quick):
    commands = [["git", "rev-parse", "HEAD"], ["git", "status", "--short"],
                ["uname", "-a"], ["rustc", "--version"], ["cargo", "--version"],
                [str(Path(env["JAVA_HOME"]) / "bin/java"), "-version"], ["mvn", "-version"]]
    if platform.system() == "Darwin":
        mount = capture(["df", "-P", tempfile.gettempdir()]).splitlines()[-1].split()[-1]
        commands += [["sw_vers"], ["sysctl", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"],
                     ["diskutil", "info", mount], ["df", "-h", tempfile.gettempdir()]]
    else:
        commands += [["lscpu"], ["free", "-h"], ["df", "-T", tempfile.gettempdir()]]
    with (out / "environment.txt").open("w") as f:
        f.write(f"date_utc: {datetime.datetime.now(datetime.timezone.utc).isoformat()}\n")
        f.write(f"quick: {quick}\nTMPDIR: {tempfile.gettempdir()}\nJAVA_HOME: {env['JAVA_HOME']}\n")
        f.write("versions: " + json.dumps(VERSIONS) + "\n")
        f.write("command: " + shlex.join(sys.argv) + "\n")
        f.write("JVM flags (exact shared array):\n" + (HERE / "java-flags.sh").read_text() + "\n")
        for command in commands:
            f.write("\n$ " + shlex.join(command) + "\n" + capture(command) + "\n")
        if "orbstack" in platform.release().lower():
            f.write("environment: OrbStack VM (Linux on macOS)\n")
        f.write("affinity: none; power/load: record manually for publication\n")


def command(tool, role, data, control, count, payload, rate, warmup, reader, mode, quick):
    cmd = [str(HERE / tool / "run.sh"), role, "--dir", str(data), "--control", str(control),
           "--count", str(count), "--payload", str(payload), "--rate", str(rate),
           "--warmup", str(warmup), "--reader", reader, "--mode", mode]
    if tool in ("chronicle", "aeron"):
        cmd += ["--jit-warmup", "1000" if quick else "100000"]
    return cmd


def parse_result(path):
    lines = path.read_text().splitlines()
    if len(lines) != 1:
        raise RuntimeError(f"expected exactly one JSON line in {path}, got {len(lines)}")
    return json.loads(lines[0])


def run_one(tool, workload, payload, reader, rate, repeat, out, env, quick, commands):
    mode = {"W1": "append", "W2": "visible_latency", "W3": "warm_replay"}[workload]
    count = (1_000_000 if payload == 64 else 100_000)
    warmup = 100_000 if workload == "W2" else 0
    if workload == "W2":
        count = 1_100_000
    if quick:
        count = 1100 if workload == "W2" else 2000
        warmup = 100 if workload == "W2" else 0
    label = f"{tool}-{workload}-{payload}-{reader}-{rate}-{repeat}"
    print(label, file=sys.stderr, flush=True)
    with tempfile.TemporaryDirectory(prefix="tt-compare-") as tmp:
        base = Path(tmp)
        data, control = base / "data", base / "control"
        data.mkdir()
        control.mkdir()
        children = []
        handles = []
        outputs = []
        deadline = time.monotonic() + count / max(rate, 1) + 120

        def spawn(role, mode_override=None):
            cmd = command(tool, role, data, control, count, payload, rate, warmup,
                          reader, mode_override or mode, quick)
            commands.write(shlex.join(cmd) + "\n")
            commands.flush()
            output = base / f"{role}-{len(children)}.json"
            outputs.append(output)
            handle = output.open("w")
            handles.append(handle)
            proc = subprocess.Popen(cmd, stdout=handle, stderr=error_log, env=env, start_new_session=True)
            children.append(proc)
            return proc, output

        def wait(proc):
            status = proc.wait(timeout=max(0.1, deadline - time.monotonic()))
            if status:
                raise RuntimeError(f"{label}: child exited {status}; see children.log")

        def ready(role):
            while not (control / f"{role}.ready").exists():
                for proc in children:
                    if proc.poll() is not None:
                        raise RuntimeError(f"{label}: child exited before barrier; see children.log")
                if time.monotonic() > deadline:
                    raise TimeoutError(f"{label}: readiness timed out")
                time.sleep(0.002)

        with (out / "children.log").open("a") as error_log:
            error_log.write(f"\n{label}\n")
            error_log.flush()
            try:
                if tool == "tickertape" and workload == "W2":
                    cmd = [str(HERE / "target/release/tt-latency"), "run", "--dir", str(data),
                           "--rate", str(rate), "--count", str(count), "--warmup", str(warmup),
                           "--payload", str(payload), "--reader", reader, "--json"]
                    commands.write(shlex.join(cmd) + "\n")
                    result = json.loads(execute(cmd, stdout=subprocess.PIPE, stderr=error_log, env=env,
                                                timeout=count/rate+120, text=True).stdout)
                elif workload == "W2":
                    writer, writer_path = spawn("writer")
                    ready("writer")
                    read, read_path = spawn("reader")
                    ready("reader")
                    (control / "start.tmp").write_text(str(time.time_ns() + 50_000_000))
                    (control / "start.tmp").rename(control / "start")
                    wait(read)
                    wait(writer)
                    result = {"latency": parse_result(read_path), "writer": parse_result(writer_path),
                              "coordinated_omission": "intended timestamps; no skipped sends or rebasing"}
                else:
                    writer, writer_path = spawn("writer", "append")
                    wait(writer)
                    if workload == "W1":
                        result = {"append": parse_result(writer_path)}
                    else:
                        # Full untimed read warms the page cache. QueueIter and
                        # queue-file iteration leave the fixture unconsumed.
                        warm, _ = spawn("reader")
                        wait(warm)
                        read, read_path = spawn("reader")
                        wait(read)
                        result = {"replay": parse_result(read_path)}
            finally:
                for proc in children:
                    if proc.poll() is None:
                        os.killpg(proc.pid, signal.SIGTERM)
                for proc in children:
                    try:
                        proc.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
                for handle in handles:
                    handle.close()
                for output in outputs:
                    error_log.write(f"{output.name}: {output.read_text()}")
    guarantees = dict(GUARANTEES[tool])
    guarantees["zero_copy"] = tool == "chronicle" or tool == "aeron" or (tool == "tickertape" and reader == "mapped")
    result.update(tool=tool, tool_version=VERSIONS[tool], guarantees=guarantees,
                  mode=mode, workload=workload, payload=payload, count=count, warmup=warmup,
                  rate=rate if workload == "W2" else None, reader=reader, segment_size=134_217_728 if tool in ("tickertape", "yaque") else None,
                  repeat=repeat, quick=quick)
    validate_result(result)
    with (out / f"{tool}-{workload}.jsonl").open("a") as f:
        f.write(json.dumps(result, allow_nan=False, separators=(",", ":")) + "\n")


def validate_result(result):
    """Fail runs with silent truncation, invalid stats, or wrong measurement counts."""
    import math
    if result["workload"] == "W2":
        assert result["latency"]["count"] == result["count"] - result["warmup"]
        assert result["writer"]["count"] == result["count"]
        assert 0 <= result["writer"]["late_records"] <= result["count"]
        stats = result["latency"]
        values = [stats[key] for key in ("min_us", "p50_us", "p90_us", "p99_us", "p99_9_us", "p99_99_us", "max_us")]
        assert all(math.isfinite(v) and v >= 0 for v in values)
        assert values == sorted(values)
        assert math.isfinite(result["writer"]["achieved_records_per_s"]) and result["writer"]["achieved_records_per_s"] > 0
    else:
        stats = result["append" if result["workload"] == "W1" else "replay"]
        assert stats["count"] == result["count"]
        assert math.isfinite(stats["records_per_s"]) and stats["records_per_s"] > 0
        expected = stats["records_per_s"] * result["payload"] / 1e6
        assert math.isclose(stats["payload_mb_per_s"], expected, rel_tol=1e-9)


def main():
    suite_start = time.monotonic()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("outdir", type=Path)
    parser.add_argument("--quick", action="store_true", help="100 warmup + 1000 measured latency; 2000 throughput records")
    args = parser.parse_args()
    # Exclusive output creation prevents accidentally mixing runs with old results.
    out = args.outdir.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if any(out.iterdir()):
        parser.error("OUTDIR must be new or empty")
    env = dict(os.environ)
    jdk = env.get("JAVA_HOME")
    if not jdk and Path("/opt/homebrew/opt/openjdk@21").exists():
        jdk = "/opt/homebrew/opt/openjdk@21"
    if not jdk:
        parser.error("set JAVA_HOME to Java 21")
    env["JAVA_HOME"] = jdk
    env["PATH"] = str(Path(jdk) / "bin") + os.pathsep + env["PATH"]
    os.environ.update({"JAVA_HOME": jdk, "PATH": env["PATH"]})
    environment(out, env, args.quick)
    with (out / "build.log").open("w") as log:
        execute(["cargo", "build", "--locked", "--release", "--manifest-path", HERE / "Cargo.toml",
                 "--target-dir", HERE / "target"], stdout=log, stderr=subprocess.STDOUT, env=env)
        execute(["cargo", "build", "--locked", "--release", "-p", "tickertape-bench", "--bin", "tt-latency",
                 "--target-dir", HERE / "target"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, env=env)
        for tool in ("chronicle", "aeron"):
            execute(["mvn", "-B", "-q", "-f", HERE / tool / "pom.xml", "package", "dependency:build-classpath", "dependency:list",
                     "-Dmdep.outputFile=target/classpath.txt",
                     f"-DoutputFile={out / (tool + '-dependencies.txt')}"], stdout=log, stderr=subprocess.STDOUT, env=env)
    with (out / "commands.txt").open("w") as commands:
        for repeat in (1, 2):
            for payload in (64, 1024):
                for tool in TOOLS:
                    if tool != "aeron":
                        run_one(tool, "W1", payload, "default", 100_000, repeat, out, env, args.quick, commands)
                        for reader in (("safe", "mapped") if tool == "tickertape" else ("default",)):
                            run_one(tool, "W3", payload, reader, 100_000, repeat, out, env, args.quick, commands)
                    if tool != "queue-file":
                        for reader in (("safe", "mapped") if tool == "tickertape" else ("default",)):
                            for rate in (100_000, 10_000):
                                run_one(tool, "W2", payload, reader, rate, repeat, out, env, args.quick, commands)
    (out / "skipped.json").write_text(json.dumps({"aeron": {"W1": "Archive not implemented; IPC has no persistent append workload",
        "W3": "Archive not implemented; IPC cannot replay"}, "queue-file": {"W2": "upstream forbids concurrent process access"}}, indent=2) + "\n")
    elapsed = time.monotonic() - suite_start
    with (out / "environment.txt").open("a") as f:
        f.write(f"suite_elapsed_seconds: {elapsed:.3f}\n")
    print(f"done: {out} ({elapsed:.1f}s)")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, AssertionError, subprocess.SubprocessError) as error:
        print(f"compare: {error}", file=sys.stderr)
        sys.exit(1)
