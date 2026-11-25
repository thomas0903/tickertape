"""Behaviour across processes and signals: live following, durable-only readers, Ctrl-C, signal handlers and fork."""

from __future__ import annotations

import json
import os
import signal
import subprocess
import sys
import textwrap
import threading
import time
from collections.abc import Callable
from pathlib import Path

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern, needs_test_hooks, pattern_tuple

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
            ("append_batch", lambda: writer.append_batch([b"child"])),
            ("checkpoint", writer.checkpoint),
            ("next_seq", lambda: writer.next_seq),
            ("recovery_report", lambda: writer.recovery_report),
            ("writer_enter", writer.__enter__),
            ("try_next", reader.try_next),
            ("next", lambda: reader.next(timeout=0)),
            ("read_many", lambda: reader.read_many(10)),
            ("seek", lambda: reader.seek(0)),
            ("seek_to_end", reader.seek_to_end),
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


# A signal whose handler raises arrives at a random moment of a read that returns a large result, often while the
# result is being built. The records must either reach the caller or stay staged for the next read. A timer signal
# (SIGALRM) is used because the kernel delivers it while the reading thread holds the GIL, which a Python thread
# could not do. The handler may also run after the read's final signal check, once the call has returned and before
# Python stores the result: that loses the value, as documented, and may happen any number of times. Every attempt
# checks what is guaranteed, whenever the signal arrives: if the read raised, it consumed nothing (the cursor is still
# at 0, as it was when the handler ran, and the next read returns all the records); if it returned, exactly the
# returned records were consumed (the cursor is at the end, and was when a late handler ran).
#
# What this test cannot tell apart: a handler that ran after the read returned (before Python stored the result) and
# one that ran inside the read after it delivered its records (which a binding that checked signals after the
# delivery would do). Both see the final cursor with nothing stored, and the interpreter runs handlers between a
# call's return and the assignment of its result, so nothing visible from Python separates them. This test checks the
# user-visible guarantee; `test_nothing_runs_python_code_between_the_delivery_and_the_return` (test hooks) checks the
# other half: that a read runs no signal handler and no other Python code from the delivery to its return.
INTERRUPTED_WHILE_BUILDING = """
    import random, signal, sys, time
    import tickertape as tt

    path, call = sys.argv[1], sys.argv[2]
    if call == "next":
        count, size, segment = 1, 24 << 20, 64 << 20
    elif call == "read_many":
        count, size, segment = 200_000, 16, 32 << 20
    else:
        count, size, segment = 20_000, 1024, 64 << 20
    with tt.Writer(path, segment_size=segment) as writer:
        writer.append_batch([i.to_bytes(8, "little") * (size // 8) for i in range(count)])
    reader = tt.Reader(path)

    def read():
        if call == "next":
            return reader.next(timeout=5)
        if call == "read_many":
            return reader.read_many(count, timeout=5)
        return reader.read_batch(count, timeout=5)

    def check(result):
        records = [result] if call == "next" else result.records() if call == "read_batch" else result
        assert [r.seq for r in records] == list(range(count)), (len(records), records[:1])
        for r in records[:: max(1, count // 100)]:
            assert r.payload == r.seq.to_bytes(8, "little") * (size // 8)

    class Interrupt(Exception):
        pass

    armed, seen = [False], []

    def handler(signum, frame):
        if armed[0]:
            armed[0] = False
            seen.append(reader.position)
            raise Interrupt

    signal.signal(signal.SIGALRM, handler)
    durations = []
    for _ in range(5):
        reader.seek(0)
        started = time.perf_counter()
        result = read()
        durations.append(time.perf_counter() - started)
        check(result)
        del result
    # Aim at the later part of the call, where the result is built. The first calls are slower, and the timing
    # drifts on a shared machine, so the target follows the latest uninterrupted calls.
    staged = after_return = delivered = attempts = 0
    while staged < 10 and attempts < 300:
        attempts += 1
        duration = min(durations[-5:])
        reader.seek(0)
        box = []
        seen.clear()
        try:
            armed[0] = True
            started = time.perf_counter()
            signal.setitimer(signal.ITIMER_REAL, random.uniform(0.2 * duration, 1.0 * duration))
            box.append(read())
            armed[0] = False
            took = time.perf_counter() - started
            signal.setitimer(signal.ITIMER_REAL, 0)
        except Interrupt:
            signal.setitimer(signal.ITIMER_REAL, 0)
            position = reader.position
            if box:
                assert position == count, position  # the handler ran after the result was stored
                delivered += 1
            elif position != 0:
                # The read returned and the handler raised before Python stored the result (after the final signal
                # check): the returned records were consumed, and the value is lost. (A handler that ran inside the
                # read after its delivery would look the same; the test-hooks test named above rules that out.)
                assert position == count and seen == [count], (position, seen)
                after_return += 1
                continue
            else:
                # The read raised: it consumed nothing, and the next read returns its records.
                assert seen == [0], seen
                staged += 1
                box.append(read())
        else:
            assert reader.position == count, reader.position
            delivered += 1
            durations.append(took)
        check(box[0])
    print(staged, after_return, delivered)
"""


