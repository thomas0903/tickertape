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


# A signal whose handler raises arrives at a random moment of a read that returns a large result, often while the
# result is being built. The records must either reach the caller or stay staged for the next read. A timer signal
# (SIGALRM) is used because the kernel delivers it while the reading thread holds the GIL, which a Python thread
# could not do. One loss is tolerated: the documented window after the call has returned its result and before
# Python stores it, which is a few microseconds long and can be told apart because the cursor had already moved
# when the handler ran.
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
        writer.append_many([i.to_bytes(8, "little") * (size // 8) for i in range(count)])
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
    # Aim at the later part of the call, where the result is built (the first calls are slower).
    duration = min(durations)
    staged = after_return = delivered = 0
    for attempt in range(40):
        reader.seek(0)
        box = []
        seen.clear()
        try:
            armed[0] = True
            signal.setitimer(signal.ITIMER_REAL, random.uniform(0.3 * duration, 1.0 * duration))
            box.append(read())
            armed[0] = False
            signal.setitimer(signal.ITIMER_REAL, 0)
        except Interrupt:
            signal.setitimer(signal.ITIMER_REAL, 0)
            if box:
                delivered += 1  # the handler ran after the result was stored
            elif seen[0] != 0:
                after_return += 1  # the documented window: returned, not yet stored
                continue
            else:
                staged += 1
                assert reader.position == 0, reader.position
                box.append(read())  # the interrupted call's records are returned by the next read
        else:
            delivered += 1
        check(box[0])
    print(staged, after_return, delivered)
"""


@pytest.mark.parametrize("call", ["next", "read_many", "read_batch"])
def test_interrupt_while_the_result_is_built_keeps_the_records(journal: Path, call: str) -> None:
    staged, after_return, delivered = map(int, run_script(INTERRUPTED_WHILE_BUILDING, journal, call,
                                                          timeout=120).split())
    assert after_return <= 1, (staged, after_return, delivered)
    assert staged >= 5, (staged, after_return, delivered)  # the test did interrupt reads


def test_signal_handler_reading_during_iteration_does_not_end_it(journal: Path) -> None:
    """A handler that takes records while a ``for`` loop iterates must not make the loop stop early."""
    script = """
        import signal, sys
        import tickertape as tt

        path, count = sys.argv[1], 50_000
        with tt.Writer(path, segment_size=16 << 20) as writer:
            writer.append_many([b"%d" % i for i in range(count)])
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
        w.append_many([b"%d" % i for i in range(count)])
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
        import signal, sys, threading
        import tickertape as tt

        path, call, closer = sys.argv[1:4]
        writer = tt.Writer(path, segment_size=1 << 20)
        raised = late = 0
        while raised < 5:
            reader = tt.Reader(path)
            if closer == "signal":
                signal.signal(signal.SIGALRM, lambda *_: reader.close())
                signal.setitimer(signal.ITIMER_REAL, 0.01)
            else:
                threading.Timer(0.01, reader.close).start()
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
            if reader.closed:
                print("returned", repr(result))  # closed before the read returned, yet it reported a timeout
                sys.exit(1)
            # The close came only after the read had returned (the timer or thread ran more than 40 ms late on a
            # loaded machine), so the timeout was right. Wait for it and try again.
            late += 1
            assert late < 20, "the closer kept running late"
            while not reader.closed:
                signal.pause() if closer == "signal" else threading.Event().wait(0.01)
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
