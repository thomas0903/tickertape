"""Sustained comparison mode (ADR 0026): a resumable matrix of long runs per tool.

Workloads, per tool and payload:
  probe  unpaced sends for the run duration, with a concurrent reader where the tool supports one. Its achieved
         rate is the tool's sustained throughput on this machine; paced rates derive from it.
  SV     sustained visibility: paced sends at a fixed rate on intended times (no skipping, no rebasing), one
         concurrent reader measuring now - intended. No harness sync.
  SD     sustained periodic durability: SV plus the tool's closest equivalent of a checkpoint every
         --sync-every-ms on the writer thread. Tools without such an operation are listed as unsupported.

Every run's result is written to its own file in runs/ as soon as it finishes (atomically), so an interrupted
matrix resumes with --resume and skips finished runs. Nothing is dropped silently: a writer that falls behind
shows late sends and latency, and the runner rejects a run whose reader did not receive every record.
"""
import dataclasses
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

import affinity
import javaopts
import reaper

HERE = Path(__file__).resolve().parent

WORKLOADS = ("probe", "SV", "SD")
DEFAULT_TOOLS = ("tickertape", "chronicle", "aeron", "yaque", "queue-file", "plain-file")
# Concurrent reader support and the periodic durability operation (None: unsupported, with the reason).
SUPPORT = {
    "tickertape": {"reader": True, "SD": "Writer::checkpoint (data + durable marker)"},
    "chronicle": {"reader": True, "SD": "ExcerptAppender.sync with SyncMode.SYNC (msync MS_SYNC)"},
    "aeron": {"reader": True, "SD": None,
              "SD_reason": "IPC transport without persistence (Archive not implemented)"},
    "yaque": {"reader": True, "SD": None, "SD_reason": "no sync operation exposed (try_send only flushes to the OS)"},
    "queue-file": {"reader": False, "SD": "QueueFile::sync_all (fsync)",
                   "reader_reason": "upstream forbids concurrent process access: writer-only runs"},
    "plain-file": {"reader": True, "SD": "File::sync_data (fdatasync)"},
}
JAVA_TOOLS = javaopts.JAVA_TOOLS
# Seconds a run takes beyond its scheduled time (process and JVM start, drain, sync); used for estimates only.
OVERHEAD_S = {"chronicle": 25, "aeron": 25}
DEFAULT_OVERHEAD_S = 4
SYNC_S = 5  # os.sync() after a long run; quick runs write little
QUICK_SYNC_S = 1


def record_disk_bytes(tool, payload):
    """Conservative on-disk bytes per record, for disk budgeting (not for results)."""
    if tool == "tickertape":
        return (40 + payload + 7) // 8 * 8
    if tool == "aeron":
        return 0  # IPC term buffers are reused; the driver directory has a fixed size.
    # 16 B envelope plus framing/index overhead with a safety margin.
    return int((payload + 16) * 1.25) + 32


@dataclasses.dataclass(frozen=True)
class Run:
    label: str
    tool: str
    workload: str
    payload: int
    reader: str
    rate_tag: str  # "unpaced", "f50" (50 % of the probe) or "r10000" (explicit)
    repeat: int


def readers_for(tool, params):
    return tuple(params["tt_readers"]) if tool == "tickertape" else ("default",)


def rate_tags(params):
    if params["rates"]:
        return tuple(f"r{rate}" for rate in params["rates"])
    return tuple(f"f{round(fraction * 100)}" for fraction in params["fractions"])


def label(workload, tool, reader, payload, rate_tag, repeat):
    return f"{workload}-{tool}-{reader}-{payload}-{rate_tag}-r{repeat}"


def unsupported(params):
    """Static unsupported cases, with reasons, for the selected tools and workloads."""
    cases = {}
    for tool in params["tools"]:
        if "SD" in params["workloads"] and SUPPORT[tool]["SD"] is None:
            cases[f"{tool}/SD"] = SUPPORT[tool]["SD_reason"]
        if not SUPPORT[tool]["reader"]:
            cases[f"{tool}/reader"] = SUPPORT[tool]["reader_reason"]
    return cases