@pytest.mark.parametrize("call", ["next", "read_many", "read_batch"])
def test_interrupt_while_the_result_is_built_keeps_the_records(journal: Path, call: str) -> None:
    staged, after_return, delivered = map(int, run_script(INTERRUPTED_WHILE_BUILDING, journal, call,
                                                          timeout=120).split())
    # Any number of late signals (`after_return`) is allowed; the script checked every attempt.
    assert staged >= 10, (staged, after_return, delivered)  # the test did interrupt reads


def test_signal_handler_reading_during_iteration_does_not_end_it(journal: Path) -> None:
    """A handler that takes records while a ``for`` loop iterates must not make the loop stop early."""
    script = """
        import signal, sys
        import tickertape as tt

        path, count = sys.argv[1], 50_000
        with tt.Writer(path, segment_size=16 << 20) as writer:
            writer.append_batch([b"%d" % i for i in range(count)])
        reader = tt.Reader(path)
        taken_by_handler = []

        def handler(*_):
            record = reader.try_next()
            if record is not None:
                taken_by_handler.append(record.seq)

        signal.signal(signal.SIGALRM, handler)
        signal.setitimer(signal.ITIMER_REAL, 0.0001, 0.0001)
        looped = [record.seq for record in reader]
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, signal.SIG_IGN)
        assert sorted(looped + taken_by_handler) == list(range(count)), (len(looped), len(taken_by_handler))
        print(len(looped), len(taken_by_handler))
    """
    looped, by_handler = map(int, run_script(script, journal, timeout=60).split())
    assert looped + by_handler == 50_000 and by_handler > 0


def test_non_blocking_reads_in_two_threads_never_return_empty_early(journal: Path) -> None:
    """``read_many(timeout=0)`` returns [] only when no committed record remains, even if another thread takes the
    records it found staged."""
    count = 10_000
    with tt.Writer(journal, segment_size=SMALL_SEGMENT * 64) as w:
        w.append_batch([b"%d" % i for i in range(count)])
    for _ in range(10):
        reader = tt.Reader(journal)
        results: dict[int, list[int]] = {0: [], 1: []}
        premature: list[int] = []

        def consume(me: int) -> None:
            while True:
                got = reader.read_many(10, timeout=0)
                if not got:
                    position = reader.position
                    if position < count:
                        premature.append(position)
                    return
                results[me].extend(r.seq for r in got)

        threads = [threading.Thread(target=consume, args=(i,)) for i in results]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(30)
        assert premature == [], premature
        assert sorted(results[0] + results[1]) == list(range(count))
        reader.close()


