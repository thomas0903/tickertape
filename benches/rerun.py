#!/usr/bin/env python3
"""Comparison rerun (issue #57): runs named parts of the matrix in docs/playbooks/rerun-comparison.md on one machine.

A part is an ordered list of units. A unit is one invocation of compare/run.py (one tool, one configuration, short or
sustained mode) with its own directory under OUT. This driver adds nothing to what is measured: it orders the units so
that drift does not favour a tool, records what the playbook asks for, retries a failed unit once, and resumes: the
same command again skips finished units. Results of a unit live in OUT/<part>-<filesystem>/<unit>/.

Standard library only. Linux only for real runs (pinning).
"""
import argparse
import dataclasses
import datetime
import fcntl
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys

import session

ROOT = Path(__file__).resolve().parent.parent
COMPARE = ROOT / "compare"
TOOLS = ("tickertape", "chronicle", "aeron", "yaque", "queue-file", "plain-file")
JAVA = ("chronicle", "aeron")
# The library that the rerun is about. The harness may be newer, the library may not.
LIBRARY_TAG, LIBRARY_PATHS = "v1.0.0", ["crates/tickertape"]

# Configurations: options of compare/run.py. `tuned` names CPUs, so its options come from the command line.
# compare/README.md ("JVM warm-up, tool settings and JVM logs"): all switches together are the old settings.
OLD_JVM = ["--no-long-warmup", "--no-micro-touch", "--chronicle-block-size", "default", "--aeron-sparse-terms",
           "--no-aeron-pre-touch", "--no-jvm-logs"]
CONFIGS = {
    "pinned": [],
    "unpinned": ["--no-pin"],
    "jvm-old": OLD_JVM,
    "microtouch-only": [o for o in OLD_JVM if o != "--no-micro-touch"],
    "tuned": [],
}

# Estimates only, for --list and the deadline. Seconds of a short-mode unit (two repeats): the W2 schedule (110 s per
# run at 10,000/s, 11 s at 100,000/s) and the rest (W1, W3, process starts, JVM warm-up). Measured on
# c4-standard-16-lssd on 2026-11-25: chronicle 1,117 s, aeron 1,070 s with the new JVM settings (long warm-up).
W2_S = {"tickertape": 2 * 484, "chronicle": 484, "aeron": 484, "yaque": 484, "queue-file": 0, "plain-file": 484}
SHORT_REST_S = {"tickertape": 60, "chronicle": 640, "aeron": 600, "yaque": 75, "queue-file": 60, "plain-file": 60}
SHORT_REST_OLD_JVM_S = 160  # jvm-old and microtouch-only: the short JVM warm-up
SUSTAINED_OVERHEAD_S = {"chronicle": 30, "aeron": 30}
SUSTAINED_DEFAULT_OVERHEAD_S = 15
NO_SD = ("aeron", "yaque")


@dataclasses.dataclass(frozen=True)
class Unit:
    name: str
    tool: str
    config: str
    mode: str  # "short" or "sustained"
    workloads: tuple = ()  # sustained only
    fractions: tuple = ()

    def options(self, tuned_options, quick):
        options = list(CONFIGS[self.config])
        if self.config == "tuned":
            options = list(tuned_options)
        if self.mode == "sustained":
            options += ["--sustained", "--keep-going", "--tools", self.tool, "--workloads", ",".join(self.workloads),
                        "--rate-fractions", ",".join(str(f) for f in self.fractions), "--repeats", "1"]
        if quick:
            options += ["--quick"]
        return options

    def sustained_runs(self, quick):
        payloads = 1 if quick else 2
        kinds = 0
        for workload in self.workloads:
            if workload == "probe":
                kinds += 1
            elif workload == "SV" or self.tool not in NO_SD:
                kinds += len(self.fractions)
        return kinds * payloads

    def estimate(self, quick):
        if self.mode == "short":
            rest = SHORT_REST_OLD_JVM_S if self.config in ("jvm-old", "microtouch-only") else SHORT_REST_S[self.tool]
            return 90 if quick else W2_S[self.tool] + rest
        overhead = SUSTAINED_OVERHEAD_S.get(self.tool, SUSTAINED_DEFAULT_OVERHEAD_S)
        return self.sustained_runs(quick) * ((6 if quick else 610) + overhead)


