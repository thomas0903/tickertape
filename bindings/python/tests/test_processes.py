"""Behaviour across processes and signals: live following, durable-only readers, Ctrl-C, signal handlers and fork."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern, pattern_tuple

FOLLOWER = textwrap.dedent("""
    import hashlib, json, sys
    import tickertape as tt

    path, count, durable = sys.argv[1], int(sys.argv[2]), sys.argv[3] == "1"
    reader = tt.Reader(path, durable_only=durable)
    print("ready", flush=True)
    seen, digest = [], hashlib.sha256()
    while len(seen) < count:
        batch = reader.read_many(count - len(seen), timeout=10)
        if not batch:
            break
        for r in batch:
            seen.append(r.seq)
            digest.update(repr(r.as_tuple()).encode())
    print(json.dumps({"seqs": seen, "digest": digest.hexdigest(), "position": reader.position}), flush=True)
""")


def expected_digest(seqs: range) -> str:
    import hashlib
    digest = hashlib.sha256()
    for s in seqs:
        digest.update(repr(pattern_tuple(s)).encode())
    return digest.hexdigest()


def start_follower(journal: Path, count: int, durable: bool = False) -> subprocess.Popen[str]:
    proc = subprocess.Popen([sys.executable, "-c", FOLLOWER, str(journal), str(count), "1" if durable else "0"],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    assert proc.stdout is not None
    assert proc.stdout.readline().strip() == "ready", proc.stderr.read() if proc.stderr else ""
    return proc


def finish(proc: subprocess.Popen[str]) -> dict[str, object]:
    out, err = proc.communicate(timeout=60)
    assert proc.returncode == 0, err
    result: dict[str, object] = json.loads(out)
    return result


def test_live_follow_from_another_process(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        follower = start_follower(journal, 3000)
        for _ in range(30):  # paced, across many segment rolls
            append_pattern(w, 100)
            time.sleep(0.005)
        result = finish(follower)
    assert result["seqs"] == list(range(3000))
    assert result["digest"] == expected_digest(range(3000))
    assert result["position"] == 3000


def test_durable_only_follower_in_another_process(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 400)
        w.checkpoint()
        append_pattern(w, 400)  # visible, not durable
        follower = start_follower(journal, 800, durable=True)
        time.sleep(0.5)
        assert follower.poll() is None  # still waiting at the checkpoint boundary
        w.checkpoint()
        result = finish(follower)
    assert result["seqs"] == list(range(800))
    assert result["digest"] == expected_digest(range(800))


INTERRUPTIBLE = textwrap.dedent("""
    import sys, time
    import tickertape as tt

    reader = tt.Reader(sys.argv[1])
    reader.seek(1)  # record 0 exists; wait for record 1
    call = sys.argv[2]
    print("ready", flush=True)
    started = time.monotonic()
    try:
        if call == "next":
            reader.next()
        elif call == "read_many":
            reader.read_many(10, timeout=None)
        else:
            reader.read_batch(10, timeout=3600)
    except KeyboardInterrupt:
        print(f"interrupted {time.monotonic() - started:.3f} {reader.position}", flush=True)
        sys.exit(0)
    print("returned", flush=True)
    sys.exit(1)