@pytest.mark.parametrize("call", ["next", "read_many", "read_batch"])
@pytest.mark.parametrize("closer", ["signal", "thread"])
def test_reader_closed_at_the_deadline_raises(journal: Path, call: str, closer: str) -> None:
    """A reader closed (by a signal handler or another thread) during the wait slice in which the deadline passes
    makes the read raise ValueError instead of reporting a timeout."""
    script = """
        import signal, sys, threading, time
        import tickertape as tt

        path, call, closer = sys.argv[1:4]
        writer = tt.Writer(path, segment_size=1 << 20)
        raised = late = 0
        while raised < 5:
            reader = tt.Reader(path)
            closed_at = []

            def close(*_, reader=reader, closed_at=closed_at):
                reader.close()
                closed_at.append(time.monotonic())  # the close had finished by this time

            if closer == "signal":
                signal.signal(signal.SIGALRM, close)
                signal.setitimer(signal.ITIMER_REAL, 0.01)
            else:
                threading.Timer(0.01, close).start()
            try:
                # One wait slice (the deadline passes during it); the reader is closed 10 ms into it.
                if call == "next":
                    result = reader.next(timeout=0.05)
                elif call == "read_many":
                    result = reader.read_many(10, timeout=0.05)
                else:
                    result = reader.read_batch(10, timeout=0.05)
            except ValueError as error:
                assert "closed" in str(error), error
                raised += 1
                continue
            returned_at = time.monotonic()
            if closed_at and closed_at[0] < returned_at - 0.005:
                # The close had finished well before the read returned, yet the read reported a timeout.
                print("returned", repr(result), "after a close that finished", returned_at - closed_at[0], "s earlier")
                sys.exit(1)
            # The close came after the read had returned, or so close to it that the order cannot be told (the timer
            # or thread ran about 40 ms late on a loaded machine), so the timeout was right. Wait for it and try again.
            late += 1
            assert late < 200, "the closer kept running late"
            # Not signal.pause(): the handler can run between the check and the pause, and no other signal follows.
            while not reader.closed:
                time.sleep(0.001)
        print("ok")
    """
    assert run_script(script, journal, call, closer).strip() == "ok"


def test_a_read_behind_a_waiting_one_does_not_wait_past_its_deadline(journal: Path) -> None:
    """A 40 ms read queued behind another waiter (which holds the reader for up to one 50 ms slice) measures its
    remaining wait once it has the reader, instead of waiting its whole timeout again (about 90 ms)."""
    script = """
        import statistics, sys, threading, time
        import tickertape as tt

        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        reader = tt.Reader(path)
        stop = threading.Event()

        def wait():
            while not stop.is_set():
                reader.next(timeout=0.2)

        thread = threading.Thread(target=wait)
        thread.start()
        time.sleep(0.1)
        took = []
        for _ in range(20):
            started = time.monotonic()
            assert reader.next(timeout=0.04) is None
            took.append(time.monotonic() - started)
        stop.set()
        thread.join()
        print(f"{statistics.median(took):.4f}")
    """
    assert float(run_script(script, journal)) < 0.07