def rotate(items, by):
    by %= len(items)
    return list(items[by:]) + list(items[:by])


def short_units(rounds):
    """`rounds` is a list of [(config, tools)]. Round k starts with another tool, and every second round runs the
    configurations of a tool in the opposite order, so a slow drift hits every tool and configuration alike."""
    units = []
    for index, configs in enumerate(rounds):
        names = [c for c, _ in configs]
        for tool in rotate(TOOLS, 2 * index):
            for config in (names if index % 2 == 0 else names[::-1]):
                if tool in dict(configs)[config]:
                    units.append(Unit(f"short-{config}-{tool}-round{index + 1}", tool, config, "short"))
    return units


def sustained_units(rounds, config, workloads, fractions, prefix="sustained"):
    units = []
    for index, tools in enumerate(rounds):
        for tool in rotate(tools, index):
            units.append(Unit(f"{prefix}-{config}-{tool}-round{index + 1}", tool, config, "sustained",
                              tuple(workloads), tuple(fractions)))
    return units


ALL = ("probe", "SV", "SD")
HEADLINE = ("tickertape", "chronicle", "aeron")
JOURNALS = ("tickertape", "chronicle")
OTHERS = ("yaque", "queue-file", "plain-file")
PARTS = {
    # Every configuration, interleaved per tool. Round 2 repeats what the headlines and the before/after rows need.
    "short": short_units([
        [("pinned", TOOLS), ("unpinned", TOOLS), ("jvm-old", JAVA), ("microtouch-only", ("chronicle",))],
        [("pinned", TOOLS), ("jvm-old", JAVA), ("microtouch-only", ("chronicle",))]]),
    "short-pinned": short_units([[("pinned", TOOLS)]]),
    "sustained": sustained_units([HEADLINE, HEADLINE], "pinned", ALL, [0.5]),
    "sustained-journals": sustained_units([JOURNALS], "pinned", ALL, [0.5]),
    "sustained-others": sustained_units([OTHERS], "pinned", ("probe", "SV"), [0.5]),
    "sustained-90": sustained_units([JOURNALS], "pinned", ("probe", "SD"), [0.9], prefix="sustained90"),
    "sustained-jvm-old": sustained_units([JAVA], "jvm-old", ALL, [0.5]),
    "tuned-short": short_units([[("tuned", TOOLS)]]),
    "tuned-sustained": sustained_units([HEADLINE], "tuned", ALL, [0.5]),
}


def is_tuned(part):
    return any(u.config == "tuned" for u in PARTS[part])


def part_estimate(part, quick=False):
    return sum(u.estimate(quick) for u in PARTS[part])


def short_mode(tools, argv):
    """compare/run.py's short mode for a subset of tools: it has no --tools of its own."""
    sys.path.insert(0, str(COMPARE))
    import run
    bad = set(tools) - set(run.TOOLS)
    if bad:
        raise SystemExit(f"rerun: unknown tools {sorted(bad)}")
    run.TOOLS = tuple(tools)
    # Short mode has no signal handling, and its children are process groups of their own: without this they would
    # outlive an interrupted unit. SystemExit runs its cleanup.
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, lambda signum, frame: sys.exit(128 + signum))
    sys.argv = [f"benches/rerun.py --short-tools {','.join(tools)} --", *argv]
    run.main()


def command(unit, directory, tuned_options, quick, resume):
    options = unit.options(tuned_options, quick)
    if unit.mode == "short":
        return [sys.executable, str(Path(__file__).resolve()), "--short-tools", unit.tool, "--", str(directory),
                *options]
    return [sys.executable, str(COMPARE / "run.py"), str(directory), *options, *(["--resume"] if resume else [])]


def git(*args):
    return subprocess.run(["git", *args], cwd=ROOT, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)


