"""Behaviour across processes and signals: live following, durable-only readers and Ctrl-C."""

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


@pytest.mark.skipif(not hasattr(os, "fork"), reason="needs fork")
def test_second_writer_in_another_process_is_refused(journal: Path) -> None:
    script = "import sys, tickertape as tt\ntry:\n    tt.Writer(sys.argv[1])\nexcept tt.LockedError as e:\n" \
             "    print('locked', e.pid)\n"
    with tt.Writer(journal):
        out = subprocess.check_output([sys.executable, "-c", script, str(journal)], text=True)
    assert out.split()[0] == "locked"
    assert out.split()[1] == str(os.getpid())
