#!/usr/bin/env python3
"""Recorded sustained session (ADR 0026): tickertape scenarios with tt-sustained, then the sustained comparison
matrix, all on the filesystem that holds --journal-dir, with results under one output directory.

The session is resumable: every finished run leaves its result in OUT, and running the same command again skips
it. It prints its estimated duration before starting, because cloud time costs money; non-interactive sessions
need --yes. See docs/playbooks/run-benchmark.md ("Sustained session").
"""
import argparse
import dataclasses
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shlex
import shutil
import signal
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
GIB = 1 << 30
MIB = 1 << 20
DEFAULT_SEGMENT = 128 * MIB
# Seconds a tickertape run takes beyond its schedule (process start, final checkpoint, drain, sync).
TT_OVERHEAD_S = 15
# Byte rate assumed for the beyond-RAM duration estimate before the probe has measured the real one.
ASSUMED_BYTE_RATE = 200 * 1_000_000

SCENARIOS = ("probe", "steady-50", "steady-90", "bursty", "lagging", "visibility-only", "beyond-ram")
MATRICES = {
    "quick": {"payloads": [64], "duration_s": 10, "warmup_s": 1, "repeats": 1,
              "scenarios": ["probe", "steady-50", "bursty", "lagging", "visibility-only"],
              "compare_args": ["--quick", "--duration", "3"]},
    "full": {"payloads": [64, 1024], "duration_s": 600, "warmup_s": 10, "repeats": 2,
             "scenarios": list(SCENARIOS), "compare_args": []},
}
SCENARIO_HELP = """Scenarios (R = the probe's achieved records/s for the payload; all but visibility-only checkpoint every
second and delete old segments by retention so the volume never fills):
  probe            unpaced, readers mapped + safe; measures R
  steady-50/-90    steady at 50 % / 90 % of R; readers mapped (spin), safe (default wait), safe durable-only (block)
  bursty           bursts at 2R for 20 % of every second (mean 0.4R), same readers: overload and catch-up
  lagging          steady 0.5R, 16 MiB segments, retention of 10 s of data (a quarter of a short run); readers
                   mapped, safe reading at a quarter of the rate (falls behind until retention removes segments
                   under it), safe with 5 s stalls every 30 s
  visibility-only  steady 0.5R without checkpoints or retention; rate capped so the data fits on the volume
  beyond-ram       steady 0.9R for long enough to append 1.5 x RAM (at least the run duration, at most --max-run),
                   retaining up to 1.5 x RAM; a reader at half the rate falls behind into data not in RAM"""


def record_bytes(payload):
    return (40 + payload + 7) // 8 * 8


class Skip(Exception):
    """A run that cannot be done on this machine, with the reason; recorded, never silently dropped."""


@dataclasses.dataclass(frozen=True)
class TtRun:
    label: str
    scenario: str
    payload: int
    repeat: int


def tt_plan(params):
    """Probes first (once per payload), then every other scenario per repeat and payload."""
    runs = []
    if "probe" in params["scenarios"] or any(s != "probe" for s in params["scenarios"]):
        for payload in params["payloads"]:
            runs.append(TtRun(f"tt-probe-{payload}-r1", "probe", payload, 1))
    for repeat in range(1, params["repeats"] + 1):
        for payload in params["payloads"]:
            for scenario in params["scenarios"]:
                if scenario != "probe":
                    runs.append(TtRun(f"tt-{scenario}-{payload}-r{repeat}", scenario, payload, repeat))
    return runs