# The parent closes its writer while a forked child is still alive and has released its inherited copies. Both must
# then be able to open a new writer: the child's copy of the lock file's descriptor no longer holds the lock.
FORK_RELEASES = """
    import gc, json, multiprocessing, os, sys, warnings
    import tickertape as tt

    warnings.simplefilter("ignore", DeprecationWarning)  # "use of fork() may lead to deadlocks" (3.12+)
    path, prefault, how, start = sys.argv[1], sys.argv[2] == "1", sys.argv[3], sys.argv[4]
    writer = tt.Writer(path, segment_size=1 << 20, prefault=prefault)
    writer.append(b"zero")
    reader = tt.Reader(path)
    assert reader.try_next().seq == 0

    def open_fds():
        return len(os.listdir("/dev/fd"))

    def child(to_parent, from_parent):
        global writer, reader
        before = open_fds()
        if how == "close":
            writer.close()
            reader.close()
        elif how == "exit":
            writer.__exit__(None, None, None)
            reader.__exit__(None, None, None)
        else:
            del writer, reader
            gc.collect()
        released = before - open_fds()
        os.write(to_parent, b"r")
        assert os.read(from_parent, 1) == b"g"
        try:
            with tt.Writer(path) as mine:
                seq = mine.append(b"child")
            result = {"released": released, "child_append": seq}
        except Exception as error:
            result = {"released": released, "error": repr(error)}
        os.write(to_parent, json.dumps(result).encode())

    to_parent_r, to_parent_w = os.pipe()
    from_parent_r, from_parent_w = os.pipe()
    if start == "os.fork":
        pid = os.fork()
        if pid == 0:
            try:
                child(to_parent_w, from_parent_r)
            finally:
                os._exit(0)
        wait = lambda: os.waitpid(pid, 0)
    else:
        proc = multiprocessing.get_context("fork").Process(target=child, args=(to_parent_w, from_parent_r))
        proc.start()
        wait = proc.join
    assert os.read(to_parent_r, 1) == b"r"  # the child has released its copies and is still alive
    assert reader.try_next() is None
    writer.close()
    reader.close()
    try:
        with tt.Writer(path) as again:
            parent_append = again.append(b"parent")
    except tt.LockedError as error:
        parent_append = repr(error)
    os.write(from_parent_w, b"g")
    child_result = json.loads(os.read(to_parent_r, 4096))
    wait()
    print(json.dumps({"parent_append": parent_append, **child_result}))
    print(json.dumps([(r.seq, r.payload.decode()) for r in tt.Reader(path)]))
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
@pytest.mark.parametrize("start", ["os.fork", "multiprocessing"])
@pytest.mark.parametrize("how", ["close", "exit", "collect"])
@pytest.mark.parametrize("prefault", [False, True], ids=["prefault=False", "prefault=True"])
def test_forked_child_releases_the_journal(journal: Path, prefault: bool, how: str, start: str) -> None:
    result, records = map(json.loads, run_script(FORK_RELEASES, journal, int(prefault), how, start).splitlines())
    assert result["parent_append"] == 1, result
    assert result.get("child_append") == 2, result
    # The writer holds at least the lock file, the checkpoint file and a segment; the reader a segment.
    assert result["released"] >= 4, result
    assert records == [[0, "zero"], [1, "parent"], [2, "child"]]
    assert tt.verify(journal).ok


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
def test_fork_guard_survives_process_id_reuse(journal: Path) -> None:
    """A handle whose recorded process id matches the current process is still foreign after a fork."""
    script = """
        import json, os, sys, warnings
        import tickertape as tt

        warnings.simplefilter("ignore", DeprecationWarning)
        path = sys.argv[1]
        writer = tt.Writer(path, segment_size=1 << 20)
        reader = tt.Reader(path)
        writer._set_owner_pid_for_testing(os.getpid())  # no change in the owner
        assert writer.append(b"parent") == 0
        pid = os.fork()
        if pid == 0:
            results = {}
            # As if this child had been given the process id of the process that opened the handles.
            writer._set_owner_pid_for_testing(os.getpid())
            reader._set_owner_pid_for_testing(os.getpid())
            for name, call in [("append", lambda: writer.append(b"child")), ("try_next", reader.try_next)]:
                try:
                    call()
                    results[name] = "returned"
                except tt.UsedAfterForkError:
                    results[name] = "refused"
            writer.close()
            reader.close()
            print(json.dumps(results), flush=True)
            os._exit(0)
        os.waitpid(pid, 0)
        print(json.dumps([writer.append(b"after"), reader.try_next().seq]), flush=True)
    """
    child, parent = map(json.loads, run_script(script, journal).splitlines())
    assert child == {"append": "refused", "try_next": "refused"}
    assert parent == [1, 0]


# A signal arrives while a read that has built its result waits for the reader's lock to deliver it (another thread
# holds the lock). The handler raises: the read must raise and deliver nothing, never deliver the records and lose
# them when the handler runs after the call returns. The test hooks make the order deterministic: the read pauses
# after building its result, the helper thread takes the lock, lets the read go on, waits until the read is queued
# for the lock, raises the signal and releases the lock.
SIGNAL_DURING_DELIVERY = """
    import signal, sys, threading
    import tickertape as tt

    path, call, runs = sys.argv[1], sys.argv[2], int(sys.argv[3])
    with tt.Writer(path, segment_size=1 << 20) as writer:
        writer.append_batch([i.to_bytes(4, "little") for i in range(runs + 10)])
    reader = tt.Reader(path)
    read = {
        "try_next": reader.try_next,
        "iter": lambda: next(iter(reader)),
        "next": lambda: reader.next(timeout=5),
        "read_many": lambda: reader.read_many(10),
        "read_batch": lambda: reader.read_batch(10),
    }[call]

    class Interrupt(Exception):
        pass

    def handler(signum, frame):
        raise Interrupt

    signal.signal(signal.SIGUSR1, handler)
    for expected in range(runs):
        reader._arm_commit_race_for_testing()
        raced = []
        helper = threading.Thread(target=lambda: raced.append(reader._race_commit_for_testing(signal.SIGUSR1)))
        helper.start()
        try:
            got = read()
        except Interrupt:
            pass
        else:
            print("returned", got)
            sys.exit(1)
        finally:
            helper.join()
        assert raced == [True], raced
        # Nothing was delivered: the next read returns the records the interrupted one had built its result from.
        assert reader.position == expected, (reader.position, expected)
        record = reader.try_next()
        assert record is not None and record.seq == expected, (record, expected)
    print("ok")
