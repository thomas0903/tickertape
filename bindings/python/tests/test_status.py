"""Operational metrics (ADR 0032): the status sample, the writer's metrics and a lagging reader's."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern


def test_status_writer_and_lagging_reader(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        fresh = w.metrics()
        assert fresh.next_seq == 0 and fresh.checkpoint is None and fresh.since_checkpoint_ns is None
        append_pattern(w, 400)
        w.checkpoint()
        append_pattern(w, 600)
        m = w.metrics()
        assert isinstance(m, tt.WriterMetrics)
        assert m.next_seq == 1000 and m.segment_base_seq > 0
        assert m.checkpoint is not None and m.checkpoint.next_seq == 400
        assert m.since_checkpoint_ns is not None and m.since_checkpoint_ns >= 0
        assert m.uncheckpointed_records == 600 and m.uncheckpointed_bytes > 0
        assert not m.poisoned and m.retention_failures == 0 and m.segment_size == SMALL_SEGMENT

        with tt.Reader(journal) as lagging, tt.Reader(journal, durable_only=True) as durable:
            assert len(lagging.read_many(100)) == 100
            assert len(durable.read_many(30)) == 30
            s = tt.JournalStatus.sample(journal)
            assert isinstance(s, tt.JournalStatus)
            assert s.path == journal
            assert (s.head.next_seq, s.head.offset) == (1000, m.offset)
            assert s.head.end == "open" and s.head.invalid_reason is None
            assert s.head.last_local_ts_ns == 999 * 10 + 1
            assert not s.head.chain_broken
            assert s.segments.segment_count > 2 and s.segments.first_seq == 0
            assert s.checkpoint is not None and s.checkpoint.next_seq == 400 and s.checkpoint.problem is None
            assert s.exposure.records == 600 and (s.exposure.bytes or 0) > 0
            assert s.exposure.oldest_local_ts_ns == 400 * 10 + 1
            assert s.storage.available_bytes > 0 and s.storage.next_segment_size == SMALL_SEGMENT
            assert s.writer_pid == os.getpid() and s.writer_alive is True
            assert s.readers_dir == journal / "readers" and s.readers_dir_state == "missing"
            assert s.readers == []
            assert "_sample" not in repr(s)

            r = lagging.metrics(s)
            assert isinstance(r, tt.ReaderMetrics)
            assert (r.next_seq, r.slot_seq, r.buffered) == (100, 100, 0)
            assert not r.seek_pending and not r.durable_only
            assert r.lag is not None and r.lag.records == 900 and (r.lag.bytes or 0) > 0
            assert r.lag_unknown is None
            assert r.next_seq == lagging.position
            d = durable.metrics(s)
            assert d.durable_only and d.lag is not None and d.lag.records == 370

            # Past the sample: unknown until a new one.
            append_pattern(w, 400)
            while lagging.read_many(1000):
                pass
            ahead = lagging.metrics(s)
            assert ahead.lag is None and ahead.lag_unknown == "ahead_of_sample"
            fresher = tt.JournalStatus.sample(journal, readers_dir=journal / "readers")
            caught_up = lagging.metrics(fresher)
            assert caught_up.lag == tt.Lag(records=0, bytes=0)

            with pytest.raises(TypeError):
                lagging.metrics(object())  # type: ignore[arg-type]
        with pytest.raises(ValueError):
            lagging.metrics(s)


def test_status_without_a_writer_and_errors(journal: Path, tmp_path: Path) -> None:
    empty = tmp_path / "empty"
    empty.mkdir()
    with pytest.raises(tt.JournalNotFoundError):
        tt.JournalStatus.sample(empty)
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 3)
    s = tt.JournalStatus.sample(journal)
    assert s.head.next_seq == 3
    assert s.checkpoint is None
    assert s.exposure.records == 3 and s.exposure.estimated_age_ns is None


def test_status_agrees_with_the_cli(journal: Path, cli: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 300)
        w.checkpoint()
        append_pattern(w, 200)
        s = tt.JournalStatus.sample(journal)
        out = subprocess.run([str(cli), "status", "--json", str(journal)], check=True, capture_output=True, text=True)
    data = json.loads(out.stdout.splitlines()[0])
    assert data["head"]["next_seq"] == s.head.next_seq
    assert data["head"]["offset"] == s.head.offset
    assert data["head"]["end"] == s.head.end
    assert data["exposure"]["records"] == s.exposure.records
    assert data["exposure"]["bytes"] == s.exposure.bytes
    assert data["segments"]["count"] == s.segments.segment_count
    assert data["segments"]["total_bytes"] == s.segments.total_bytes
    assert data["checkpoint"]["next_seq"] == s.checkpoint.next_seq  # type: ignore[union-attr]
    assert data["readers_dir"]["state"] == s.readers_dir_state