def round_down(value, unit):
    return max(unit, value // unit * unit)


def scenario_args(run, probe_rate, host, params):
    """tt-sustained arguments and scheduled seconds for one run. `probe_rate` is None for the probe itself;
    `host` has "ram" and "free" (bytes free on the journal filesystem)."""
    payload, rb = run.payload, record_bytes(run.payload)
    duration, warmup = params["duration_s"], params["warmup_s"]
    retain = round_down(min(int(host["free"] * 0.4), 64 * GIB), DEFAULT_SEGMENT)
    if retain < 4 * DEFAULT_SEGMENT:
        raise Skip(f"only {host['free']} B free on the journal filesystem; need at least {10 * DEFAULT_SEGMENT} B")
    base = ["--payload", str(payload), "--checkpoint-every", "1s", "--retain-bytes", str(retain)]
    readers = "mapped,safe:wait=default,safe:durable:wait=block"
    timed = ["--duration", f"{duration}s", "--warmup", f"{warmup}s"]
    if run.scenario == "probe":
        return ["--unpaced", "--duration", f"{duration}s", *base, "--readers", "mapped,safe:wait=default"], duration
    if probe_rate is None:
        raise ValueError("scenarios after the probe need its rate")
    half = max(1, math.floor(probe_rate * 0.5))
    if run.scenario in ("steady-50", "steady-90"):
        rate = half if run.scenario == "steady-50" else max(1, math.floor(probe_rate * 0.9))
        return ["--rate", str(rate), *timed, *base, "--readers", readers], duration + warmup
    if run.scenario == "bursty":
        peak = max(5, math.floor(probe_rate * 2))
        return ["--burst-rate", str(peak), "--burst-period", "1s", "--burst-duty", "20", *timed, *base,
                "--readers", readers], duration + warmup
    if run.scenario == "lagging":
        segment = 16 * MIB
        # Ten seconds of data (a quarter of a short run), so the slow reader, whose lag grows at 75 % of the rate,
        # falls out of the retained window early in the run.
        window_s = min(10, (duration + warmup) / 4)
        window = round_down(min(retain, max(4 * segment, int(half * rb * window_s))), segment)
        delay_ns = math.ceil(4e9 / half)
        return ["--rate", str(half), *timed, "--payload", str(payload), "--checkpoint-every", "1s",
                "--segment-size", str(segment), "--retain-bytes", str(window),
                "--readers", f"mapped,safe:delay={delay_ns}ns,safe:wait=default:stall=5s/30s"], duration + warmup
    if run.scenario == "visibility-only":
        fits = math.floor(host["free"] * 0.5 / (rb * (duration + warmup)))
        rate = min(half, fits)
        if rate < 1:
            raise Skip("not enough free space for a visibility-only run without retention")
        return ["--rate", str(rate), *timed, "--payload", str(payload), "--readers", "mapped,safe:wait=default"], \
            duration + warmup
    if run.scenario == "beyond-ram":
        rate = max(1, math.floor(probe_rate * 0.9))
        needed = math.ceil(1.5 * host["ram"] / (rate * rb))
        seconds = min(params["max_run_s"], max(duration, needed))
        keep = round_down(min(int(host["free"] * 0.5), int(host["ram"] * 1.5)), DEFAULT_SEGMENT)
        delay_ns = math.ceil(2e9 / rate)
        return ["--rate", str(rate), "--duration", f"{seconds}s", "--warmup", f"{warmup}s", "--payload",
                str(payload), "--checkpoint-every", "1s", "--retain-bytes", str(keep),
                "--readers", f"mapped,safe:wait=default,safe:delay={delay_ns}ns"], seconds + warmup
    raise ValueError(f"unknown scenario {run.scenario}")


def probe_rate(out, payload):
    result = out / "tickertape" / f"tt-probe-{payload}-r1" / "result.json"
    if not result.exists():
        return None
    return json.loads(result.read_text())["writer"]["achieved_records_per_s"]


def tt_done(out, run):
    return (out / "tickertape" / run.label / "result.json").exists()


def estimate_tt(runs, out, host, params, manifest):
    """Estimated seconds for the unfinished tickertape runs (beyond-ram uses an assumed rate until probed)."""
    total = 0
    for run in runs:
        if tt_done(out, run) or manifest_status(manifest, run.label) == "skipped":
            continue
        if run.scenario == "probe":
            seconds = params["duration_s"]
        elif run.scenario == "beyond-ram" and probe_rate(out, run.payload) is None:
            needed = 1.5 * host["ram"] / ASSUMED_BYTE_RATE
            seconds = min(params["max_run_s"], max(params["duration_s"], needed)) + params["warmup_s"]
        else:
            rate = probe_rate(out, run.payload) or 1
            try:
                seconds = scenario_args(run, rate, host, params)[1]
            except Skip:
                continue
        total += seconds + TT_OVERHEAD_S
    return total


def manifest_status(manifest, label):
    return manifest.get("runs", {}).get(label, {}).get("status")


def now_utc():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_atomic(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text)
    os.replace(tmp, path)


def save_manifest(out, manifest):
    manifest["updated_utc"] = now_utc()
    write_atomic(out / "manifest.json", json.dumps(manifest, indent=2) + "\n")


def capture(command, cwd=None):
    try:
        result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=30,
                                check=False, cwd=cwd)
        return result.stdout.rstrip()
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unavailable: {error}"