"""


@needs_test_hooks
@pytest.mark.parametrize("call", ["try_next", "iter", "next", "read_many", "read_batch"])
def test_signal_while_a_read_waits_to_deliver_consumes_nothing(journal: Path, call: str) -> None:
    runs = int(os.environ.get("TICKERTAPE_DELIVERY_RUNS", "20"))
    assert run_script(SIGNAL_DURING_DELIVERY, journal, call, runs, timeout=60 + runs).strip() == "ok"


# A handle whose mutex was locked when the process forked (another thread was inside a call) may be half-updated in
# the child. Closing or collecting it there must neither wait for the lock nor drop the value: the child's copies of
# the descriptors stay open. A handle that was not busy is released as usual.
FORKED_WHILE_BUSY = """
    import gc, json, os, sys, warnings
    import tickertape as tt

    warnings.simplefilter("ignore", DeprecationWarning)  # "use of fork() may lead to deadlocks" (3.12+)
    path = sys.argv[1]
    writer = tt.Writer(path, segment_size=1 << 20)
    writer.append(b"zero")
    busy_reader = tt.Reader(path)
    idle_reader = tt.Reader(path)
    assert idle_reader.try_next().seq == 0 and busy_reader.try_next().seq == 0  # both have a segment open
    # As if a thread had been inside a call on these two handles at the fork.
    writer._lock_forever_for_testing()
    busy_reader._lock_forever_for_testing()

    def open_fds():
        return len(os.listdir("/dev/fd"))

    pid = os.fork()
    if pid == 0:
        results = {"refused": 0}
        for call in (lambda: writer.append(b"child"), busy_reader.try_next, idle_reader.try_next):
            try:
                call()
            except tt.UsedAfterForkError:
                results["refused"] += 1
        del call  # a bound method keeps its handle alive
        writer.close()
        busy_reader.close()
        results["closed"] = writer.closed and busy_reader.closed
        before = open_fds()
        del writer
        gc.collect()
        results["writer"] = before - open_fds()
        before = open_fds()
        del busy_reader
        gc.collect()
        results["busy_reader"] = before - open_fds()
        before = open_fds()
        del idle_reader
        gc.collect()
        results["idle_reader"] = before - open_fds()
        print(json.dumps(results), flush=True)
        os._exit(0)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, status