""")


@pytest.mark.parametrize("call", ["next", "read_many", "read_batch"])
def test_ctrl_c_interrupts_a_blocking_read(journal: Path, call: str) -> None:
    with tt.Writer(journal) as w:
        w.append(b"zero")
        proc = subprocess.Popen([sys.executable, "-c", INTERRUPTIBLE, str(journal), call],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        assert proc.stdout is not None
        assert proc.stdout.readline().strip() == "ready"
        time.sleep(0.3)  # let it block
        sent = time.monotonic()
        proc.send_signal(signal.SIGINT)
        out, err = proc.communicate(timeout=10)
        elapsed = time.monotonic() - sent
    assert proc.returncode == 0, (out, err)
    word, _, position = out.split()
    assert word == "interrupted"
    assert position == "1"
    assert elapsed < 5


def test_ctrl_c_consumes_nothing(journal: Path) -> None:
    script = textwrap.dedent("""
        import sys
        import tickertape as tt
        reader = tt.Reader(sys.argv[1])
        reader.seek(1)
        print("ready", flush=True)
        try:
            reader.next()
        except KeyboardInterrupt:
            pass
        print(reader.position, flush=True)
        record = reader.next(timeout=10)
        print(record.seq, record.payload.decode(), flush=True)
    """)
    with tt.Writer(journal) as w:
        w.append(b"zero")
        proc = subprocess.Popen([sys.executable, "-c", script, str(journal)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        assert proc.stdout is not None
        assert proc.stdout.readline().strip() == "ready"
        time.sleep(0.3)
        proc.send_signal(signal.SIGINT)
        assert proc.stdout.readline().strip() == "1"
        w.append(b"one")
        out, err = proc.communicate(timeout=10)
    assert proc.returncode == 0, err
    assert out.strip() == "1 one"


def test_second_writer_in_another_process_is_refused(journal: Path) -> None:
    script = "import sys, tickertape as tt\ntry:\n    tt.Writer(sys.argv[1])\nexcept tt.LockedError as e:\n" \
             "    print('locked', e.pid)\n"
    with tt.Writer(journal):
        out = subprocess.check_output([sys.executable, "-c", script, str(journal)], text=True)
    assert out.split()[0] == "locked"
    assert out.split()[1] == str(os.getpid())


def run_script(script: str, *args: object, timeout: float = 30) -> str:
    """Runs a Python script in a fresh interpreter; fails with its output if it exits non-zero or hangs."""
    proc = subprocess.run([sys.executable, "-c", textwrap.dedent(script), *map(str, args)], capture_output=True,
                          text=True, timeout=timeout)
    assert proc.returncode == 0, (proc.stdout, proc.stderr)
    return proc.stdout


# A record arrives while the read waits, and a signal is pending when it resumes: the record must not be lost. A
# helper thread sends SIGINT to the process and immediately appends; the main thread is blocked in the read.
INTERRUPTED_WITH_A_RECORD = """
    import os, signal, sys, threading, time
    import tickertape as tt

    path, call = sys.argv[1], sys.argv[2]
    writer = tt.Writer(path, segment_size=1 << 20)
    writer.append(b"zero")
    reader = tt.Reader(path)
    reader.seek(1)

    def interrupt_and_append():
        time.sleep(0.2)
        os.kill(os.getpid(), signal.SIGINT)
        writer.append(b"one")

    for attempt in range(3):
        threading.Thread(target=interrupt_and_append).start()
        try:
            if call == "next":
                got = reader.next()
            elif call == "read_many":
                got = reader.read_many(10, timeout=None)
            else:
                got = reader.read_batch(10, timeout=None)
            print("returned", got)
            sys.exit(1)
        except KeyboardInterrupt:
            pass
        assert reader.position == 1 + attempt, reader.position
        record = reader.next(timeout=5)
        assert record is not None and (record.seq, record.payload) == (1 + attempt, b"one"), record
        assert reader.position == 2 + attempt
    print("ok")