def environment_commands(journal):
    j = str(journal)
    common = [["date", "-u", "+%FT%TZ"], ["git", "rev-parse", "HEAD"], ["git", "status", "--short"],
              ["rustc", "--version"], ["cargo", "--version"], ["python3", "--version"]]
    java = os.environ.get("JAVA_HOME")
    common += [[str(Path(java) / "bin" / "java") if java else "java", "-version"], ["mvn", "-version"]]
    if platform.system() == "Darwin":
        mount = capture(["df", "-P", j]).splitlines()[-1].split()[-1]
        return common + [["sw_vers"], ["uname", "-a"], ["sysctl", "machdep.cpu.brand_string", "hw.ncpu", "hw.memsize"],
                         ["df", "-h", j], ["mount"], ["diskutil", "info", mount], ["pmset", "-g", "batt"]]
    return common + [
        ["uname", "-a"], ["cat", "/etc/os-release"], ["systemd-detect-virt"],
        ["cat", "/sys/devices/virtual/dmi/id/product_name"], ["cat", "/sys/devices/virtual/dmi/id/sys_vendor"],
        ["lscpu"], ["nproc"], ["free", "-b"], ["numactl", "--hardware"],
        ["df", "-T", j], ["findmnt", "-T", j, "-o", "TARGET,SOURCE,FSTYPE,OPTIONS"],
        ["lsblk", "-o", "NAME,MODEL,SIZE,ROTA,TYPE,MOUNTPOINT,SCHED,DISC-GRAN"],
        ["sh", "-c", "grep . /sys/block/*/queue/scheduler /sys/block/*/queue/rotational 2>/dev/null"],
        ["sh", "-c", "grep . /sys/kernel/mm/transparent_hugepage/enabled "
                     "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null"],
        ["sysctl", "vm.dirty_ratio", "vm.dirty_background_ratio", "vm.dirty_bytes", "vm.dirty_background_bytes",
         "vm.dirty_expire_centisecs", "vm.dirty_writeback_centisecs", "vm.swappiness"],
        ["uptime"]]


def capture_environment(path, journal, params, matrix):
    with path.open("w") as f:
        f.write(f"session matrix: {matrix}\nparameters: {json.dumps(params)}\njournal_dir: {journal}\n")
        f.write("command: " + shlex.join(sys.argv) + "\n")
        if "orbstack" in platform.release().lower():
            f.write("environment: OrbStack VM (Linux on macOS)\n")
        f.write("Record manually for publication: instance type, host load, power and thermal state.\n")
        for command in environment_commands(journal):
            f.write("\n$ " + shlex.join(command) + "\n" + capture(command, cwd=ROOT) + "\n")