"""


@needs_test_hooks
@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
def test_handle_busy_at_fork_is_never_released_in_the_child(journal: Path) -> None:
    result = json.loads(run_script(FORKED_WHILE_BUSY, journal))
    assert result["refused"] == 3 and result["closed"] is True
    assert result["writer"] == 0 and result["busy_reader"] == 0, result  # forgotten, not dropped
    assert result["idle_reader"] >= 1, result  # the control: its segment descriptor was closed


# Real threads are inside calls when the process forks: a read waiting for records, a read filling a large batch, and
# a writer appending batches. Whatever each handle's state at the fork, the child must refuse to use it and close and
# collect it without waiting.
FORKED_DURING_CALLS = """
    import gc, os, sys, threading, time, warnings
    import tickertape as tt

    warnings.simplefilter("ignore", DeprecationWarning)
    path = sys.argv[1]
    writer = tt.Writer(path, segment_size=64 << 20)
    writer.append_batch([b"x" * 1000] * 20_000)
    waiting = tt.Reader(path)
    filling = tt.Reader(path)
    stop = threading.Event()

    def wait_loop():
        waiting.seek(writer.next_seq + 10**9)
        while not stop.is_set():
            waiting.next(timeout=0.5)

    def fill_loop():
        while not stop.is_set():
            filling.seek(0)
            filling.read_batch(20_000)

    def append_loop():
        while not stop.is_set():
            writer.append_batch([b"y" * 100] * 2000)
            time.sleep(0.002)

    threads = [threading.Thread(target=f) for f in (wait_loop, fill_loop, append_loop)]
    for thread in threads:
        thread.start()
    statuses = []
    for _ in range(5):
        time.sleep(0.05)
        pid = os.fork()
        if pid == 0:
            refused = 0
            for call in (lambda: writer.append(b"c"), waiting.try_next, lambda: filling.read_many(1)):
                try:
                    call()
                except tt.UsedAfterForkError:
                    refused += 1
            for handle in (writer, waiting, filling):
                handle.close()
            del writer, waiting, filling, handle, call
            gc.collect()
            os._exit(0 if refused == 3 else 3)
        _, status = os.waitpid(pid, 0)
        statuses.append(os.waitstatus_to_exitcode(status))
    stop.set()
    for thread in threads:
        thread.join()
    print(statuses)
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
def test_fork_while_threads_are_inside_calls(journal: Path) -> None:
    assert run_script(FORKED_DURING_CALLS, journal, timeout=60).strip() == "[0, 0, 0, 0, 0]"