def preflight(parts, units, tuned_options, allow_library_diff):
    """Everything that would otherwise fail hours into a paid session."""
    problems = []
    if git("status", "--porcelain", "--untracked-files=no").stdout.strip():
        problems.append("the checkout has uncommitted changes")
    diff = git("diff", "--quiet", LIBRARY_TAG, "--", *LIBRARY_PATHS)
    if diff.returncode and not allow_library_diff:
        problems.append(f"{', '.join(LIBRARY_PATHS)} differs from {LIBRARY_TAG} (or the tag is missing: git fetch "
                        f"--tags); check out the library of {LIBRARY_TAG}, or pass --allow-library-diff")
    helptext = session.capture([sys.executable, str(COMPARE / "run.py"), "--help"])
    for option in sorted({o for u in units for o in u.options(tuned_options, False) if o.startswith("--")}):
        if option not in helptext:
            problems.append(f"compare/run.py has no {option} in this checkout")
    isolated = session.capture(["cat", "/sys/devices/system/cpu/isolated"])
    tuned = [p for p in parts if is_tuned(p)]
    if tuned and len(tuned) != len(parts):
        problems.append("tuned parts need another boot than the others; run them separately")
    if tuned and not isolated:
        problems.append("a tuned part needs isolated CPUs (benches/rerun-vm.sh tune)")
    if tuned and not tuned_options:
        problems.append("a tuned part needs --tuned-options with the isolated CPUs")
    if not tuned and isolated:
        problems.append(f"CPUs {isolated} are isolated: this is a tuned boot, which only tuned parts may use")
    return problems


def environment_commands(data):
    """session.py's environment, and what it lacks for this rerun: the CPU layout, boot options and machine type."""
    metadata = "http://metadata.google.internal/computeMetadata/v1/instance/"
    return session.environment_commands(data) + [
        ["cat", "/proc/cmdline"],
        ["lscpu", "-e=CPU,CORE,SOCKET,NODE,CACHE,ONLINE"],
        ["sh", "-c", "grep . /sys/devices/system/cpu/cpu*/topology/thread_siblings_list"],
        ["sh", "-c", "cd /sys/devices/system/cpu; grep . isolated nohz_full online offline smt/control smt/active"],
        ["sh", "-c", "grep -E 'CONFIG_(NO_HZ_FULL|RCU_NOCB_CPU|CPU_ISOLATION|HZ|PREEMPT[A-Z_]*)=' "
                     "/boot/config-$(uname -r)"],
        ["sh", "-c", f"for k in machine-type zone image; do curl -sf -m 2 -H Metadata-Flavor:Google {metadata}$k; "
                     "echo; done"],
        ["chronyc", "tracking"],
        ["sh", "-c", "systemctl list-timers --all --no-pager | head -20"],
        ["git", "log", "-1", "--format=%H %cI %s"], ["git", "describe", "--tags", "--always", "--dirty"],
        ["git", "diff", "--stat", LIBRARY_TAG, "--", "crates"],
    ]


def capture_environment(path, data):
    with path.open("w") as f:
        f.write(f"command: {shlex.join(sys.argv)}\ndata_dir: {data}\n")
        for cmd in environment_commands(data):
            f.write("\n$ " + shlex.join(cmd) + "\n" + session.capture(cmd, cwd=ROOT) + "\n")


class Interrupted(Exception):
    pass


def run_child(cmd, log, env):
    """Runs one unit in its own process group; a signal to this driver ends the group and keeps the unit pending."""
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, env=env, start_new_session=True, cwd=ROOT)
    try:
        return proc.wait()
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGTERM)  # run.py reaps its own children on SIGTERM in sustained mode.
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()


def set_aside(directory):
    """An unfinished short unit cannot be resumed. It is kept beside the new one, never mixed with it."""
    n = 1
    while (target := directory.with_name(f"{directory.name}.incomplete-{n}")).exists():
        n += 1
    directory.rename(target)


