"""Writer and reader behaviour within one process."""

from __future__ import annotations

import threading
import time
from pathlib import Path

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern, as_tuples, pattern_tuple


def test_round_trip_and_metadata(journal: Path) -> None:
    with tt.Writer(journal) as w:
        assert w.next_seq == 0
        assert w.append(b"hello", schema_id=7, venue_ts_ns=11, local_ts_ns=13) == 0
        assert w.append(b"", schema_id=2**32 - 1, venue_ts_ns=2**64 - 1, local_ts_ns=2**64 - 1) == 1
        assert w.append(bytearray(b"ba")) == 2
        assert w.append(memoryview(b"xmemx")[1:4]) == 3
        assert w.next_seq == 4
    with tt.Reader(journal) as r:
        records = list(r)
    assert as_tuples(records) == [
        (0, 7, 11, 13, b"hello"),
        (1, 2**32 - 1, 2**64 - 1, 2**64 - 1, b""),
        (2, 0, 0, 0, b"ba"),
        (3, 0, 0, 0, b"mem"),
    ]
    assert records[0] == records[0] and records[0] != records[1]
    assert hash(records[0]) == hash(records[0].as_tuple())
    assert records[0].as_dict() == {"seq": 0, "schema_id": 7, "venue_ts_ns": 11, "local_ts_ns": 13,
                                    "payload": b"hello"}
    assert "seq=0" in repr(records[0])


def test_payload_and_metadata_types_are_checked(journal: Path) -> None:
    with tt.Writer(journal) as w:
        with pytest.raises(TypeError):
            w.append("text")  # type: ignore[arg-type]
        with pytest.raises(OverflowError):
            w.append(b"", schema_id=2**32)
        with pytest.raises(OverflowError):
            w.append(b"", venue_ts_ns=-1)
        assert w.next_seq == 0


def test_records_own_their_payloads(journal: Path) -> None:
    source = bytearray(b"original")
    with tt.Writer(journal) as w:
        w.append(source)
        source[:] = b"changed!"  # the writer copied it
        w.append(b"second")
    reader = tt.Reader(journal)
    first = reader.try_next()
    reader.try_next()
    reader.close()
    assert first is not None and first.payload == b"original"
    assert isinstance(first.payload, bytes)


def test_try_next_returns_none_at_end_and_follows_appends(journal: Path) -> None:
    with tt.Writer(journal) as w:
        r = tt.Reader(journal)
        assert r.try_next() is None
        w.append(b"a")
        record = r.try_next()
        assert record is not None and record.payload == b"a"
        assert r.try_next() is None
        assert r.position == 1
        # Iteration stops at the current end and continues from there later.
        assert list(r) == []
        w.append(b"b")
        assert [x.payload for x in r] == [b"b"]


def test_seek_and_position(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 1000)
    with tt.Reader(journal) as r:
        r.seek(777)
        assert r.position == 777
        record = r.try_next()
        assert record is not None and record.as_tuple() == pattern_tuple(777)
        r.seek(1005)  # not written yet: the reader waits for it
        assert r.try_next() is None
        assert r.position == 1005


def test_segment_rolls_are_transparent(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 2000)
        w.checkpoint()
    segments = tt.list_segments(journal)
    assert len(segments) > 3
    assert segments[0].base_seq == 0 and all(s.segment_size == SMALL_SEGMENT for s in segments)
    with tt.Reader(journal) as r:
        assert as_tuples(r) == [pattern_tuple(s) for s in range(2000)]
    report = tt.verify(journal)
    assert report.ok and report.records == 2000 and report.first_seq == 0 and report.last_seq == 1999
    assert [s.end for s in report.segments] == ["sealed"] * (len(segments) - 1) + ["open"]
    assert tt.read_checkpoint(journal) == tt.Checkpoint(1, 2000, segments[-1].base_seq,
                                                        tt.read_checkpoint(journal).offset)  # type: ignore[union-attr]


def test_append_many_returns_range_and_accepts_columns(journal: Path) -> None:
    with tt.Writer(journal) as w:
        assert w.append_many([]) == range(0, 0)
        assert w.append_many([b"a", b"b"], schema_id=5, venue_ts_ns=[1, 2], local_ts_ns=(3, 4)) == range(0, 2)
        assert w.append_many(iter([b"c"])) == range(2, 3)
    with tt.Reader(journal) as r:
        assert as_tuples(r) == [(0, 5, 1, 3, b"a"), (1, 5, 2, 4, b"b"), (2, 0, 0, 0, b"c")]