# A signal handler forks during a read while another thread holds the reader's lock. The child resumes the read where
# the handler returned; the lock stays held there for ever, because its holder exists only in the parent. The read
# must raise UsedAfterForkError in the child instead of waiting for that lock. "deliver": the handler runs while the
# read retries the lock to deliver a built result, and the race hook's thread holds the lock until the read stops
# retrying. "wait": the handler runs between two wait slices of a blocking read, after a thread has taken the lock and
# kept it (as a thread inside a call would hold it at the fork). "timeout": a timer signal (SIGALRM) arrives during a
# read with a 0.2 s timeout on an empty journal, and the handler takes longer than that before it forks, so the read
# resumes past its deadline: the child must not report a timeout. "closed": the handler closes the reader and forks:
# the child must not raise ValueError for the closed reader. A watchdog kills a child that has not finished within
# 10 s.
FORK_IN_HANDLER = """
    import os, signal, sys, threading, time, warnings
    import tickertape as tt

    warnings.simplefilter("ignore", DeprecationWarning)  # "use of fork() may lead to deadlocks" (3.12+)
    path, where, runs = sys.argv[1], sys.argv[2], int(sys.argv[3])

    class Interrupt(Exception):
        pass

    children = []

    def handler(signum, frame):
        if where == "wait":
            holder = threading.Thread(target=reader._lock_forever_for_testing)
            holder.start()
            holder.join()
        elif where == "timeout":
            time.sleep(0.25)  # the read's deadline passes before the handler returns
        elif where == "closed":
            reader.close()
        pid = os.fork()
        if pid == 0:
            return  # the child resumes the read
        children.append(pid)
        raise Interrupt

    signal.signal(signal.SIGUSR1, handler)
    signal.signal(signal.SIGALRM, handler)
    with tt.Writer(path, segment_size=1 << 20) as writer:
        writer.append_batch([b"record"] * 10)
    outcomes = []
    for run in range(runs):
        reader = tt.Reader(path)
        raced = []
        if where == "deliver":
            reader._arm_commit_race_for_testing()
            helper = threading.Thread(target=lambda: raced.append(reader._race_commit_for_testing(signal.SIGUSR1)))
            helper.start()
            read = lambda: reader.read_many(10)
        elif where == "wait":
            reader.seek(10)  # nothing there: the read waits
            helper = threading.Timer(0.2, signal.raise_signal, (signal.SIGUSR1,))
            helper.start()
            read = lambda: reader.next(timeout=10)
        else:
            reader.seek(10)
            helper = None
            signal.setitimer(signal.ITIMER_REAL, 0.02)
            read = lambda: reader.next(timeout=0.2 if where == "timeout" else 10)
        try:
            got = read()
        except tt.UsedAfterForkError:
            os._exit(0)  # the child: refused, as it must be
        except Interrupt:
            pass  # the parent
        except BaseException:
            os._exit(3)  # the child, failing otherwise (or the parent: no child then, the wait below fails)
        else:
            os._exit(2)  # the child returned (records, or a timeout) (or the parent: no signal arrived)
        if helper is not None:
            helper.join()
        assert raced == ([True] if where == "deliver" else []), raced
        child = children.pop()
        deadline = time.monotonic() + 10  # the watchdog
        while (done := os.waitpid(child, os.WNOHANG))[0] == 0 and time.monotonic() < deadline:
            time.sleep(0.01)
        if done[0] == 0:
            os.kill(child, signal.SIGKILL)
            os.waitpid(child, 0)
            outcomes.append("hung")
        else:
            code = os.waitstatus_to_exitcode(done[1])
            outcomes.append({0: "refused", 2: "returned", 3: "failed"}.get(code, f"exit {code}"))
    print(sorted(set(map(str, outcomes))))
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
@pytest.mark.parametrize("where", ["deliver", "wait", "timeout", "closed"])
def test_fork_in_a_signal_handler_during_a_read_does_not_hang_the_child(journal: Path, where: str) -> None:
    if where in ("deliver", "wait") and not hasattr(tt.Reader, "_race_commit_for_testing"):
        pytest.skip(needs_test_hooks.kwargs["reason"])  # "timeout" and "closed" need no hooks
    runs = int(os.environ.get("TICKERTAPE_DELIVERY_RUNS", "20" if where == "deliver" else "3"))
    out = run_script(FORK_IN_HANDLER, journal, where, runs, timeout=60 + 11 * runs).strip()
    assert out == "['refused']", out


# A finalizer forks while the binding builds the exception that a call on a closed handle raises. On CPython 3.11 the
# garbage collector runs inside the allocation that crosses its threshold, so with the threshold at 1 and a garbage
# cycle whose finalizer forks, the first garbage-collected allocation in the call runs the finalizer there. In these
# calls that is the exception object (`ValueError`, built from the error PyO3 created lazily): the child resumes
# building it, and must raise `UsedAfterForkError` all the same, because the owner check comes after the exception is
# built (`Owner::settle`). From 3.12 on, the collector runs only at the interpreter's next check, after the call, so
# the test needs 3.11. The calls are bound before the collector is armed (a bound method is a garbage-collected
# object).
FORK_WHILE_RAISING = """
    import gc, os, sys
    import tickertape as tt

    path, call = sys.argv[1], sys.argv[2]
    writer = tt.Writer(path, segment_size=1 << 20)
    reader = tt.Reader(path)
    owner, name = call.split(".")
    handle = writer if owner == "writer" else reader
    handle.close()
    args = {"append": (b"x",), "read_many": (10,), "read_batch": (10,), "seek": (0,)}.get(name, ())
    function = getattr if name in ("position", "next_seq") else getattr(handle, name)
    if function is getattr:
        args = (handle, name)
    forked = []

    class Forker:
        def __del__(self):
            forked.append(os.fork())

    gc.collect()
    gc.disable()
    cycle = Forker()
    cycle.cycle = cycle
    del cycle
    gc.set_threshold(1)
    keep = [[] for _ in range(10)]  # well past the threshold
    gc.enable()
    try:
        function(*args)
    except tt.UsedAfterForkError:
        outcome = "refused"
    except ValueError:
        outcome = "ValueError"
    except BaseException as error:
        outcome = type(error).__name__
    else:
        outcome = "returned"
    gc.set_threshold(700)
    if forked == [0]:
        os._exit({"refused": 0, "ValueError": 3}.get(outcome, 4))
    assert len(forked) == 1, "the finalizer did not run inside the call"
    _, status = os.waitpid(forked[0], 0)
    child = {0: "refused", 3: "ValueError", 4: "other"}.get(os.waitstatus_to_exitcode(status), "exit")
    print(outcome, child)