def run_unit(unit, directory, scratch, args, manifest, save):
    entry = manifest["units"].setdefault(f"{directory.parent.name}/{unit.name}", {"attempts": []})
    resume = False
    if directory.exists() and any(directory.iterdir()):
        resume = unit.mode == "sustained" and (directory / "matrix.json").exists()
        if not resume:
            set_aside(directory)
    directory.mkdir(parents=True, exist_ok=True)
    shutil.rmtree(scratch, ignore_errors=True)
    scratch.mkdir()
    cmd = command(unit, directory, args.tuned_options, args.quick, resume)
    attempt = {"command": shlex.join(cmd), "tmpdir": str(scratch), "started_utc": session.now_utc(),
               "status": "running"}
    entry["attempts"].append(attempt)
    entry["status"] = "running"
    save()
    log_path = directory.with_name(directory.name + ".log")
    try:
        with log_path.open("a") as log:
            log.write(f"\n{attempt['started_utc']} {attempt['command']}\n")
            log.flush()
            status = run_child(cmd, log, dict(os.environ, TMPDIR=str(scratch)))
    except BaseException:
        attempt.update(status="interrupted", finished_utc=session.now_utc())
        entry["status"] = "interrupted"
        save()
        raise
    attempt.update(status="done" if status == 0 else "failed", exit_status=status, finished_utc=session.now_utc())
    started, finished = (datetime.datetime.fromisoformat(attempt[k]) for k in ("started_utc", "finished_utc"))
    attempt["seconds"] = round((finished - started).total_seconds(), 1)
    entry["status"] = attempt["status"]
    save()
    shutil.rmtree(scratch, ignore_errors=True)
    os.sync()
    return entry["status"]


def execute(plan, out, scratch, args, manifest, save, runner=run_unit, now=None):
    """Runs the pending units in order, then every failed one once more. Returns the labels that are not done."""
    now = now or (lambda: datetime.datetime.now(datetime.timezone.utc))
    for attempt in (1, 2):
        for part_dir, unit in plan:
            label = f"{part_dir}/{unit.name}"
            entry = manifest["units"].get(label, {})
            if entry.get("status") == "done":
                continue
            failures = sum(a["status"] == "failed" for a in entry.get("attempts", []))
            if failures >= 2 or (attempt == 1 and failures):
                continue
            if args.deadline and now() + datetime.timedelta(seconds=unit.estimate(args.quick)) > args.deadline:
                manifest["units"].setdefault(label, {"attempts": []})["status"] = "not started: deadline"
                save()
                print(f"{label}: not started, it would run past {args.deadline.isoformat()}", flush=True)
                continue
            print(f"{session.now_utc()} {label} (estimated {session.format_seconds(unit.estimate(args.quick))})",
                  flush=True)
            result = runner(unit, out / part_dir / unit.name, scratch, args, manifest, save)
            print(f"{session.now_utc()} {label}: {result}", flush=True)
    return [f"{p}/{u.name}" for p, u in plan if manifest["units"].get(f"{p}/{u.name}", {}).get("status") != "done"]


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", action="store_true", help="print the parts with their units and estimates")
    parser.add_argument("--part", action="append", default=[], metavar="PART:DIR",
                        help="part and the directory on the filesystem under test, for example short:/data/tt; "
                             "repeatable, run in this order")
    parser.add_argument("--out", type=Path, help="output directory, on another filesystem (resumed if it exists)")
    parser.add_argument("--tuned-options", type=shlex.split, default=[], metavar="OPTIONS",
                        help='pinning of tuned parts, "--writer-cpu 1 --reader-cpu 2 --helper-cpus 0,3-7"')
    parser.add_argument("--units", help="only units whose name contains this text (dry runs)")
    parser.add_argument("--deadline", type=datetime.datetime.fromisoformat,
                        help="UTC time (2026-11-27T10:00:00+00:00) after which no unit may run: before the VM deletes "
                             "itself")
    parser.add_argument("--quick", action="store_true", help="run.py --quick in every unit; functional, never results")
    parser.add_argument("--allow-library-diff", action="store_true", help=f"accept a library other than {LIBRARY_TAG}")
    parser.add_argument("--dry-run", action="store_true", help="print the plan and the estimate, run nothing")
    args = parser.parse_args(argv)
    chosen = []
    for item in args.part:
        part, _, data = item.partition(":")
        if part not in PARTS or not data:
            parser.error(f"--part {item}: use PART:DIR with a part from --list")
        chosen.append((part, Path(data)))
    if args.deadline and args.deadline.tzinfo is None:
        parser.error("--deadline needs a UTC offset")
    if not args.list and (not chosen or (not args.out and not args.dry_run)):
        parser.error("name at least one --part PART:DIR and --out (or use --list)")
    args.chosen = chosen
    return args