def plan(params):
    """All runs of the matrix, in execution order: probes first, then repeats with tools interleaved."""
    runs = []
    if params["needs_probe"]:
        for payload in params["payloads"]:
            for tool in params["tools"]:
                for reader in readers_for(tool, params):
                    runs.append(Run(label("probe", tool, reader, payload, "unpaced", 1),
                                    tool, "probe", payload, reader, "unpaced", 1))
    for repeat in range(1, params["repeats"] + 1):
        for payload in params["payloads"]:
            for tool in params["tools"]:
                for reader in readers_for(tool, params):
                    for workload in ("SV", "SD"):
                        if workload not in params["workloads"]:
                            continue
                        if workload == "SD" and SUPPORT[tool]["SD"] is None:
                            continue
                        for tag in rate_tags(params):
                            runs.append(Run(label(workload, tool, reader, payload, tag, repeat),
                                            tool, workload, payload, reader, tag, repeat))
    return runs


def resolve_rate(run, params, out):
    """The paced rate of a run: explicit, or a fraction of the matching probe's achieved rate."""
    if run.rate_tag == "unpaced":
        return None
    if run.rate_tag.startswith("r"):
        return int(run.rate_tag[1:])
    probe = out / "runs" / (label("probe", run.tool, run.reader, run.payload, "unpaced", 1) + ".json")
    if not probe.exists():
        raise RuntimeError(f"{run.label}: probe result {probe.name} missing (probe failed or was skipped)")
    achieved = json.loads(probe.read_text())["writer"]["achieved_records_per_s"]
    return max(1, math.floor(achieved * int(run.rate_tag[1:]) / 100))


def finished(run, out):
    return (out / "runs" / f"{run.label}.json").exists() or (out / "skipped" / f"{run.label}.json").exists()


def estimate_seconds(runs, params, out):
    """Estimated remaining wall time of the unfinished runs."""
    total = 0.0
    for run in runs:
        if finished(run, out):
            continue
        scheduled = params["duration_s"] + (0 if run.workload == "probe" else params["warmup_s"])
        total += scheduled + OVERHEAD_S.get(run.tool, DEFAULT_OVERHEAD_S) + (QUICK_SYNC_S if params["quick"] else SYNC_S)
    return total


def format_seconds(seconds):
    seconds = int(seconds)
    return f"{seconds // 3600}h{seconds % 3600 // 60:02d}m{seconds % 60:02d}s"


def scratch_root(out):
    """The runner's own scratch directory under TMPDIR (the filesystem under test), named after OUTDIR.

    Every measurement's data lives below it. A session that was killed can leave data there; the next session for
    the same OUTDIR removes it before starting, so stale data never fills the disk or skews free-space checks."""
    digest = hashlib.sha256(str(out).encode()).hexdigest()[:12]
    return Path(tempfile.gettempdir()) / f"tt-compare-sustained-{digest}"


def reset_scratch(out):
    root = scratch_root(out)
    owner = root / "owner.txt"
    if root.exists():
        if not owner.exists() or owner.read_text() != str(out):
            raise RuntimeError(f"{root} exists but does not belong to {out}; remove it manually")
        shutil.rmtree(root)
    root.mkdir()
    owner.write_text(str(out))
    return root


def write_atomic(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text)
    os.replace(tmp, path)


def validate_histogram(stats, name):
    values = [stats[key] for key in ("min_us", "p50_us", "p90_us", "p99_us", "p99_9_us", "p99_99_us", "max_us")]
    assert all(isinstance(v, (int, float)) and math.isfinite(v) and v >= 0 for v in values), f"{name}: {values}"
    assert values == sorted(values), f"{name} percentiles out of order: {values}"


