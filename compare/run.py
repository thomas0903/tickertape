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
import subprocess
import sys
import tempfile
import time

import affinity
import javaopts
import reaper

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
TOOLS = ("tickertape", "chronicle", "aeron", "yaque", "queue-file", "plain-file")
def workspace_version():
    """tickertape's version, from the workspace Cargo.toml (the adapter builds the library in this checkout)."""
    for line in (Path(__file__).resolve().parent.parent / "Cargo.toml").read_text().splitlines():
        if line.startswith("version = "):
            return line.split('"')[1]
    raise RuntimeError("no workspace version in Cargo.toml")


VERSIONS = {"tickertape": workspace_version(), "chronicle": "2026.8", "aeron": "1.54.0",
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


# Settings of the JVM tools in this session (see javaopts.py); main() replaces them with the command line's.
JAVA = dict(javaopts.DEFAULTS)


def execute(command, *, timeout=180, **kwargs):
    return subprocess.run([str(x) for x in command], check=True, timeout=timeout, **kwargs)


def capture(command):
    try:
        r = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           text=True, timeout=15, check=False)
        return r.stdout.rstrip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return f"unavailable: {e}"


def environment(out, env, quick, name="environment.txt", pinning=None, tools=None):
    commands = [["git", "rev-parse", "HEAD"], ["git", "status", "--short"],
                ["uname", "-a"], ["rustc", "--version"], ["cargo", "--version"],
                [str(Path(env["JAVA_HOME"]) / "bin/java"), "-version"], ["mvn", "-version"]]
    if platform.system() == "Darwin":
        mount = capture(["df", "-P", tempfile.gettempdir()]).splitlines()[-1].split()[-1]
        commands += [["sw_vers"], ["sysctl", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"],
                     ["diskutil", "info", mount], ["df", "-h", tempfile.gettempdir()]]
    else:
        commands += [["lscpu"], ["free", "-h"], ["df", "-T", tempfile.gettempdir()],
                     ["findmnt", "-T", tempfile.gettempdir()], ["lsblk", "-o", "NAME,MODEL,SIZE,ROTA,TYPE,MOUNTPOINT"]]
    with (out / name).open("w") as f:
        f.write(f"date_utc: {datetime.datetime.now(datetime.timezone.utc).isoformat()}\n")
        f.write(f"quick: {quick}\nTMPDIR: {tempfile.gettempdir()}\nJAVA_HOME: {env['JAVA_HOME']}\n")
        f.write("versions: " + json.dumps(VERSIONS) + "\n")
        if tools is not None:
            f.write("selected_tools: " + json.dumps(list(tools)) + "\n")
        f.write("JVM tool settings: " + json.dumps(JAVA) + "\n")
        f.write("command: " + shlex.join(sys.argv) + "\n")
        f.write("JVM flags (exact shared array):\n" + (HERE / "java-flags.sh").read_text() + "\n")
        for command in commands:
            f.write("\n$ " + shlex.join(command) + "\n" + capture(command) + "\n")
        if "orbstack" in platform.release().lower():
            f.write("environment: OrbStack VM (Linux on macOS)\n")
        f.write((pinning or {"summary": "affinity: none"})["summary"] + "; power/load: record manually for publication\n")


def command(tool, role, data, control, count, payload, rate, warmup, reader, mode, quick, pinning=None, out=None,
            name=None):
    cmd = [str(HERE / tool / "run.sh"), role, "--dir", str(data), "--control", str(control),
           "--count", str(count), "--payload", str(payload), "--rate", str(rate),
           "--warmup", str(warmup), "--reader", reader, "--mode", mode]
    if tool in javaopts.JAVA_TOOLS:
        cmd = javaopts.wrap(cmd, tool, JAVA, quick, False, out, name)
    return cmd + affinity.adapter_options(pinning, tool, role)


def latency_command(data, rate, count, warmup, payload, reader, pinning):
    """tt-latency, which starts tickertape's writer and reader processes itself (W2 of tickertape)."""
    return [str(HERE / "target/release/tt-latency"), "run", "--dir", str(data), "--rate", str(rate), "--count",
            str(count), "--warmup", str(warmup), "--payload", str(payload), "--reader", reader, "--json",
            *affinity.latency_options(pinning)]


def select_tools(text):
    """The tools of a --tools list in the matrix's own order (the order of tools in a run is fixed), or ValueError.
    The same syntax and checks as the sustained mode's --tools."""
    tools = text.split(",") if text else list(TOOLS)
    bad = [t for t in tools if t not in TOOLS]
    if bad or len(set(tools)) != len(tools):
        raise ValueError(f"invalid or repeated tool: {bad or tools}")
    return [t for t in TOOLS if t in tools]


def parse_result(path):
    lines = path.read_text().splitlines()
    if len(lines) != 1:
        raise RuntimeError(f"expected exactly one JSON line in {path}, got {len(lines)}")
    return json.loads(lines[0])


TIMEOUT_SLACK_S = 120  # A measurement's time limit: its records at the intended rate plus this (JVM: plus warm-up).
CLOCK_STEP = "wall clock moved backwards"
CLOCK_RETRIES = 3


def run_one(tool, workload, payload, reader, rate, repeat, out, env, quick, commands, pinning=None, selected=TOOLS):
    """Runs one measurement. A latency run that saw the wall clock step backwards (an NTP adjustment; latency is
    measured against intended wall-clock send times) is invalid: it is discarded and measured again, at most
    CLOCK_RETRIES times, and each discarded attempt is recorded in retries.jsonl. Any other failure stops the suite."""
    log = out / "children.log"
    for attempt in range(1, CLOCK_RETRIES + 2):
        start = log.stat().st_size if log.exists() else 0
        try:
            try:
                return run_attempt(tool, workload, payload, reader, rate, repeat, out, env, quick, commands, pinning,
                                   selected)
            finally:
                # The measurement's scratch directory is gone by now; wait until the kernel has written back what it
                # wrote, so the next measurement (often another tool) doesn't pay for it.
                os.sync()
        except RuntimeError as error:
            with log.open("rb") as f:
                f.seek(start)
                new = f.read().decode("utf-8", "replace")
            if CLOCK_STEP not in new or attempt > CLOCK_RETRIES:
                raise
            label = f"{tool}-{workload}-{payload}-{reader}-{rate}-{repeat}"
            with (out / "retries.jsonl").open("a") as f:
                f.write(json.dumps({"label": label, "attempt": attempt, "reason": CLOCK_STEP,
                                    "error": str(error)}) + "\n")
            print(f"{label}: discarded attempt {attempt} ({CLOCK_STEP}); retrying", file=sys.stderr, flush=True)


def run_attempt(tool, workload, payload, reader, rate, repeat, out, env, quick, commands, pinning=None,
                selected=TOOLS):
    pinning = pinning or {"mode": "none", "summary": "affinity: none"}
    mode = {"W1-cold": "append_cold", "W1-steady": "append_steady", "W2": "visible_latency", "W3": "warm_replay"}[workload]
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
        group = reaper.Group()
        children = group.procs
        handles = []
        outputs = []
        java = None
        deadline = time.monotonic() + count / max(rate, 1) + TIMEOUT_SLACK_S
        if tool in javaopts.JAVA_TOOLS:
            deadline += javaopts.WARMUP_ALLOWANCE_S

        def spawn(role, mode_override=None):
            cmd = command(tool, role, data, control, count, payload, rate, warmup,
                          reader, mode_override or mode, quick, pinning, out, f"{label}-{role}-{len(children)}")
            commands.write(shlex.join(cmd) + "\n")
            commands.flush()
            output = base / f"{role}-{len(children)}.json"
            outputs.append(output)
            handle = output.open("w")
            handles.append(handle)
            proc = group.popen(cmd, stdout=handle, stderr=error_log, env=env)
            keeper.watch(proc.pid, role)
            return proc, output

        def wait(proc):
            status = group.wait(proc, timeout=max(0.1, deadline - time.monotonic()))
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

        with (out / "children.log").open("a") as error_log, affinity.Keeper(pinning, control) as keeper:
            error_log.write(f"\n{label}\n")
            error_log.flush()
            try:
                if tool == "tickertape" and workload == "W2":
                    cmd = latency_command(data, rate, count, warmup, payload, reader, pinning)
                    commands.write(shlex.join(cmd) + "\n")
                    # A process group of its own like the other children: its processes end with it.
                    latency_path = base / "latency.json"
                    with latency_path.open("w") as handle:
                        latency = group.popen(cmd, stdout=handle, stderr=error_log, env=env)
                    status = group.wait(latency, timeout=count/rate+TIMEOUT_SLACK_S)
                    if status:
                        raise subprocess.CalledProcessError(status, cmd)
                    result = json.loads(latency_path.read_text())
                    # Not watched by the keeper: tt-latency starts its own processes (moved_threads is null).
                    result["affinity"] = affinity.record(pinning, tool, *[{"affinity": result["affinity"][role]}
                                                                          for role in ("writer", "reader")])
                elif workload == "W2":
                    writer, writer_path = spawn("writer")
                    if not javaopts.warms_up_before_open(tool, JAVA):
                        ready("writer")
                    read, read_path = spawn("reader")
                    ready("writer")
                    ready("reader")
                    (control / "start.tmp").write_text(str(time.time_ns() + 50_000_000))
                    (control / "start.tmp").rename(control / "start")
                    wait(read)
                    wait(writer)
                    sent, received = parse_result(writer_path), parse_result(read_path)
                    java = javaopts.collect(tool, JAVA, {"writer": sent, "reader": received}, quick, False, True)
                    result = {"affinity": affinity.record(pinning, tool, sent, received, keeper),
                              "latency": received, "writer": sent,
                              "coordinated_omission": "intended timestamps; no skipped sends or rebasing"}
                else:
                    writer, writer_path = spawn("writer", "append" if workload == "W3" else mode)
                    wait(writer)
                    sent = parse_result(writer_path)
                    if workload != "W3":
                        java = javaopts.collect(tool, JAVA, {"writer": sent}, quick, False, False)
                        result = {"affinity": affinity.record(pinning, tool, sent, None, keeper), "append": sent}
                    else:
                        # Full untimed read warms the page cache. QueueIter and
                        # queue-file iteration leave the fixture unconsumed.
                        warm, _ = spawn("reader")
                        wait(warm)
                        read, read_path = spawn("reader")
                        wait(read)
                        received = parse_result(read_path)
                        java = javaopts.collect(tool, JAVA, {"reader": received}, quick, False, False)
                        result = {"affinity": affinity.record(pinning, tool, sent, received, keeper), "replay": received}
            finally:
                group.stop()
                for handle in handles:
                    handle.close()
                for output in outputs:
                    error_log.write(f"{output.name}: {output.read_text()}")
    guarantees = dict(GUARANTEES[tool])
    guarantees["zero_copy"] = tool == "chronicle" or tool == "aeron" or (tool == "tickertape" and reader == "mapped")
    result.update(tool=tool, tool_version=VERSIONS[tool], guarantees=guarantees,
                  mode=mode, workload=workload, payload=payload, count=count, warmup=warmup,
                  rate=rate if workload == "W2" else None, reader=reader, segment_size=134_217_728 if tool in ("tickertape", "yaque") else None,
                  repeat=repeat, quick=quick, java=java, selected_tools=list(selected))
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
        stats = result["append" if result["workload"].startswith("W1") else "replay"]
        assert stats["count"] == result["count"]
        assert math.isfinite(stats["records_per_s"]) and stats["records_per_s"] > 0
        expected = stats["records_per_s"] * result["payload"] / 1e6
        assert math.isclose(stats["payload_mb_per_s"], expected, rel_tol=1e-9)


def build(out, env, java_tools, tt_latency=False):
    """Builds the Rust adapters (and tt-latency for the short mode) and the selected Java adapters once."""
    with (out / "build.log").open("a") as log:
        execute(["cargo", "build", "--locked", "--release", "--manifest-path", HERE / "Cargo.toml",
                 "--target-dir", HERE / "target"], stdout=log, stderr=subprocess.STDOUT, env=env, timeout=1800)
        if tt_latency:
            execute(["cargo", "build", "--locked", "--release", "-p", "tickertape-bench", "--bin", "tt-latency",
                     "--target-dir", HERE / "target"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, env=env,
                    timeout=1800)
        for tool in java_tools:
            execute(["mvn", "-B", "-q", "-f", HERE / tool / "pom.xml", "package", "dependency:build-classpath", "dependency:list",
                     "-Dmdep.outputFile=target/classpath.txt",
                     f"-DoutputFile={out / (tool + '-dependencies.txt')}"], stdout=log, stderr=subprocess.STDOUT, env=env,
                    timeout=1800)


def java_environment(env, needed, parser):
    jdk = env.get("JAVA_HOME")
    if not jdk and Path("/opt/homebrew/opt/openjdk@21").exists():
        jdk = "/opt/homebrew/opt/openjdk@21"
    if not jdk:
        if needed:
            parser.error("set JAVA_HOME to Java 21 (or leave chronicle and aeron out with --tools)")
        jdk = "unavailable"
    env["JAVA_HOME"] = jdk
    env["PATH"] = str(Path(jdk) / "bin") + os.pathsep + env["PATH"]
    os.environ.update({"JAVA_HOME": jdk, "PATH": env["PATH"]})


SKIPPED = {"aeron": {**{w: "Archive not implemented; IPC has no persistent append workload"
                        for w in ("W1-cold", "W1-steady")},
                     "W3": "Archive not implemented; IPC cannot replay"},
           "queue-file": {"W2": "upstream forbids concurrent process access"}}


def short_matrix(out, env, quick, pinning, tools):
    """Every cell of the short mode for `tools` (in TOOLS order), twice. Results of a tool are the same whether it
    runs alone or with the others, apart from `selected_tools`."""
    environment(out, env, quick, pinning=pinning, tools=tools)
    build(out, env, [t for t in javaopts.JAVA_TOOLS if t in tools], tt_latency="tickertape" in tools)
    affinity.place_runner(pinning, *reaper.guard_pids())
    with (out / "commands.txt").open("w") as commands:
        for repeat in (1, 2):
            for payload in (64, 1024):
                for tool in tools:
                    if tool != "aeron":
                        for workload in ("W1-cold", "W1-steady"):
                            run_one(tool, workload, payload, "default", 100_000, repeat, out, env, quick, commands,
                                    pinning, tools)
                        for reader in (("safe", "mapped") if tool == "tickertape" else ("default",)):
                            run_one(tool, "W3", payload, reader, 100_000, repeat, out, env, quick, commands,
                                    pinning, tools)
                    if tool != "queue-file":
                        for reader in (("safe", "mapped") if tool == "tickertape" else ("default",)):
                            for rate in (100_000, 10_000):
                                run_one(tool, "W2", payload, reader, rate, repeat, out, env, quick, commands,
                                        pinning, tools)
    skipped = {tool: cases for tool, cases in SKIPPED.items() if tool in tools}
    (out / "skipped.json").write_text(json.dumps(skipped, indent=2) + "\n")


def main():
    suite_start = time.monotonic()
    reaper.install()
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("outdir", type=Path)
    parser.add_argument("--quick", action="store_true",
                        help="short mode: 100 warmup + 1000 measured latency; 2000 throughput records. "
                             "Sustained mode: 5 s runs, 1 s warmup, one repeat")
    parser.add_argument("--tools", help="comma-separated subset of " + ",".join(TOOLS) + " (default all). The "
                        "short mode runs them in this order whatever the order given; its results record the selection")
    sustained_args = parser.add_argument_group("sustained mode (ADR 0026; see compare/README.md)")
    sustained_args.add_argument("--sustained", action="store_true", help="run the resumable sustained matrix")
    sustained_args.add_argument("--resume", action="store_true", help="continue the matrix started in OUTDIR")
    sustained_args.add_argument("--dry-run", action="store_true", help="print the plan and estimate, run nothing")
    sustained_args.add_argument("--keep-going", action="store_true", help="record failed runs and continue")
    sustained_args.add_argument("--duration", type=float, help="seconds per run (default 600; quick 5)")
    sustained_args.add_argument("--warmup", type=float, help="paced seconds before the measured part (default 10; quick 1)")
    sustained_args.add_argument("--repeats", type=int, help="repeats of every paced run (default 2; quick 1)")
    sustained_args.add_argument("--workloads", help="comma-separated subset of probe,SV,SD (default all)")
    sustained_args.add_argument("--payloads", help="comma-separated subset of 64,1024 (default both)")
    sustained_args.add_argument("--rates", help="explicit paced rates in records/s, the same for every tool")
    sustained_args.add_argument("--rate-fractions", dest="fractions",
                                help="paced rates as fractions of each probe's achieved rate (default 0.5,0.9)")
    sustained_args.add_argument("--sync-every-ms", type=int, default=1000, help="SD sync interval (default 1000)")
    sustained_args.add_argument("--tt-readers", help="tickertape readers: safe,mapped (default mapped)")
    affinity.add_options(parser)
    javaopts.add_arguments(parser)
    args = parser.parse_args()
    JAVA.update(javaopts.settings(args))
    tools = args.tools.split(",") if args.tools else TOOLS
    if not args.sustained:
        try:
            tools = select_tools(args.tools)
        except ValueError as error:
            parser.error(str(error))
    try:
        args.affinity = affinity.resolve(args)
        if not args.dry_run:
            affinity.check(args.affinity, tools)
    except ValueError as error:
        parser.error(str(error))
    for line in affinity.warnings(args.affinity, tools):
        print(f"warning: {line}", file=sys.stderr)
    if args.sustained:
        import sustained
        env = dict(os.environ)
        java_environment(env, any(t in ("chronicle", "aeron") for t in tools) and not args.dry_run, parser)
        sustained.main(args, env, build)
        return
    if any([args.resume, args.dry_run, args.keep_going, args.duration, args.warmup, args.repeats, args.workloads,
            args.payloads, args.rates, args.fractions, args.tt_readers]):
        parser.error("these options need --sustained")
    # Exclusive output creation prevents accidentally mixing runs with old results.
    out = args.outdir.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if any(out.iterdir()):
        parser.error("OUTDIR must be new or empty")
    env = dict(os.environ)
    java_environment(env, any(t in javaopts.JAVA_TOOLS for t in tools), parser)
    pinning = args.affinity
    print(pinning["summary"], file=sys.stderr)
    reaper.start_guard(mark=out)
    complete = False
    try:
        short_matrix(out, env, args.quick, pinning, tools)
        complete = True
    finally:
        if not complete:
            # Children are stopped by now. An unfinished matrix is never left where a finished one would be.
            try:
                target = reaper.mark_incomplete(out)
            except OSError as error:
                target = None
                print(f"compare: could not mark {out} as incomplete: {error}", file=sys.stderr)
            if target:
                print(f"compare: incomplete; the results so far are in {target}", file=sys.stderr)
        reaper.stop_guard()
    elapsed = time.monotonic() - suite_start
    with (out / "environment.txt").open("a") as f:
        f.write(f"suite_elapsed_seconds: {elapsed:.3f}\n")
    print(f"done: {out} ({elapsed:.1f}s)")


def cli():
    try:
        main()
    except reaper.Terminated as stop:
        try:
            print(f"compare: {stop}", file=sys.stderr)
        except OSError:
            pass  # The terminal is gone (SIGHUP).
        raise
    except (OSError, ValueError, RuntimeError, AssertionError, subprocess.SubprocessError) as error:
        print(f"compare: {error}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    cli()