def test_append_many_checks_everything_before_appending(journal: Path) -> None:
    with tt.Writer(journal) as w:
        with pytest.raises(TypeError):
            w.append_many([b"a", 3])  # type: ignore[list-item]
        with pytest.raises(TypeError):
            w.append_many(b"not a list of payloads")  # type: ignore[arg-type]
        with pytest.raises(ValueError, match="2 values for 3 payloads"):
            w.append_many([b"a", b"b", b"c"], schema_id=[1, 2])
        with pytest.raises(OverflowError):
            w.append_many([b"a", b"b"], venue_ts_ns=[1, -1])
        with pytest.raises(OverflowError):
            w.append_many([b"a"], schema_id=2**32)
        assert w.next_seq == 0


def test_append_many_partial_failure(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        w.append(b"before")
        too_large = b"x" * SMALL_SEGMENT
        with pytest.raises(tt.BatchAppendError) as raised:
            w.append_many([b"a", b"b", too_large, b"c"], schema_id=[1, 2, 3, 4])
        error = raised.value
        assert (error.appended, error.first_seq, error.failed_index) == (2, 1, 2)
        assert isinstance(error.__cause__, tt.RecordTooLargeError)
        assert isinstance(error.__cause__, ValueError)
        assert w.next_seq == 3
        # The caller retries the rest after fixing the cause.
        assert w.append_many([b"c"], schema_id=4) == range(3, 4)
        with pytest.raises(tt.BatchAppendError) as raised:
            w.append_many([too_large])
        assert (raised.value.appended, raised.value.first_seq) == (0, None)
    with tt.Reader(journal) as r:
        assert [(x.seq, x.schema_id, x.payload) for x in r] == [
            (0, 0, b"before"), (1, 1, b"a"), (2, 2, b"b"), (3, 4, b"c")]


def test_read_many_and_timeouts(journal: Path) -> None:
    with tt.Writer(journal) as w:
        append_pattern(w, 25)
        r = tt.Reader(journal)
        assert as_tuples(r.read_many(10)) == [pattern_tuple(s) for s in range(10)]
        assert len(r.read_many(100)) == 15
        assert r.read_many(100) == []
        assert r.read_many(0) == []
        started = time.monotonic()
        assert r.read_many(5, timeout=0.2) == []
        assert r.next(timeout=0.1) is None
        assert time.monotonic() - started >= 0.29

        def later() -> None:
            time.sleep(0.2)
            w.append_many([b"x", b"y"])

        thread = threading.Thread(target=later)
        thread.start()
        got = r.read_many(10, timeout=None)  # waits for the first, then takes what is committed
        thread.join()
        got += r.read_many(10)
        assert [x.payload for x in got] == [b"x", b"y"]
        with pytest.raises(ValueError):
            r.next(timeout=-1)


def test_blocked_reader_does_not_block_other_threads(journal: Path) -> None:
    with tt.Writer(journal) as w:
        r = tt.Reader(journal)
        results: list[tt.Record | None] = []
        thread = threading.Thread(target=lambda: results.append(r.next(timeout=5)))
        thread.start()
        time.sleep(0.1)
        w.append(b"from another thread")  # needs the GIL, which the blocked reader released
        thread.join(5)
        assert not thread.is_alive()
        assert results[0] is not None and results[0].payload == b"from another thread"


def test_context_managers_and_double_close(journal: Path) -> None:
    w = tt.Writer(journal)
    with w as entered:
        assert entered is w and not w.closed
        w.append(b"a")
    assert w.closed
    w.close()  # idempotent
    with pytest.raises(ValueError, match="closed writer"):
        w.append(b"b")
    with pytest.raises(ValueError):
        w.checkpoint()
    with pytest.raises(ValueError):
        w.__enter__()
    assert "closed" in repr(w)
    # Closing released the lock.
    with tt.Writer(journal) as again:
        assert again.next_seq == 1

    r = tt.Reader(journal)
    with r:
        assert r.try_next() is not None
    r.close()
    assert r.closed
    with pytest.raises(ValueError, match="closed reader"):
        r.try_next()
    with pytest.raises(ValueError):
        r.read_many(1)
    with pytest.raises(ValueError):
        list(r)


def test_exceptions_propagate_through_context_manager(journal: Path) -> None:
    with pytest.raises(RuntimeError):
        with tt.Writer(journal) as w:
            raise RuntimeError("boom")
    assert w.closed


def test_second_writer_is_refused(journal: Path) -> None:
    with tt.Writer(journal) as w:
        w.append(b"a")
        with pytest.raises(tt.LockedError) as raised:
            tt.Writer(journal)
        assert raised.value.path == journal
        assert raised.value.pid is None or isinstance(raised.value.pid, int)
        assert w.append(b"b") == 1  # the refused open touched nothing


def test_closing_without_checkpoint_keeps_records_visible_not_durable(journal: Path) -> None:
    with tt.Writer(journal) as w:
        w.append(b"a")
        w.checkpoint()
        w.append(b"b")
    checkpoint = tt.read_checkpoint(journal)
    assert checkpoint is not None and checkpoint.next_seq == 1
    with tt.Reader(journal) as r:
        assert [x.payload for x in r] == [b"a", b"b"]
    with tt.Reader(journal, durable_only=True) as r:
        assert r.durable_only
        assert [x.payload for x in r] == [b"a"]


def test_durable_only_waits_for_checkpoints(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        r = tt.Reader(journal, durable_only=True)
        append_pattern(w, 500)
        assert r.try_next() is None  # no checkpoint yet: nothing is covered
        assert r.read_many(10, timeout=0.05) == []
        w.checkpoint()
        assert as_tuples(r.read_many(1000)) == [pattern_tuple(s) for s in range(500)]
        append_pattern(w, 300)  # crosses segment boundaries
        assert r.next(timeout=0.1) is None

        def later() -> None:
            time.sleep(0.1)
            w.checkpoint()

        thread = threading.Thread(target=later)
        thread.start()
        first = r.next(timeout=5)
        thread.join()
        assert first is not None and first.seq == 500
        assert as_tuples(r.read_many(1000)) == [pattern_tuple(s) for s in range(501, 800)]


def test_open_errors(journal: Path) -> None:
    journal.mkdir()
    with pytest.raises(tt.JournalNotFoundError) as raised:
        tt.Reader(journal)
    assert raised.value.path == journal
    with pytest.raises(tt.JournalNotFoundError):
        tt.verify(journal)
    with pytest.raises(tt.JournalIOError) as io_error:
        tt.Reader(journal / "missing")
    assert isinstance(io_error.value, OSError) and io_error.value.errno is not None
    with pytest.raises(tt.InvalidConfigError):
        tt.Writer(journal, segment_size=1000)
    with pytest.raises(ValueError, match="recovery"):
        tt.Writer(journal, recovery="maybe")  # type: ignore[arg-type]
    assert tt.read_checkpoint(journal) is None


def test_seek_before_first_available_record(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT,
                   retention=tt.Retention.delete(max_segments=2)) as w:
        append_pattern(w, 2000)
        w.checkpoint()
        assert w.last_retention_error is None
        assert w.retention_report["removed_segments"] > 0
    first = tt.list_segments(journal)[0].base_seq
    assert first > 0
    with tt.Reader(journal) as r:
        with pytest.raises(tt.SeqUnavailableError) as raised:
            r.seek(0)
        assert isinstance(raised.value, LookupError)
        assert (raised.value.requested, raised.value.first) == (0, first)
        record = r.try_next()
        assert record is not None and record.as_tuple() == pattern_tuple(first)


def test_recovery_report_and_config_objects(journal: Path, tmp_path: Path) -> None:
    with tt.Writer(journal, recovery="fail", notify=False, prefault=False) as w:
        report = w.recovery_report
        assert report["actions"] == [{"kind": "created"}]
        assert w.path == journal
    with tt.Writer(journal) as w:
        assert w.recovery_report["actions"] == []
    backoff = tt.Backoff(spins=1, yields=2, min_sleep=0.0001, max_sleep=0.01)
    assert (backoff.spins, backoff.yields, backoff.min_sleep, backoff.max_sleep) == (1, 2, 0.0001, 0.01)
    assert tt.Backoff() == tt.Backoff() and tt.Backoff.busy_spin().yields == 0
    with pytest.raises(ValueError):
        tt.Backoff(min_sleep=1, max_sleep=0.5)
    with tt.Reader(journal, notify=False, backoff=backoff) as r:
        assert r.next(timeout=0.01) is None
    archive = tmp_path / "archive"
    retention = tt.Retention.archive(archive, max_bytes=10, max_age=3.5)
    assert (retention.archive_dir, retention.max_bytes, retention.max_age, retention.max_segments) == (
        archive, 10, 3.5, None)
    assert tt.Retention.delete().archive_dir is None
    import datetime
    assert tt.Retention.delete(max_age=datetime.timedelta(minutes=1)).max_age == 60.0


def test_retention_error_is_reported_not_raised(journal: Path, tmp_path: Path) -> None:
    missing = tmp_path / "no-such-archive"
    with tt.Writer(journal, segment_size=SMALL_SEGMENT,
                   retention=tt.Retention.archive(missing, max_segments=1)) as w:
        append_pattern(w, 2000)
        w.checkpoint()  # succeeds although retention fails
        error = w.last_retention_error
        assert isinstance(error, tt.JournalIOError)


def test_module_metadata() -> None:
    assert tt.__version__ == tt.CORE_VERSION
    assert tt.FORMAT_VERSION == 1
    assert tt.DEFAULT_SEGMENT_SIZE == 128 * 1024 * 1024
    assert "bad_checksum" in tt.CORRUPT_REASONS