"""


@pytest.mark.parametrize("call", ["next", "read_many", "read_batch"])
def test_interrupt_keeps_a_record_that_arrived_during_the_wait(journal: Path, call: str) -> None:
    assert run_script(INTERRUPTED_WITH_A_RECORD, journal, call).strip() == "ok"


def test_signal_handler_that_closes_the_reader_ends_the_read(journal: Path) -> None:
    script = """
        import os, signal, sys, threading, time
        import tickertape as tt

        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        reader = tt.Reader(path)
        signal.signal(signal.SIGUSR1, lambda *_: reader.close())
        threading.Timer(0.2, os.kill, (os.getpid(), signal.SIGUSR1)).start()
        started = time.monotonic()
        try:
            reader.next()
        except ValueError as error:
            print(f"{error} {time.monotonic() - started:.2f}")
        assert reader.closed
    """
    out = run_script(script, journal, timeout=20)
    assert out.startswith("the reader was closed while this read was waiting"), out
    assert float(out.split()[-1]) < 2


def test_signal_handler_can_use_the_reader_during_a_read(journal: Path) -> None:
    script = """
        import os, signal, sys, threading
        import tickertape as tt

        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        writer.append(b"zero")
        reader = tt.Reader(path)
        reader.seek(1)
        seen = []

        def handler(*_):
            seen.append((reader.position, reader.closed, repr(reader), reader.try_next()))
            writer.append(b"one")

        signal.signal(signal.SIGUSR1, handler)
        threading.Timer(0.2, os.kill, (os.getpid(), signal.SIGUSR1)).start()
        record = reader.next(timeout=10)
        print(seen[0][:2], "position=1" in seen[0][2], seen[0][3], record.seq)
    """
    assert run_script(script, journal, timeout=20).split() == ["(1,", "False)", "True", "None", "1"]


def test_close_from_another_thread_ends_a_read_that_waits_forever(journal: Path) -> None:
    script = """
        import sys, threading, time
        import tickertape as tt

        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        reader = tt.Reader(path)
        outcome = []

        def read():
            try:
                outcome.append(reader.next(timeout=None))
            except ValueError as error:
                outcome.append(error)

        thread = threading.Thread(target=read)
        thread.start()
        time.sleep(0.2)
        # Neither closed, repr nor position waits for the blocked read.
        started = time.monotonic()
        for _ in range(20):
            assert not reader.closed and "position=0" in repr(reader) and reader.position == 0
        quick = time.monotonic() - started
        started = time.monotonic()
        reader.close()
        thread.join(5)
        print(f"{quick:.3f} {time.monotonic() - started:.3f} {thread.is_alive()} {outcome[0]}")
    """
    quick, close, alive, *message = run_script(script, journal, timeout=20).split()
    assert float(quick) < 0.5 and float(close) < 1 and alive == "False"
    assert " ".join(message) == "the reader was closed while this read was waiting"


def test_another_thread_reads_while_one_waits(journal: Path) -> None:
    """A read in a second thread proceeds while the first waits, and takes the record it finds."""
    script = """
        import sys, threading, time
        import tickertape as tt

        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        reader = tt.Reader(path)
        waited = []
        thread = threading.Thread(target=lambda: waited.append(reader.next(timeout=10)))
        thread.start()
        time.sleep(0.2)
        started = time.monotonic()
        nothing = reader.try_next()
        took = time.monotonic() - started
        writer.append(b"a")
        thread.join(10)
        writer.append(b"b")
        print(nothing, f"{took:.3f}", waited[0].payload, reader.try_next().payload)
    """
    nothing, took, first, second = run_script(script, journal, timeout=20).split()
    assert nothing == "None" and float(took) < 0.5 and (first, second) == ("b'a'", "b'b'")


# A handle copied into a child by fork() must not touch the journal there. The child writes its results as JSON lines.
FORKED = """
    import gc, json, os, sys, warnings
    import tickertape as tt

    warnings.simplefilter("ignore", DeprecationWarning)  # "use of fork() may lead to deadlocks" (3.12+)
    path, prefault = sys.argv[1], sys.argv[2] == "1"
    writer = tt.Writer(path, segment_size=1 << 20, prefault=prefault)
    writer.append(b"zero")
    reader = tt.Reader(path)
    pid = os.fork()
    if pid == 0:
        results = {}
        for name, call in [
            ("append", lambda: writer.append(b"child")),
            ("append_many", lambda: writer.append_many([b"child"])),
            ("checkpoint", writer.checkpoint),
            ("next_seq", lambda: writer.next_seq),
            ("recovery_report", lambda: writer.recovery_report),
            ("writer_enter", writer.__enter__),
            ("try_next", reader.try_next),
            ("next", lambda: reader.next(timeout=0)),
            ("read_many", lambda: reader.read_many(10)),
            ("seek", lambda: reader.seek(0)),
            ("position", lambda: reader.position),
            ("iter", lambda: iter(reader)),
        ]:
            try:
                call()
                results[name] = "returned"
            except tt.UsedAfterForkError as error:
                assert isinstance(error, tt.TickertapeError)
                assert (error.owner_pid, error.pid) == (os.getppid(), os.getpid()), error
                results[name] = "refused"
        results["repr"] = repr(writer)
        # Leaving a with block entered before the fork, closing and garbage collection discard the copies without
        # touching the parent's.
        writer.__exit__(None, None, None)
        reader.__exit__(None, None, None)
        writer.close()
        reader.close()
        results["closed"] = writer.closed and reader.closed
        del writer, reader
        gc.collect()
        print(json.dumps(results), flush=True)
        os._exit(0)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, status
    print(json.dumps({"parent_append": writer.append(b"parent"), "parent_next": reader.try_next().seq}), flush=True)
    writer.checkpoint()
    writer.close()
    reader.close()
    with tt.Writer(path) as again:  # the lock was released by the parent's close, not the child's
        again.append(b"after")
    print(json.dumps([(r.seq, r.payload.decode()) for r in tt.Reader(path)]), flush=True)
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
@pytest.mark.parametrize("prefault", [False, True], ids=["prefault=False", "prefault=True"])
def test_handles_refuse_use_after_fork(journal: Path, prefault: bool) -> None:
    child, parent, records = map(json.loads, run_script(FORKED, journal, int(prefault)).splitlines())
    calls = [name for name in child if name not in ("repr", "closed")]
    assert {name: child[name] for name in calls} == dict.fromkeys(calls, "refused")
    assert "opened in process" in child["repr"] and child["closed"] is True
    assert parent == {"parent_append": 1, "parent_next": 0}
    assert records == [[0, "zero"], [1, "parent"], [2, "after"]]
    assert tt.verify(journal).ok
