"""Stand-ins for the tool adapters, for the tests of the runner itself (test_signals.py, test_tools.py).

`standin.py harness RUN-OPTIONS` is compare/run.py with every adapter (and tt-latency) replaced by this file: no build,
no JDK, nothing measured. The adapters get their real command lines, so the runner checks their results as it checks a
tool's. Environment:

  STANDIN_PIDS       directory where every stand-in process (and the runner's guard) leaves a file named by its PID
  STANDIN_HANG       an adapter --mode (or "latency" for tt-latency), or MODE:ROLE: such processes get ready, then
                     sleep
  STANDIN_FAIL       MODE:ROLE: such a process exits 3 after it got ready (before STANDIN_HANG applies)
  STANDIN_GRANDCHILD 1: a hanging process starts a process of its own first (tt-latency's children, a helper).
                     MODE:ROLE: that process starts one, waits until it is set up, then goes on (finishes, fails or
                     hangs), so the member can outlive its leader
  STANDIN_IGNORE_TERM 1: hanging processes ignore SIGTERM
  STANDIN_TERM_DELAY_S seconds a group member takes after SIGTERM before it notes it and exits (default 0): it needs
                     the grace period, not only its leader
  STANDIN_SLACK_S    the runner's time limit beyond a measurement's records (run.TIMEOUT_SLACK_S)
  STANDIN_REAL       comma-separated tools whose adapters are already built and run for real (the JVM test)
"""
import json
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
SLEEP_S = 300  # Bounded, so that even a broken test leaves nothing for long.


def record(kind):
    record_pid(os.getpid(), kind)


def record_pid(pid, kind):
    """<pid>: what the process is. For a stand-in also <pid>.ids: its session, process group and kind of stdout."""
    directory = os.environ.get("STANDIN_PIDS")
    if directory:
        path = Path(directory) / str(pid)
        if pid == os.getpid():
            mode = os.fstat(1).st_mode
            ids = {"sid": os.getsid(0), "pgid": os.getpgid(0),
                   "stdout": "pipe" if stat.S_ISFIFO(mode) else "file" if stat.S_ISREG(mode) else "other"}
            path.with_suffix(".tmp").write_text(json.dumps(ids))
            path.with_suffix(".tmp").rename(path.with_suffix(".ids"))
        path.with_suffix(".tmp").write_text(kind)
        path.with_suffix(".tmp").rename(path)


def options(argv):
    """--name value pairs and repeated --jvm=... flags of an adapter command line."""
    values, jvm, i = {}, [], 0
    while i < len(argv):
        arg = argv[i]
        if arg.startswith("--jvm="):
            jvm.append(arg[len("--jvm="):])
            i += 1
        elif arg.startswith("--") and i + 1 < len(argv):
            values[arg[2:]] = argv[i + 1]
            i += 2
        else:
            i += 1
    return values, jvm


def grandchild(kind, wait=False):
    """A process in this one's group, not waited for: what a tool leaves behind if only its leader is ended."""
    proc = subprocess.Popen([sys.executable, "-I", "-S", str(Path(__file__).resolve()), "sleeper", kind])
    end = time.monotonic() + 60
    while wait and not (Path(os.environ["STANDIN_PIDS"]) / str(proc.pid)).exists() and time.monotonic() < end:
        time.sleep(0.01)  # It records itself once its SIGTERM handler is in place.


def hang(kind):
    if os.environ.get("STANDIN_IGNORE_TERM") == "1":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    if os.environ.get("STANDIN_GRANDCHILD") == "1":
        grandchild(kind)
    (Path(os.environ["STANDIN_PIDS"]) / f"{os.getpid()}.alive").write_text(kind)
    time.sleep(SLEEP_S)
    sys.exit(4)


def sleeper(kind):
    """A member of another stand-in's process group. It notes a SIGTERM (<pid>.term) before it exits, so a test can
    tell that the whole group got one, not only its leader."""
    if os.environ.get("STANDIN_IGNORE_TERM") == "1":
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    else:
        def terminated(signum, frame):
            time.sleep(float(os.environ.get("STANDIN_TERM_DELAY_S", "0")))
            (Path(os.environ["STANDIN_PIDS"]) / f"{os.getpid()}.term").write_text("")
            os._exit(0)
        signal.signal(signal.SIGTERM, terminated)
    record(f"grandchild of {kind}")
    time.sleep(SLEEP_S)


def java_block(tool, values, jvm):
    sys.path.insert(0, str(HERE))
    import javaopts
    block = values.get("block-size", "default")
    return {"warmup": {"mode": values["warmup-mode"], "records": int(values["jit-warmup"])},
            "tool": {"block_size": javaopts.CHRONICLE_DEFAULT_BLOCK_SIZE if block == "default" else int(block),
                     "micro_touch": values.get("micro-touch") == "on", "pre_touch": values.get("pre-touch") == "on",
                     "sparse_terms": values.get("sparse-terms") == "on"},
            "jvm_arguments": jvm}


