"""Child processes of the comparison runner: each one in a process group of its own, and none left behind.

Every tool process starts in a new session (so a terminal's Ctrl-C or hang-up reaches the runner, not the tools) and
is ended as a group: SIGTERM, a grace period, then SIGKILL, and the leader is reaped. The runner handles SIGTERM,
SIGINT and SIGHUP by unwinding through that cleanup (`install`). A signal that arrives while a child is being
started or while children are being stopped waits until that is done (`deferred`), so no child is ever started
without being tracked, and no cleanup is cut short; a second signal during the cleanup is ignored.

SIGKILL of the runner cannot be handled in the runner. For that case a `Guard` process waits, blocked on a pipe,
for the runner to go away: if the pipe closes without the runner having said "release", the guard ends every group
the runner still had and marks the unfinished result directory. It runs no code while the runner is alive except to
record a group when a child starts or ends, outside any measured loop, on the runner's (helper) CPUs.

Standard library only; Linux and macOS.
"""
import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

GRACE_S = 3.0
SIGNALS = (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)


class Terminated(SystemExit):
    """Raised in the runner by SIGTERM, SIGINT or SIGHUP; the exit status is the shell's 128 + signal."""

    def __init__(self, signum):
        super().__init__(128 + signum)
        self.signum = signum

    def __str__(self):
        return f"terminated by {signal.Signals(self.signum).name}"


_state = {"defer": 0, "pending": None, "raised": False}


def _handler(signum, frame):
    if _state["raised"]:
        return  # The runner is already unwinding; its cleanup finishes first.
    if _state["defer"]:
        _state["pending"] = _state["pending"] or signum
        return
    _state["raised"] = True
    raise Terminated(signum)


def install():
    """Handles SIGTERM, SIGINT and SIGHUP in this (main) thread, except signals this process was started ignoring
    (nohup)."""
    _state.update(defer=0, pending=None, raised=False)
    for sig in SIGNALS:
        if signal.getsignal(sig) != signal.SIG_IGN:
            signal.signal(sig, _handler)


@contextlib.contextmanager
def deferred():
    """Holds back a signal until the block is done, then raises it."""
    _state["defer"] += 1
    try:
        yield
    finally:
        _state["defer"] -= 1
        if not _state["defer"] and _state["pending"] is not None and not _state["raised"]:
            _state["raised"] = True
            raise Terminated(_state["pending"])


def _signal_group(pgid, sig):
    try:
        os.killpg(pgid, sig)
    except (ProcessLookupError, PermissionError):
        pass  # The group is gone (macOS answers EPERM for a group of zombies).


class Group:
    """The children of one measurement. Each is the leader of its own process group (a JVM, or tt-latency with the
    processes it starts)."""

    def __init__(self, guard=None):
        self.guard = guard if guard is not None else _guard
        self.procs = []

    def popen(self, cmd, **kwargs):
        with deferred():
            proc = subprocess.Popen([str(x) for x in cmd], start_new_session=True, **kwargs)
            self.procs.append(proc)
            if self.guard:
                self.guard.add(proc.pid)
        return proc

    def wait(self, proc, timeout=None):
        status = proc.wait(timeout=timeout)
        if self.guard:
            self.guard.remove(proc.pid)
        return status

    def stop(self, grace=GRACE_S):
        """Ends every child that is still running and what it started in its group, and reaps it."""
        with deferred():
            live = [p for p in self.procs if p.poll() is None]
            for proc in live:
                _signal_group(proc.pid, signal.SIGTERM)
            end = time.monotonic() + grace
            for proc in live:
                try:
                    proc.wait(timeout=max(0.0, end - time.monotonic()))
                except subprocess.TimeoutExpired:
                    pass
            for proc in live:
                # The leader if it outlived the grace period, and anything it left in its group.
                _signal_group(proc.pid, signal.SIGKILL)
                proc.wait()
            if self.guard:
                for proc in self.procs:
                    self.guard.remove(proc.pid)


def mark_incomplete(path):
    """Renames an unfinished result directory to <name>.incomplete-N (the rerun driver's convention) and returns the
    new path; None if there is nothing to mark."""
    path = Path(path)
    if not path.is_dir() or not any(path.iterdir()):
        return None
    n = 1
    while (target := path.with_name(f"{path.name}.incomplete-{n}")).exists():
        n += 1
    path.rename(target)
    return target


class Guard:
    """The watchdog for SIGKILL of the runner (see the module docstring). One per session."""

    def __init__(self, grace=GRACE_S, mark=None):
        read, write = os.pipe()
        try:
            self.proc = subprocess.Popen([sys.executable, "-I", "-S", str(Path(__file__).resolve()), "guard",
                                          str(grace), *([str(mark)] if mark else [])],
                                         stdin=read, stdout=subprocess.DEVNULL, start_new_session=True)
        finally:
            os.close(read)
        self.pid = self.proc.pid
        self.pipe = os.fdopen(write, "w", buffering=1)  # Not inherited by children (PEP 446).
        self.lost = False

    def _send(self, line):
        try:
            self.pipe.write(line + "\n")
        except (BrokenPipeError, ValueError):
            if not self.lost:
                print("compare: the guard process has ended; SIGKILL of the runner would leave children",
                      file=sys.stderr)
            self.lost = True

    def add(self, pgid):
        self._send(f"add {pgid}")

    def remove(self, pgid):
        self._send(f"remove {pgid}")

    def close(self):
        """The runner is leaving normally: its children are stopped and its output is complete or marked."""
        with deferred():
            self._send("release")
            try:
                self.pipe.close()
            except BrokenPipeError:
                pass
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()


_guard = None


def start_guard(grace=GRACE_S, mark=None):
    """Starts the session's guard; Group() reports to it from then on."""
    global _guard
    _guard = Guard(grace, mark)
    return _guard


def guard_pids():
    return [_guard.pid] if _guard is not None else []


def stop_guard():
    global _guard
    if _guard is not None:
        _guard.close()
        _guard = None


def _guard_main(grace, mark):
    groups, released = set(), False
    for line in sys.stdin:  # Blocks until the runner writes or goes away.
        word, _, arg = line.strip().partition(" ")
        if word == "add":
            groups.add(int(arg))
        elif word == "remove":
            groups.discard(int(arg))
        elif word == "release":
            released = True
    if released:
        return
    for pgid in groups:
        _signal_group(pgid, signal.SIGTERM)
    end = time.monotonic() + grace
    while groups and time.monotonic() < end:
        for pgid in list(groups):
            try:
                os.killpg(pgid, 0)
            except (ProcessLookupError, PermissionError):
                groups.discard(pgid)
        time.sleep(0.02)
    for pgid in groups:
        _signal_group(pgid, signal.SIGKILL)
    if mark:
        target = mark_incomplete(mark)
        if target:
            print(f"compare: the runner was killed; its partial results are in {target}", file=sys.stderr)


if __name__ == "__main__" and sys.argv[1:2] == ["guard"]:
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    _guard_main(float(sys.argv[2]), sys.argv[3] if len(sys.argv) > 3 else None)