def validate_sustained(result):
    """Rejects runs with dropped, truncated or unaccounted records and invalid statistics."""
    writer, reader = result["writer"], result.get("reader_result")
    assert writer["count"] > 0 and math.isfinite(writer["achieved_records_per_s"]) \
        and writer["achieved_records_per_s"] > 0, "writer sent nothing"
    if result["workload"] == "probe":
        assert writer["count"] <= writer["planned_count"]
        assert writer["late_records"] is None
    else:
        # Paced runs send every scheduled record: none skipped, none dropped.
        assert writer["count"] == result["count"] == writer["planned_count"], "paced writer did not send every record"
        assert 0 <= writer["late_records"] <= writer["count"]
    if result["workload"] == "SD":
        sync = writer["periodic_sync"]
        assert sync and sync["count"] >= 1, "no periodic sync ran"
        validate_histogram(sync["latency"], "sync latency")
    if reader is None:
        assert not SUPPORT[result["tool"]]["reader"], "reader result missing"
        return
    if result["workload"] == "probe":
        assert reader["count"] == writer["count"], "probe reader did not receive every record"
        latency = reader["latency"]
        assert latency["count"] == writer["count"]
    else:
        latency = reader
        assert latency["count"] == writer["count"] - result["warmup"], "reader did not receive every record"
    validate_histogram(latency, "latency")


def adapter_command(run, role, data, control, count, warmup, rate, params, out=None):
    cmd = [str(HERE / run.tool / "run.sh"), role, "--dir", str(data), "--control", str(control),
           "--count", str(count), "--payload", str(run.payload), "--rate", str(rate or 1),
           "--warmup", str(warmup), "--reader", run.reader,
           "--mode", "sustained_probe" if run.workload == "probe" else "visible_latency"]
    if run.workload == "probe":
        cmd += ["--duration-ms", str(int(params["duration_s"] * 1000))]
    if run.workload == "SD":
        cmd += ["--sync-every-ms", str(params["sync_every_ms"])]
    if run.tool in JAVA_TOOLS:
        cmd = javaopts.wrap(cmd, run.tool, params["java"], params["quick"], True, out, f"{run.label}-{role}")
    return cmd + affinity.adapter_options(params.get("affinity"), run.tool, role)


def parse_one(path):
    lines = path.read_text().splitlines()
    if len(lines) != 1:
        raise RuntimeError(f"expected exactly one JSON line in {path}, got {len(lines)}")
    return json.loads(lines[0])