def build_plan(args):
    plan, data_dirs = [], {}
    for part, data in args.chosen:
        name = f"{part}-{filesystem(data)}"
        if name in data_dirs:
            raise SystemExit(f"rerun: {name} is listed twice")
        data_dirs[name] = data
        plan += [(name, u) for u in PARTS[part] if not args.units or args.units in u.name]
    return plan, data_dirs


def filesystem(path):
    kind = session.capture(["stat", "-f", "-c", "%T", str(path)]).strip()
    return {"ext2/ext3": "ext4"}.get(kind, kind.replace("/", "-") if kind and " " not in kind else "fs")


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv[:1] == ["--short-tools"] and argv[2:3] == ["--"]:
        return short_mode(argv[1].split(","), argv[3:])
    args = parse_args(argv)
    if args.list:
        for part, units in PARTS.items():
            print(f"{part}: {len(units)} units, about {session.format_seconds(part_estimate(part))}")
            for unit in units:
                print(f"  {unit.name}  {session.format_seconds(unit.estimate(False))}")
        return 0
    plan, data_dirs = build_plan(args)
    total = sum(u.estimate(args.quick) for _, u in plan)
    print(f"{len(plan)} units, estimated {session.format_seconds(total)}\nestimated_seconds={total}")
    if args.dry_run:
        for part_dir, unit in plan:
            print(f"  {part_dir}/{unit.name}: {shlex.join(unit.options(args.tuned_options, args.quick))}")
        return 0
    for data in data_dirs.values():
        if not data.is_dir():
            raise SystemExit(f"rerun: {data} must be an existing directory")
    problems = preflight([p for p, _ in args.chosen], [u for _, u in plan], args.tuned_options, args.allow_library_diff)
    if problems:
        raise SystemExit("rerun: not started:\n  " + "\n  ".join(problems))
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    lock = (out / "lock").open("w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        raise SystemExit(f"rerun: another driver is using {out}") from None
    path = out / "rerun.json"
    manifest = json.loads(path.read_text()) if path.exists() else {"units": {}, "sessions": []}
    # One commit and one mode per OUT: otherwise the units would not be comparable.
    identity = {"commit": session.capture(["git", "rev-parse", "HEAD"], cwd=ROOT), "quick": args.quick}
    if manifest.setdefault("identity", identity) != identity:
        raise SystemExit(f"rerun: {out} was started as {manifest['identity']}, now {identity}; use a new --out")
    save = lambda: session.write_atomic(path, json.dumps(manifest, indent=2) + "\n")
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    for name, data in data_dirs.items():
        capture_environment(out / f"environment-{name}-{stamp}.txt", data)
    manifest["sessions"].append({"started_utc": session.now_utc(), "command": shlex.join([sys.executable, *sys.argv]),
                                 "parts": {n: str(d) for n, d in data_dirs.items()}, "estimated_seconds": total})
    save()

    def stop(signum, frame):
        raise Interrupted(signal.Signals(signum).name)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, stop)
    open_units = []
    try:
        for name, data in data_dirs.items():
            # The scratch directory is this driver's own; leftovers of a killed session must not fill the disk.
            open_units += execute([e for e in plan if e[0] == name], out, data / "tt-rerun", args, manifest, save)
    except (Interrupted, KeyboardInterrupt) as error:
        print(f"rerun: interrupted ({error or 'SIGINT'}); run the same command to continue", file=sys.stderr)
        return 130
    print(f"rerun done: {out}; not done: {open_units or 'none'}")
    return 1 if open_units else 0


if __name__ == "__main__":
    sys.exit(main())