def reset_scratch(journal, out):
    """The session's own directory under --journal-dir, named after OUT; leftovers of a killed session go."""
    digest = hashlib.sha256(str(out).encode()).hexdigest()[:12]
    scratch = journal / f"tt-session-{digest}"
    owner = scratch / "owner.txt"
    if scratch.exists():
        if not owner.exists() or owner.read_text() != str(out):
            raise SystemExit(f"session: {scratch} exists but does not belong to {out}; remove it manually")
        shutil.rmtree(scratch)
    scratch.mkdir()
    owner.write_text(str(out))
    return scratch


def run_tt(run, args_, seconds, out, scratch, binary, cpus, manifest, commands):
    """Runs one tt-sustained measurement into OUT/tickertape/<label>/ and records it in the manifest."""
    result_dir = out / "tickertape" / run.label
    if result_dir.exists():
        shutil.rmtree(result_dir)  # A partial result of an interrupted run; the session owns this directory.
    journal = scratch / run.label
    if journal.exists():
        shutil.rmtree(journal)
    log = out / "logs" / f"{run.label}.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    command = [str(binary), "run", "--dir", str(journal), "--out", str(result_dir), *args_, "--json"]
    if cpus:
        command[len(command) - 1:len(command) - 1] = ["--cpus", cpus]
    entry = {"kind": "tickertape", "scenario": run.scenario, "payload": run.payload, "repeat": run.repeat,
             "command": shlex.join(command), "status": "running", "started_utc": now_utc()}
    manifest["runs"][run.label] = entry
    save_manifest(out, manifest)
    commands.write(f"{run.label}: {shlex.join(command)}\n")
    commands.flush()
    started = datetime.datetime.now(datetime.timezone.utc)
    with log.open("w") as handle:
        proc = subprocess.Popen(command, stdout=handle, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            status = proc.wait(timeout=seconds + 900)
        except subprocess.TimeoutExpired:
            status = None
        finally:
            if proc.poll() is None:
                os.killpg(proc.pid, signal.SIGKILL)  # tt-sustained's writer and readers share its process group.
                proc.wait()
            shutil.rmtree(journal, ignore_errors=True)
            os.sync()
    entry["finished_utc"] = now_utc()
    entry["seconds"] = (datetime.datetime.now(datetime.timezone.utc) - started).total_seconds()
    if status == 0 and (result_dir / "result.json").exists():
        entry.update(status="done", result=str((result_dir / "result.json").relative_to(out)))
    else:
        entry.update(status="failed", error=f"exit status {status}; see {log.relative_to(out)}")
    save_manifest(out, manifest)
    return entry["status"]


def compare_command(out, params, compare_tools, dry_run):
    command = [sys.executable, str(ROOT / "compare" / "run.py"), str(out / "compare"), "--sustained",
               "--keep-going", *params["compare_args"]]
    if compare_tools:
        command += ["--tools", compare_tools]
    if (out / "compare" / "matrix.json").exists() and not dry_run:
        command.append("--resume")
    if dry_run:
        command.append("--dry-run")
    return command


def compare_estimate(out, params, compare_tools, env):
    output = capture_with_env(compare_command(out, params, compare_tools, True), env)
    for line in output.splitlines():
        if line.startswith("estimated_seconds="):
            return int(line.split("=", 1)[1])
    raise SystemExit(f"session: cannot estimate the comparison matrix:\n{output}")


def capture_with_env(command, env):
    result = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env,
                            check=False, timeout=120)
    return result.stdout


def format_seconds(seconds):
    seconds = int(seconds)
    return f"{seconds // 3600}h{seconds % 3600 // 60:02d}m{seconds % 60:02d}s"


def host_facts(journal):
    ram = None
    if platform.system() == "Darwin":
        ram = int(capture(["sysctl", "-n", "hw.memsize"]) or 0)
    else:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                ram = int(line.split()[1]) * 1024
    return {"ram": ram or 0, "free": shutil.disk_usage(journal).free}