def run_attempt(run, params, out, env, commands):
    """Runs one measurement and returns its result (not yet validated or written)."""
    import run as short  # The short-mode module: versions and guarantees.
    rate = resolve_rate(run, params, out)
    per_record = record_disk_bytes(run.tool, run.payload)
    tmp_root = scratch_root(out)
    free = shutil.disk_usage(tmp_root).free
    budget = int(free * 0.8) - (1 << 30)
    if run.workload == "probe":
        warmup = 0
        count = budget // per_record if per_record else 1 << 40
        count = min(count, 1 << 40)
        if count < 1000:
            return {"skipped": f"only {free} B free on {tmp_root}"}
    else:
        warmup = math.ceil(rate * params["warmup_s"])
        count = math.ceil(rate * (params["warmup_s"] + params["duration_s"]))
        predicted = count * per_record
        if predicted > budget:
            return {"skipped": f"predicted {predicted} B of data exceeds 80 % of the {free} B free on {tmp_root} "
                               f"minus 1 GiB; use a larger disk or a shorter duration"}
    timeout = params["duration_s"] + params["warmup_s"] + 300
    if run.tool in JAVA_TOOLS:
        timeout += javaopts.WARMUP_ALLOWANCE_S
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    with tempfile.TemporaryDirectory(prefix="run-", dir=tmp_root) as tmp, \
            (out / "children.log").open("a") as error_log, \
            affinity.Keeper(params["affinity"], Path(tmp) / "control") as keeper:
        base = Path(tmp)
        data, control = base / "data", base / "control"
        data.mkdir()
        control.mkdir()
        error_log.write(f"\n{run.label}\n")
        error_log.flush()
        group = reaper.Group()
        handles, outputs = [], {}
        deadline = time.monotonic() + timeout

        def spawn(role):
            cmd = adapter_command(run, role, data, control, count, warmup, rate, params, out)
            commands.write(f"{run.label}: {shlex.join(cmd)}\n")
            commands.flush()
            path = base / f"{role}.json"
            handle = path.open("w")
            handles.append(handle)
            outputs[role] = path
            proc = group.popen(cmd, stdout=handle, stderr=error_log, env=env)
            keeper.watch(proc.pid, role)
            return proc

        def ready(role):
            while not (control / f"{role}.ready").exists():
                for proc in group.procs:
                    if proc.poll() is not None:
                        raise RuntimeError(f"{run.label}: child exited before barrier; see children.log")
                if time.monotonic() > deadline:
                    raise TimeoutError(f"{run.label}: readiness timed out")
                time.sleep(0.005)

        def wait(proc):
            status = group.wait(proc, timeout=max(0.1, deadline - time.monotonic()))
            if status:
                raise RuntimeError(f"{run.label}: child exited {status}; see children.log")

        try:
            writer = spawn("writer")
            if not javaopts.warms_up_before_open(run.tool, params["java"]):
                ready("writer")
            reader = None
            if SUPPORT[run.tool]["reader"]:
                reader = spawn("reader")
                ready("writer")
                ready("reader")
            elif run.tool in JAVA_TOOLS:
                raise RuntimeError("Java adapters need a reader")
            (control / "start.tmp").write_text(str(time.time_ns() + 100_000_000))
            (control / "start.tmp").rename(control / "start")
            if reader is not None:
                wait(reader)
            wait(writer)
            result = {"writer": parse_one(outputs["writer"]),
                      "reader_result": parse_one(outputs["reader"]) if reader is not None else None}
            result["java"] = javaopts.collect(run.tool, params["java"],
                                              {"writer": result["writer"], "reader": result["reader_result"]},
                                              params["quick"], True, True)
            result["affinity"] = affinity.record(params["affinity"], run.tool, result["writer"], result["reader_result"],
                                                 keeper)
        finally:
            group.stop()
            for handle in handles:
                handle.close()
            for path in outputs.values():
                if path.exists():
                    error_log.write(f"{path.name}: {path.read_text()}")
    guarantees = dict(short.GUARANTEES[run.tool])
    guarantees["zero_copy"] = run.tool in JAVA_TOOLS or (run.tool == "tickertape" and run.reader == "mapped")
    result.update(
        label=run.label, tool=run.tool, tool_version=short.VERSIONS[run.tool], guarantees=guarantees,
        mode="sustained", workload=run.workload, payload=run.payload, reader=run.reader,
        concurrent_reader=SUPPORT[run.tool]["reader"], rate_tag=run.rate_tag, rate=rate, count=count,
        warmup=warmup, duration_s=params["duration_s"], warmup_s=0 if run.workload == "probe" else params["warmup_s"],
        sync_every_ms=params["sync_every_ms"] if run.workload == "SD" else None,
        periodic_sync_operation=SUPPORT[run.tool]["SD"] if run.workload == "SD" else None,
        repeat=run.repeat, quick=params["quick"], started_utc=started,
        finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(), data_dir=str(tmp_root),
        free_bytes_before=free,
        coordinated_omission="unpaced probe: latency from send start" if run.workload == "probe"
        else "intended timestamps; no skipped sends or rebasing")
    return result


CLOCK_STEP = "wall clock moved backwards"
CLOCK_RETRIES = 3