def adapter(tool, role, argv):
    record(f"{tool} {role}")
    values, jvm = options(argv)
    mode, count, payload = values["mode"], int(values["count"]), int(values["payload"])
    if os.environ.get("STANDIN_GRANDCHILD") == f"{mode}:{role}":
        grandchild(f"{tool} {role} {mode}", wait=True)
    (Path(values["control"]) / f"{role}.ready").write_text("")
    if os.environ.get("STANDIN_FAIL") == f"{mode}:{role}":
        sys.exit(3)
    if os.environ.get("STANDIN_HANG") in (mode, f"{mode}:{role}"):
        hang(f"{tool} {role} {mode}")
    if mode == "visible_latency":  # Both processes wait for the runner's start, as the adapters do.
        end = time.monotonic() + 60
        while not (Path(values["control"]) / "start").exists() and time.monotonic() < end:
            time.sleep(0.002)
    if mode == "visible_latency" and role == "writer":
        result = {"count": count, "late_records": 0, "achieved_records_per_s": float(values["rate"])}
    elif mode == "visible_latency":
        result = {"count": count - int(values["warmup"]), "min_us": 1.0, "p50_us": 2.0, "p90_us": 3.0, "p99_us": 4.0,
                  "p99_9_us": 5.0, "p99_99_us": 6.0, "max_us": 7.0}
    else:
        result = {"count": count, "records_per_s": 1e6, "payload_mb_per_s": 1e6 * payload / 1e6}
    result["affinity"] = {"standin": True}
    if tool in ("chronicle", "aeron"):
        result["java"] = java_block(tool, values, jvm)
    print(json.dumps(result))


def latency(argv):
    """tt-latency: starts a writer and a reader process of its own (same process group) and reports for both."""
    record("tt-latency")
    values, _ = options(argv)
    if os.environ.get("STANDIN_HANG") == "latency":
        for role in ("writer", "reader"):
            subprocess.Popen([sys.executable, "-I", "-S", str(Path(__file__).resolve()), "sleeper",
                              f"tt-latency {role}"])
        hang("tt-latency")
    count, warmup = int(values["count"]), int(values["warmup"])
    print(json.dumps({"latency": {"count": count - warmup, "min_us": 1.0, "p50_us": 2.0, "p90_us": 3.0, "p99_us": 4.0,
                                  "p99_9_us": 5.0, "p99_99_us": 6.0, "max_us": 7.0},
                      "writer": {"count": count, "late_records": 0, "achieved_records_per_s": float(values["rate"])},
                      "affinity": {"writer": {"standin": True}, "reader": {"standin": True}}}))


def harness(argv):
    """compare/run.py's __main__ with stand-ins; started as an interactive shell would start it."""
    signal.signal(signal.SIGINT, signal.default_int_handler)
    signal.signal(signal.SIGHUP, signal.SIG_DFL)
    sys.path.insert(0, str(HERE))
    import reaper
    import run
    me = [sys.executable, "-I", "-S", str(Path(__file__).resolve())]
    original = run.command
    real = os.environ.get("STANDIN_REAL", "").split(",")  # Prebuilt adapters that run as themselves.
    run.command = lambda tool, *a, **k: (original(tool, *a, **k) if tool in real
                                         else [*me, "adapter", tool, *original(tool, *a, **k)[1:]])
    original_latency = run.latency_command
    run.latency_command = lambda *a: [*me, "latency", *original_latency(*a)[1:]]
    run.build = lambda *a, **k: None
    run.capture = lambda command: "stand-in"
    run.os.sync = lambda: None
    if "STANDIN_SLACK_S" in os.environ:
        run.TIMEOUT_SLACK_S = float(os.environ["STANDIN_SLACK_S"])
    os.environ.setdefault("JAVA_HOME", "/nonexistent-jdk")
    start_guard = reaper.start_guard

    def recorded_guard(*a, **k):
        guard = start_guard(*a, **k)
        record_pid(guard.pid, "guard")
        return guard
    reaper.start_guard = recorded_guard
    sys.argv = [str(HERE / "run.py"), *argv]
    run.cli()


if __name__ == "__main__":
    command, rest = sys.argv[1], sys.argv[2:]
    if command == "harness":
        harness(rest)
    elif command == "adapter":
        # run.sh ROLE ... or, for the JVM tools, run.sh --jvm=... ROLE ...
        position = next(i for i, a in enumerate(rest) if i and not a.startswith("--jvm="))
        adapter(rest[0], rest[position], rest[1:position] + rest[position + 1:])
    elif command == "latency":
        latency(rest[1:])
    elif command == "sleeper":
        sleeper(rest[0])
    else:
        sys.exit(f"standin: unknown command {command}")