def execute_tt(runs, out, scratch, host, params, manifest, runner):
    """Runs the unfinished tickertape runs in order; skipped and failed runs are recorded and the session goes on."""
    counts = {"done": 0, "failed": 0, "skipped": 0, "already": 0}
    with (out / "commands.txt").open("a") as commands:
        for index, run in enumerate(runs, 1):
            if tt_done(out, run):
                counts["already"] += 1
                continue
            if manifest_status(manifest, run.label) == "skipped":
                counts["already"] += 1
                continue
            rate = None if run.scenario == "probe" else probe_rate(out, run.payload)
            if run.scenario != "probe" and rate is None:
                manifest["runs"][run.label] = {"kind": "tickertape", "status": "failed",
                                               "error": f"probe for {run.payload} B has no result"}
                save_manifest(out, manifest)
                counts["failed"] += 1
                continue
            try:
                args_, seconds = scenario_args(run, rate, host, params)
            except Skip as reason:
                manifest["runs"][run.label] = {"kind": "tickertape", "status": "skipped", "reason": str(reason)}
                save_manifest(out, manifest)
                counts["skipped"] += 1
                continue
            print(f"[tickertape {index}/{len(runs)}] {run.label}: {seconds:.0f} s scheduled", flush=True)
            counts[runner(run, args_, seconds, commands)] += 1
    return counts


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog=SCENARIO_HELP,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--journal-dir", type=Path, required=True,
                        help="existing directory on the filesystem under test; journals and comparison data go here")
    parser.add_argument("--out", type=Path, required=True, help="output directory (created; resumed if it exists)")
    parser.add_argument("--matrix", choices=sorted(MATRICES), default="full")
    parser.add_argument("--quick", action="store_true", help="same as --matrix quick: a few minutes in total")
    parser.add_argument("--only", choices=("tickertape", "compare"), help="run only one half of the session")
    parser.add_argument("--scenarios", help="comma-separated subset of tickertape scenarios")
    parser.add_argument("--compare-tools", help="comma-separated subset of comparison tools")
    parser.add_argument("--duration", type=int, help="seconds per tickertape run (default from the matrix)")
    parser.add_argument("--repeats", type=int, help="repeats of every tickertape scenario (default from the matrix)")
    parser.add_argument("--max-run", type=int, default=7200, help="longest beyond-ram run in seconds (default 7200)")
    parser.add_argument("--cpus", help="passed to tt-sustained --cpus (writer, then readers; Linux only)")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and estimate, run nothing")
    parser.add_argument("--yes", action="store_true", help="start without asking (required when not interactive)")
    args = parser.parse_args(argv)
    if args.quick:
        args.matrix = "quick"
    return args


def parameters(args):
    matrix = dict(MATRICES[args.matrix])
    if args.scenarios:
        chosen = args.scenarios.split(",")
        bad = [s for s in chosen if s not in SCENARIOS]
        if bad:
            raise SystemExit(f"session: unknown scenarios {bad}; choose from {', '.join(SCENARIOS)}")
        matrix["scenarios"] = chosen
    if args.duration is not None:
        matrix["duration_s"] = args.duration
    if args.repeats is not None:
        matrix["repeats"] = args.repeats
    if matrix["duration_s"] <= 0 or matrix["repeats"] < 1:
        raise SystemExit("session: duration and repeats must be positive")
    matrix["max_run_s"] = args.max_run
    matrix["only"] = args.only
    matrix["compare_tools"] = args.compare_tools
    return matrix