def run_one(run, params, out, env, commands, attempt_fn=None):
    """Runs one measurement, retrying runs invalidated by a backwards wall-clock step (recorded in
    retries.jsonl), validates it and writes runs/<label>.json or skipped/<label>.json."""
    attempt_fn = attempt_fn or run_attempt
    log = out / "children.log"
    for attempt in range(1, CLOCK_RETRIES + 2):
        start = log.stat().st_size if log.exists() else 0
        try:
            try:
                result = attempt_fn(run, params, out, env, commands)
            finally:
                os.sync()
            break
        except RuntimeError as error:
            new = ""
            if log.exists():
                with log.open("rb") as f:
                    f.seek(start)
                    new = f.read().decode("utf-8", "replace")
            if CLOCK_STEP not in new or attempt > CLOCK_RETRIES:
                raise
            with (out / "retries.jsonl").open("a") as f:
                f.write(json.dumps({"label": run.label, "attempt": attempt, "reason": CLOCK_STEP,
                                    "error": str(error)}) + "\n")
    if "skipped" in result:
        write_atomic(out / "skipped" / f"{run.label}.json",
                     json.dumps({"label": run.label, "reason": result["skipped"]}) + "\n")
        return "skipped"
    validate_sustained(result)
    write_atomic(out / "runs" / f"{run.label}.json", json.dumps(result, allow_nan=False) + "\n")
    return "done"


