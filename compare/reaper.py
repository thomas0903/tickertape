"""Child processes of the comparison runner: each one in a process group of its own, and none left behind.

Every child leads a process group of its own and is ended as a group: SIGTERM, a grace period, then SIGKILL, and the
leader is reaped. The adapters (the JVMs and the Rust adapters) also lead a session of their own, as they did at
v1.0.0; tt-latency stays in the runner's session, as it did at v1.0.0, with only a group of its own (`new_session`).
The runner handles SIGTERM, SIGINT and SIGHUP by unwinding through that cleanup (`install`). A signal that arrives
while a child is being started or while children are being stopped waits until that is done (`deferred`), so no child
is ever started without being tracked, and no cleanup is cut short; later signals are ignored (SIGQUIT and SIGKILL
still stop the runner, and the guard ends the children).

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


def start_time(pid):
    """The process's start time in clock ticks since boot (Linux), None where it cannot be read (macOS, or the
    process is gone). With the PID it names one process for good."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    return int(stat.rsplit(")", 1)[1].split()[19])  # Field 22; the name in field 2 may contain spaces.


def same_leader(pgid, start):
    """False if the PID `pgid` now belongs to another process than the leader that started at `start`. A PID that
    no process holds cannot be the ID of a new group, so the group (if any is left) is still the one recorded. On macOS
    (`start` None) this cannot be told: a leader reaped by the runner just before it was killed, whose number another
    process got within the guard's few seconds, would be signalled. That window remains."""
    if start is None or not Path("/proc").is_dir():
        return True
    now = start_time(pgid)
    return now is None or now == start


def _group_alive(pgid):
    """True while any process is in the group. macOS answers EPERM for a group of zombies only (a zombie with a live
    member is signalled normally), and a zombie member is reaped by init or launchd: neither keeps the group."""
    try:
        os.killpg(pgid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    return True


class Group:
    """The children of one measurement. Each is the leader of its own process group (a JVM, or tt-latency with the
    processes it starts).

    A group is ended as a whole, also when its leader has already exited: every group that may still have a member
    gets SIGTERM, every member gets the grace period (not only the leader), and SIGKILL goes only to the groups that
    are still there after it. A group is known to be empty ("settled") when the runner reaped its leader through
    wait() or communicate() and no member was left at that moment; such a group is not signalled again, since its
    number may by now belong to someone else. A group whose leader was reaped elsewhere (poll() while waiting for a
    barrier, just before a failure stops the measurement) is signalled, as is one that still had members."""

    def __init__(self, guard=None):
        self.guard = guard if guard is not None else _guard
        self.procs = []
        self.settled = set()

    def popen(self, cmd, new_session=True, **kwargs):
        """Starts a child as the leader of a new process group, and of a new session unless `new_session` is False
        (then it stays in the runner's session; Python 3.11 or later)."""
        group = {"start_new_session": True} if new_session else {"process_group": 0}
        with deferred():
            proc = subprocess.Popen([str(x) for x in cmd], **group, **kwargs)
            self.procs.append(proc)
            if self.guard:
                self.guard.add(proc.pid)
        return proc

    def _reaped(self, proc):
        """Right after the runner reaped a leader: if nothing is left in its group, the group is done (also for the
        guard); otherwise it stays to be ended by stop() or, if the runner is killed, by the guard."""
        if not _group_alive(proc.pid):
            self.settled.add(proc.pid)
            if self.guard:
                self.guard.remove(proc.pid)

    def wait(self, proc, timeout=None):
        status = proc.wait(timeout=timeout)
        self._reaped(proc)
        return status

    def communicate(self, proc, timeout=None):
        """proc.communicate(): reads the child's pipes in a blocking wait until it exits. Returns (stdout, stderr)."""
        output = proc.communicate(timeout=timeout)
        self._reaped(proc)
        return output

    def stop(self, grace=GRACE_S):
        """Ends every child that is still running and every process left in a child's group, and reaps the
        leaders."""
        with deferred():
            groups = [p for p in self.procs if p.pid not in self.settled]
            for proc in groups:
                _signal_group(proc.pid, signal.SIGTERM)
            end = time.monotonic() + grace
            for proc in groups:
                try:
                    proc.wait(timeout=max(0.0, end - time.monotonic()))
                except subprocess.TimeoutExpired:
                    pass
            # The leaders are reaped (or past the grace period); the other members get the rest of it.
            while time.monotonic() < end and any(_group_alive(p.pid) for p in groups):
                time.sleep(0.01)
            for proc in groups:
                if proc.poll() is None or _group_alive(proc.pid):
                    _signal_group(proc.pid, signal.SIGKILL)
                proc.wait()
            self.settled.update(p.pid for p in groups)
            if self.guard:
                for proc in self.procs:
                    self.guard.remove(proc.pid)


def identity(path):
    """(st_dev, st_ino) of a directory: which directory it is, whatever is renamed later."""
    info = os.stat(path)
    return info.st_dev, info.st_ino


def mark_incomplete(path, expected=None):
    """Renames an unfinished result directory to <name>.incomplete-N (the rerun driver's convention) and returns the
    new path; None if there is nothing to mark, or if the path is no longer the directory `expected` names (the
    driver has set it aside and started the unit again in a new one)."""
    path = Path(path)
    if not path.is_dir() or not any(path.iterdir()):
        return None
    if expected is not None and identity(path) != tuple(expected):
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
        marked = [str(mark), *map(str, identity(mark))] if mark else []
        try:
            self.proc = subprocess.Popen([sys.executable, "-I", "-S", str(Path(__file__).resolve()), "guard",
                                          str(grace), *marked],
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
        start = start_time(pgid)
        self._send(f"add {pgid}" + ("" if start is None else f" {start}"))

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


def _guard_main(grace, mark, expected):
    groups, released = {}, False  # Group ID: its leader's start time (None on macOS).
    for line in sys.stdin:  # Blocks until the runner writes or goes away.
        word, *args = line.split()
        if word == "add":
            groups[int(args[0])] = int(args[1]) if len(args) > 1 else None
        elif word == "remove":
            groups.pop(int(args[0]), None)
        elif word == "release":
            released = True
    if released:
        return

    def signal_ours(sig):
        for pgid, start in list(groups.items()):
            if same_leader(pgid, start):
                _signal_group(pgid, sig)
            else:
                del groups[pgid]  # Its number now belongs to someone else.
    signal_ours(signal.SIGTERM)
    end = time.monotonic() + grace
    while groups and time.monotonic() < end:
        for pgid in list(groups):
            try:
                os.killpg(pgid, 0)
            except (ProcessLookupError, PermissionError):
                del groups[pgid]
        time.sleep(0.02)
    signal_ours(signal.SIGKILL)
    if mark:
        target = mark_incomplete(mark, expected)
        if target:
            print(f"compare: the runner was killed; its partial results are in {target}", file=sys.stderr)


if __name__ == "__main__" and sys.argv[1:2] == ["guard"]:
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    marked = sys.argv[3:6]
    _guard_main(float(sys.argv[2]), marked[0] if marked else None, tuple(map(int, marked[1:])) if marked else None)