"""


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
@pytest.mark.skipif(sys.version_info[:2] != (3, 11), reason="the garbage collector runs inside allocations only on 3.11")
@pytest.mark.parametrize("call", ["reader.try_next", "reader.read_many", "reader.read_batch", "reader.seek",
                                  "reader.position", "reader.__iter__", "writer.append", "writer.next_seq",
                                  "writer.checkpoint", "writer.__enter__"])
def test_fork_while_the_exception_is_built_raises_used_after_fork_in_the_child(journal: Path, call: str) -> None:
    assert run_script(FORK_WHILE_RAISING, journal, call).split() == ["ValueError", "refused"]


@needs_test_hooks
@pytest.mark.parametrize("size", ["small", "oversized"])
@pytest.mark.parametrize("call", ["try_next", "iter", "next", "read_many", "read_batch"])
def test_nothing_runs_python_code_between_the_delivery_and_the_return(journal: Path, call: str, size: str) -> None:
    """A read runs no signal handler and no other Python code from the delivery of its records to its return.

    The read pauses right after the delivery while another thread raises a signal (`_signal_after_commit_for_testing`),
    then goes on. If anything in the rest of the call checked signals or ran Python code (which checks them too), the
    handler would run inside the read, and see `_returning_for_testing()` true; it must run only once the call has
    returned. "oversized" also covers freeing a staging buffer too large to keep, which happens in that stretch.
    """
    count = 1 if call in ("try_next", "iter", "next") else 1024
    big = size == "oversized"
    with tt.Writer(journal, segment_size=64 << 20) as w:
        if count == 1:
            w.append(b"x" * ((7 << 20) if big else 100))
        else:
            w.append_batch([b"x" * (7168 if big else 16)] * count)  # 7 MiB in all: more than RETAINED_CAPACITY
    seen: list[tuple[bool, int]] = []
    runs = int(os.environ.get("TICKERTAPE_DELIVERY_RUNS", "20"))
    with tt.Reader(journal) as reader:
        returning = getattr(reader, "_returning_for_testing")  # test hooks, not in the stubs
        arm = getattr(reader, "_arm_signal_after_commit_for_testing")
        race = getattr(reader, "_signal_after_commit_for_testing")
        reads: dict[str, Callable[[], object]] = {
            "try_next": reader.try_next,
            "iter": lambda: next(iter(reader)),
            "next": lambda: reader.next(timeout=5),
            "read_many": lambda: reader.read_many(count),
            "read_batch": lambda: reader.read_batch(count),
        }

        def handler(signum: int, frame: object) -> None:
            seen.append((returning(), reader.position))

        previous = signal.signal(signal.SIGUSR1, handler)
        try:
            for _ in range(runs):
                reader.seek(0)
                seen.clear()
                arm()
                raced: list[bool] = []
                helper = threading.Thread(target=lambda: raced.append(race(signal.SIGUSR1)))
                helper.start()
                got = reads[call]()
                helper.join()
                deadline = time.monotonic() + 5
                while not seen and time.monotonic() < deadline:
                    time.sleep(0.001)  # the handler runs at the main thread's next check
                assert raced == [True]
                assert seen == [(False, count)], seen  # it ran after the read had returned
                if count == 1:
                    assert isinstance(got, tt.Record)
                else:
                    assert isinstance(got, (list, tt.RecordBatch)) and len(got) == count
        finally:
            signal.signal(signal.SIGUSR1, previous)