def execute(runs, params, out, env, keep_going, attempt_fn=None, log=print):
    """Runs every unfinished run in order. Returns (done, skipped, failed) counts."""
    counts = {"done": 0, "skipped": 0, "failed": 0, "already": 0}
    with (out / "commands.txt").open("a") as commands:
        for index, run in enumerate(runs, 1):
            if finished(run, out):
                counts["already"] += 1
                continue
            remaining = estimate_seconds(runs[index - 1:], params, out)
            log(f"[{index}/{len(runs)}] {run.label} (estimated remaining {format_seconds(remaining)})", file=sys.stderr,
                flush=True)
            try:
                counts[run_one(run, params, out, env, commands, attempt_fn)] += 1
            except (OSError, RuntimeError, AssertionError, subprocess.SubprocessError, ValueError, KeyError,
                    TypeError) as error:
                counts["failed"] += 1
                with (out / "failures.jsonl").open("a") as f:
                    f.write(json.dumps({"label": run.label, "error": f"{type(error).__name__}: {error}",
                                        "time_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}) + "\n")
                if not keep_going:
                    raise
    return counts


def collect(out):
    """Concatenates finished results into results.jsonl (regenerated; runs/ stays the source of truth)."""
    lines = [p.read_text().strip() for p in sorted((out / "runs").glob("*.json"))] if (out / "runs").exists() else []
    write_atomic(out / "results.jsonl", "".join(line + "\n" for line in lines))
    return len(lines)


def parameters(args):
    if args.rates and args.fractions:
        raise ValueError("choose --rates or --rate-fractions, not both")
    fractions = [float(f) for f in (args.fractions or "0.5,0.9").split(",")]
    if any(not 0 < f <= 1 for f in fractions):
        raise ValueError("rate fractions must be in (0, 1]")
    rates = [int(r) for r in args.rates.split(",")] if args.rates else []
    if any(r <= 0 for r in rates):
        raise ValueError("rates must be positive")
    tools = args.tools.split(",") if args.tools else list(DEFAULT_TOOLS)
    workloads = args.workloads.split(",") if args.workloads else list(WORKLOADS)
    payloads = [int(p) for p in (args.payloads or "64,1024").split(",")]
    tt_readers = (args.tt_readers or "mapped").split(",")
    for value, allowed, name in [(tools, DEFAULT_TOOLS, "tool"), (workloads, WORKLOADS, "workload"),
                                 (payloads, (64, 1024), "payload"), (tt_readers, ("safe", "mapped"), "reader")]:
        bad = [v for v in value if v not in allowed]
        if bad or len(set(value)) != len(value):
            raise ValueError(f"invalid or repeated {name}: {bad or value}")
    quick = args.quick
    if quick and not args.payloads:
        payloads = [64]
    duration = args.duration if args.duration is not None else (5 if quick else 600)
    warmup = args.warmup if args.warmup is not None else (1 if quick else 10)
    repeats = args.repeats if args.repeats is not None else (1 if quick else 2)
    if duration <= 0 or warmup < 0 or repeats < 1 or args.sync_every_ms <= 0:
        raise ValueError("duration and sync interval must be positive, warmup non-negative, repeats at least 1")
    paced = [w for w in workloads if w != "probe"]
    if paced and not rates and "probe" not in workloads:
        raise ValueError("derived rates need the probe workload; add probe or pass --rates")
    return {"tools": tools, "workloads": workloads, "payloads": payloads, "tt_readers": tt_readers,
            "duration_s": duration, "warmup_s": warmup, "repeats": repeats, "rates": rates,
            "fractions": [] if rates else fractions, "sync_every_ms": args.sync_every_ms, "quick": quick,
            "needs_probe": "probe" in workloads, "java": javaopts.settings(args)}


def prepare_output(out, params, resume):
    """Creates OUTDIR or checks that a resumed OUTDIR was started with the same matrix."""
    out.mkdir(parents=True, exist_ok=True)
    matrix = out / "matrix.json"
    if any(out.iterdir()):
        if not resume:
            raise ValueError("OUTDIR is not empty; pass --resume to continue the matrix started there")
        if not matrix.exists():
            raise ValueError("OUTDIR has no matrix.json; it was not started by the sustained mode")
        previous = json.loads(matrix.read_text())["parameters"]
        if "affinity" in params:
            # Matrices started before pinning existed were unpinned.
            previous.setdefault("affinity", {"mode": "none", "summary": "affinity: none (--no-pin)"})
        if previous != params:
            raise ValueError(f"OUTDIR was started with different parameters:\n  {previous}\n  now {params}")
        return False
    write_atomic(matrix, json.dumps({"parameters": params, "unsupported": unsupported(params),
                                     "created_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                                     "command": shlex.join(sys.argv)}, indent=2) + "\n")
    return True


def main(args, env, build):
    """Entry point from run.py --sustained."""
    params = parameters(args)
    params["affinity"] = args.affinity
    out = args.outdir.resolve()
    runs = plan(params)
    pending = [r for r in runs if not (out.exists() and finished(r, out))]
    estimate = estimate_seconds(runs, params, out) if out.exists() else estimate_seconds(runs, params, Path("/nonexistent"))
    print(f"sustained matrix: {len(runs)} runs, {len(runs) - len(pending)} already finished; "
          f"estimated remaining {format_seconds(estimate)}")
    print(f"estimated_seconds={int(estimate)}")
    print(params["affinity"]["summary"])
    for case, reason in unsupported(params).items():
        print(f"  unsupported: {case}: {reason}")
    if args.dry_run:
        for run in pending:
            print(f"  {run.label}")
        return
    first = prepare_output(out, params, args.resume)
    # run.main handles SIGTERM, SIGINT and SIGHUP (reaper.install): the session unwinds so that every child process
    # group is ended and reaped; finished results are already on disk, and the matrix resumes. The guard covers
    # SIGKILL of the runner; a resumable OUTDIR is not marked incomplete.
    reaper.start_guard()
    try:
        session(args, params, runs, out, env, build, first)
    finally:
        reaper.stop_guard()


def session(args, params, runs, out, env, build, first):
    import run as short
    scratch = reset_scratch(out)
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    environment = out / ("environment.txt" if first else f"environment-resume-{stamp}.txt")
    short.JAVA.update(params["java"])  # `run` was started as __main__, so this is a second copy of the module.
    short.environment(out, env, params["quick"], environment.name, params["affinity"])
    (out / "skipped").mkdir(exist_ok=True)
    (out / "runs").mkdir(exist_ok=True)
    build(out, env, [t for t in params["tools"] if t in JAVA_TOOLS])
    affinity.place_runner(params["affinity"], *reaper.guard_pids())
    suite_start = time.monotonic()
    counts = execute(runs, params, out, env, args.keep_going)
    total = collect(out)
    shutil.rmtree(scratch, ignore_errors=True)
    with environment.open("a") as f:
        f.write(f"session_elapsed_seconds: {time.monotonic() - suite_start:.3f}\n")
    print(f"done: {out}: {counts['done']} run now, {counts['already']} earlier, {counts['skipped']} skipped, "
          f"{counts['failed']} failed; {total} results in results.jsonl")
    if counts["failed"]:
        raise RuntimeError(f"{counts['failed']} runs failed; see failures.jsonl (rerun with --resume to retry them)")