def main(argv=None):
    sys.stdout.reconfigure(line_buffering=True)
    args = parse_args(argv)
    params = parameters(args)
    journal = args.journal_dir.resolve()
    if not journal.is_dir():
        raise SystemExit(f"session: --journal-dir {journal} must be an existing directory")
    out = args.out.resolve()
    if out == journal or out.is_relative_to(journal) or journal.is_relative_to(out):
        raise SystemExit("session: --out and --journal-dir must not contain each other")
    manifest_path = out / "manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    if manifest and manifest.get("parameters") != params:
        raise SystemExit(f"session: {out} was started with different parameters:\n  {manifest.get('parameters')}\n"
                         f"  now {params}\nUse a new --out, or the original options.")
    if not manifest and out.exists() and any(out.iterdir()):
        raise SystemExit(f"session: {out} is not empty and has no manifest.json")
    host = host_facts(journal)
    env = dict(os.environ)
    runs = tt_plan(params) if params["only"] != "compare" else []
    tt_seconds = estimate_tt(runs, out, host, params, manifest)
    compare_seconds = compare_estimate(out, params, params["compare_tools"], env) \
        if params["only"] != "tickertape" else 0
    total = tt_seconds + compare_seconds
    print(f"session {args.matrix}: journal filesystem {journal} ({host['free']} B free), RAM {host['ram']} B")
    print(f"  tickertape: {len(runs)} runs ({sum(tt_done(out, r) for r in runs)} finished), "
          f"estimated {format_seconds(tt_seconds)}")
    print(f"  comparison: estimated {format_seconds(compare_seconds)}")
    print(f"  estimated total remaining: {format_seconds(total)} (beyond-ram durations depend on the probe)")
    print(f"estimated_seconds={int(total)}")
    if args.dry_run:
        for run in runs:
            if not tt_done(out, run):
                print(f"  {run.label}")
        return 0
    if not args.yes:
        if not sys.stdin.isatty():
            raise SystemExit("session: not interactive; pass --yes to start")
        if input("Start? [y/N] ").strip().lower() != "y":
            return 1
    out.mkdir(parents=True, exist_ok=True)
    first = not manifest
    if first:
        manifest = {"matrix": args.matrix, "parameters": params, "created_utc": now_utc(), "journal_dir": str(journal),
                    "commit": capture(["git", "rev-parse", "HEAD"], cwd=ROOT), "runs": {}, "environment": []}
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    env_file = out / ("environment.txt" if first else f"environment-resume-{stamp}.txt")
    capture_environment(env_file, journal, params, args.matrix)
    manifest["environment"].append(env_file.name)
    manifest.setdefault("sessions", []).append({"started_utc": now_utc(), "command": shlex.join(sys.argv),
                                                "estimated_seconds": int(total)})
    save_manifest(out, manifest)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda signum, frame: sys.exit(128 + signum))
    scratch = reset_scratch(journal, out)
    failed = 0
    if runs:
        (out / "logs").mkdir(exist_ok=True)
        with (out / "logs" / "build.log").open("a") as log:
            subprocess.run(["cargo", "build", "--locked", "--release", "-p", "tickertape-bench", "--bin",
                            "tt-sustained"], cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, check=True)
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        binary = (target if target.is_absolute() else ROOT / target) / "release" / "tt-sustained"
        counts = execute_tt(runs, out, scratch, host, params, manifest,
                            lambda run, a, s, c: run_tt(run, a, s, out, scratch, binary, args.cpus, manifest, c))
        failed += counts["failed"]
        print(f"tickertape: {counts}")
    if params["only"] != "tickertape":
        data = scratch / "compare-data"
        data.mkdir()
        command = compare_command(out, params, params["compare_tools"], False)
        manifest["compare"] = {"outdir": "compare", "command": shlex.join(command), "data_dir": str(data),
                               "status": "running", "started_utc": now_utc()}
        save_manifest(out, manifest)
        with (out / "logs" / "compare.log").open("a") as log:
            proc = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=dict(os.environ, TMPDIR=str(data)),
                                    start_new_session=True)
            try:
                status = proc.wait()
            finally:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGTERM)  # The runner reaps its own children on SIGTERM.
                    proc.wait()
        manifest["compare"].update(status="done" if status == 0 else "failed", exit_status=status,
                                   finished_utc=now_utc())
        save_manifest(out, manifest)
        failed += status != 0
        print(f"comparison: exit status {status}; see {out / 'logs' / 'compare.log'}")
    shutil.rmtree(scratch, ignore_errors=True)
    print(f"session done: {out} (manifest.json); {failed} failures")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
