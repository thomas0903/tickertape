"""Writer and reader behaviour within one process."""

from __future__ import annotations

import array
import datetime
import inspect
import itertools
import math
import os
import re
import signal
import sys
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern, as_tuples, needs_test_hooks, pattern_tuple, synthetic_journal


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
        # Iteration stops at the current end; a new iterator continues from there later.
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
        assert w.retention_report.removed_segments > 0
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
        assert isinstance(report, tt.RecoveryReport)
        assert report.actions == [tt.RecoveryAction("created")]
        assert (report.next_seq, report.scanned_from_seq, report.had_checkpoint) == (0, 0, False)
        assert w.retention_report == tt.RetentionReport(0, 0, 0)
        assert w.path == journal
    with tt.Writer(journal) as w:
        assert w.recovery_report.actions == []
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


@pytest.mark.parametrize("base", [2**63 - 1, 2**63, 2**64 - 4])
def test_append_many_range_beyond_isize(tmp_path: Path, base: int) -> None:
    """The returned range is built from Python ints when the numbers don't fit in a C ssize_t."""
    journal = tmp_path / "journal"
    synthetic_journal(journal, base)
    with tt.Writer(journal) as w:
        assert w.next_seq == base
        assert w.append_many([b"a"]) == range(base, base + 1)
        assert w.append_many([]) == range(base + 1, base + 1)
        assert w.append_many([b"b", b"c"]) == range(base + 1, base + 3)
        assert w.next_seq == base + 3
    with tt.Reader(journal) as r:
        assert [x.seq for x in r] == [base, base + 1, base + 2]


def test_append_many_rejects_a_single_payload_as_container(journal: Path) -> None:
    with tt.Writer(journal) as w:
        for single in (b"", b"ab", bytearray(), bytearray(b"ab"), memoryview(b""), memoryview(b"ab"), "", "ab",
                       array.array("B"), array.array("B", b"ab")):
            with pytest.raises(TypeError, match="iterable of payloads"):
                w.append_many(single)  # type: ignore[arg-type]
        assert w.next_seq == 0


def test_append_many_stops_reading_an_endless_metadata_column(journal: Path) -> None:
    with tt.Writer(journal) as w:
        started = time.monotonic()
        with pytest.raises(ValueError, match="schema_id has more values than the 1 payloads"):
            w.append_many([b"a"], schema_id=itertools.count())
        with pytest.raises(ValueError, match="venue_ts_ns has more values than the 2 payloads"):
            w.append_many([b"a", b"b"], venue_ts_ns=iter(range(3)))
        assert time.monotonic() - started < 1
        assert w.next_seq == 0


def test_append_many_propagates_the_metadata_iterators_own_exception(journal: Path) -> None:
    def values_then(error: BaseException, count: int) -> Iterator[int]:
        yield from range(count)
        raise error

    with tt.Writer(journal) as w:
        for error in (KeyboardInterrupt(), RuntimeError("metadata source failed")):
            with pytest.raises(type(error)) as caught:
                w.append_many([b"a", b"b"], schema_id=values_then(error, 2))
            assert caught.value is error
            with pytest.raises(type(error)):
                w.append_many([b"a", b"b"], local_ts_ns=values_then(error, 1))
        assert w.next_seq == 0


def test_append_many_propagates_the_exception_of_a_metadata_objects_iter(journal: Path) -> None:
    class Unreadable:
        def __init__(self, error: BaseException) -> None:
            self.error = error

        def __iter__(self) -> Iterator[int]:
            raise self.error

    class HidesIter(type):
        """A metaclass that hides `__iter__` from attribute lookup on its classes (the slot still works)."""

        def __getattribute__(cls, name: str) -> Any:
            if name == "__iter__":
                raise AttributeError(name)
            return super().__getattribute__(name)

    class HiddenUnreadable(Unreadable, metaclass=HidesIter):
        pass

    class NotIterable:
        __iter__ = None

    def removes_its_iter(error: BaseException) -> Any:
        """A sequence whose `__iter__` removes itself from its class and then raises: the binding must decide before
        calling `iter()` that the exception is the object's own (afterwards the class has no `__iter__`)."""

        class RemovesItsIter:
            def __getitem__(self, index: int) -> int:
                return 0

            def __iter__(self) -> Iterator[int]:
                delattr(type(self), "__iter__")
                raise error

        return RemovesItsIter()

    makers: tuple[Callable[[BaseException], Any], ...] = (Unreadable, HiddenUnreadable, removes_its_iter)

    match = re.match("a", "a")  # supports indexing (a mapping-only __getitem__) but not iteration
    assert match is not None

    with tt.Writer(journal) as w:
        for error in (KeyboardInterrupt(), RuntimeError("no values today"), TypeError("not like this")):
            for cls in makers:
                for name in ("schema_id", "venue_ts_ns", "local_ts_ns"):
                    with pytest.raises(type(error)) as caught:
                        w.append_many([b"a", b"b"], **{name: cls(error)})
                    assert caught.value is error
        # An object that is not iterable at all still gets the TypeError that names the argument.
        for value in (object(), 1.5, NotIterable(), match):
            with pytest.raises(TypeError, match="venue_ts_ns must be an int or a sequence of ints"):
                w.append_many([b"a"], venue_ts_ns=value)  # type: ignore[arg-type]
        assert w.next_seq == 0


def test_append_many_looks_up_a_metadata_types_iter_once(journal: Path) -> None:
    """A successful call does one `__iter__` lookup on the type of a metadata argument that is not a list or tuple
    (and none for a list or tuple, which cannot be observed from Python: see `iterate` in `writer.rs`)."""
    lookups: list[str] = []

    class CountsLookups(type):
        def __getattribute__(cls, name: str) -> Any:
            if name == "__iter__":
                lookups.append(name)
            return super().__getattribute__(name)

    class Values(metaclass=CountsLookups):
        def __init__(self, *values: int) -> None:
            self.values = values

        def __iter__(self) -> Iterator[int]:
            return iter(self.values)

    with tt.Writer(journal) as w:
        assert w.append_many([b"a", b"b"], schema_id=Values(1, 2), venue_ts_ns=[3, 4], local_ts_ns=(5, 6)) == range(2)
        assert lookups == ["__iter__"]
        lookups.clear()
        w.append_many([b"c"], schema_id=Values(7), venue_ts_ns=Values(8), local_ts_ns=Values(9))
        assert lookups == ["__iter__"] * 3


def test_closed_writer_consumes_nothing(journal: Path) -> None:
    consumed: list[int] = []

    def endless() -> Iterator[bytes]:
        while len(consumed) < 1_000_000:  # bounded, so that a failing build fails instead of filling memory
            consumed.append(1)
            yield b"x"

    w = tt.Writer(journal)
    w.close()
    started = time.monotonic()
    with pytest.raises(ValueError, match="closed writer"):
        w.append_many(endless())
    with pytest.raises(ValueError, match="closed writer"):
        w.append_many([b"a"], schema_id=itertools.count())
    with pytest.raises(ValueError, match="closed writer"):
        w.append(12345)  # type: ignore[arg-type]  # not a payload, but the closed state is checked first
    assert consumed == [] and time.monotonic() - started < 1


def test_payload_buffers_must_be_unsigned_bytes(journal: Path) -> None:
    with tt.Writer(journal) as w:
        assert w.append(array.array("B", b"ok")) == 0
        with pytest.raises(TypeError, match="unsigned bytes"):
            w.append(array.array("b", b"no"))
        with pytest.raises(TypeError, match="unsigned bytes"):
            w.append(array.array("I", [1]))


def later(delay: float, action: object) -> threading.Thread:
    """Runs `action()` in a thread after `delay` seconds."""
    def run() -> None:
        time.sleep(delay)
        action()  # type: ignore[operator]
    thread = threading.Thread(target=run)
    thread.start()
    return thread


FOREVER = [None, math.inf, 1e19, 1e300, float(sys.maxsize), sys.maxsize, 2**64, 10**400,
           datetime.timedelta.max]


@pytest.mark.parametrize("timeout", FOREVER, ids=repr)
def test_timeouts_too_large_to_represent_wait_forever(journal: Path, timeout: object) -> None:
    with tt.Writer(journal) as w:
        r = tt.Reader(journal)
        thread = later(0.15, lambda: w.append_many([b"a", b"b", b"c"]))
        record = r.next(timeout=timeout)  # type: ignore[arg-type]
        thread.join()
        assert record is not None and record.seq == 0
        assert len(r.read_many(1, timeout=timeout)) == 1  # type: ignore[arg-type]
        assert len(r.read_batch(1, timeout=timeout)) == 1  # type: ignore[arg-type]
        thread = later(0.1, lambda: w.append(b"d"))
        assert [x.seq for x in r.read_many(10, timeout=timeout)] == [3]  # type: ignore[arg-type]
        thread.join()


def test_timeouts_accept_numbers_and_timedelta(journal: Path) -> None:
    with tt.Writer(journal):
        r = tt.Reader(journal)
        for timeout in (datetime.timedelta(milliseconds=60), 0.06):
            started = time.monotonic()
            assert r.next(timeout=timeout) is None
            assert r.read_many(1, timeout=timeout) == []
            assert len(r.read_batch(1, timeout=timeout)) == 0
            assert time.monotonic() - started >= 0.17
        for no_wait in (0, 0.0, datetime.timedelta(0)):
            assert r.next(timeout=no_wait) is None
        for bad in (-1, -0.5, -(2**70), math.nan, -math.inf, datetime.timedelta(seconds=-1)):
            with pytest.raises(ValueError, match="non-negative"):
                r.next(timeout=bad)
            with pytest.raises(ValueError, match="non-negative"):
                r.read_batch(1, timeout=bad)
        with pytest.raises(TypeError, match="timeout must be"):
            r.next(timeout="1")  # type: ignore[arg-type]


def test_backoff_sleeps_are_range_checked_and_huge_sleeps_do_not_panic(journal: Path) -> None:
    for bad in ({"min_sleep": 3601, "max_sleep": 3601}, {"max_sleep": 1e19}, {"min_sleep": -1}, {"max_sleep": math.inf}):
        with pytest.raises(ValueError):
            tt.Backoff(**bad)
    longest = tt.Backoff(spins=0, yields=0, min_sleep=3600, max_sleep=3600)
    assert longest.max_sleep == 3600.0
    with tt.Writer(journal, notify=False):
        r = tt.Reader(journal, notify=False, backoff=longest)
        started = time.monotonic()
        assert r.next(timeout=0.2) is None  # waits in 50 ms slices, doubling a 3600 s sleep each time
        assert time.monotonic() - started < 2


def test_reader_is_an_iterable_with_sticky_iterators(journal: Path) -> None:
    with tt.Writer(journal) as w:
        r = tt.Reader(journal)
        assert not hasattr(r, "__next__")
        with pytest.raises(TypeError):
            next(r)  # type: ignore[call-overload]
        w.append(b"a")
        it = iter(r)
        assert iter(it) is it and it is not iter(r)
        assert next(it).payload == b"a"
        with pytest.raises(StopIteration):
            next(it)
        w.append(b"b")
        with pytest.raises(StopIteration):
            next(it)  # an exhausted iterator stays exhausted
        assert r.position == 1
        assert [x.payload for x in r] == [b"b"]  # a new iterator continues from the reader's cursor
        r.close()
        with pytest.raises(ValueError, match="closed reader"):
            iter(r)


def test_signatures_show_the_real_defaults() -> None:
    def default(function: object, name: str) -> object:
        return inspect.signature(function).parameters[name].default  # type: ignore[arg-type]

    assert default(tt.Reader.next, "timeout") is None
    assert default(tt.Reader.read_many, "timeout") == 0
    assert default(tt.Reader.read_batch, "timeout") == 0
    assert str(inspect.signature(tt.Reader.read_many)).endswith("max_records, *, timeout=0)")
    assert str(inspect.signature(tt.Reader.next)).endswith("timeout=None)")
    assert "..." not in str(inspect.signature(tt.Writer.append_many))
    assert default(tt.Writer, "segment_size") == tt.DEFAULT_SEGMENT_SIZE
    assert default(tt.Writer, "recovery") == "quarantine"


def test_equality_and_hashing() -> None:
    assert tt.Backoff().__eq__(1) is NotImplemented
    assert tt.Backoff() == tt.Backoff() and tt.Backoff() != tt.Backoff.busy_spin()
    assert tt.Backoff() != (100, 10) and hash(tt.Backoff()) == hash(tt.Backoff())
    assert len({tt.Backoff(), tt.Backoff(), tt.Backoff.busy_spin()}) == 2
    a, b = tt.Retention.delete(max_segments=2), tt.Retention.delete(max_segments=2)
    assert a == b and hash(a) == hash(b) and a.__eq__("x") is NotImplemented
    assert a != tt.Retention.delete(max_segments=3) and a != tt.Retention.archive("/tmp", max_segments=2)
    assert tt.Retention.archive("/a", max_age=1) == tt.Retention.archive("/a", max_age=1.0)
    assert len({a, b, tt.Retention.delete()}) == 2


def test_record_equality_with_other_types(journal: Path) -> None:
    with tt.Writer(journal) as w:
        w.append(b"a")
    with tt.Reader(journal) as r:
        record = r.try_next()
    assert record is not None
    assert record.__eq__(record.as_tuple()) is NotImplemented
    assert record != record.as_tuple()
    assert record == tt.Reader(journal).try_next()


def test_reports_are_named_tuples(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT, retention=tt.Retention.delete(max_segments=2)) as w:
        append_pattern(w, 2000)
        w.checkpoint()
        report = w.retention_report
        assert isinstance(report, tt.RetentionReport) and isinstance(report, tuple)
        assert report.removed_segments > 0 and report.removed_bytes == report.removed_segments * SMALL_SEGMENT
        assert report.already_removed_segments == 0
    with tt.Writer(journal) as w:
        recovery = w.recovery_report
        assert recovery.had_checkpoint and recovery.next_seq == 2000 and recovery.scanned_from_seq == 2000
        assert recovery._fields == ("next_seq", "scanned_from_seq", "had_checkpoint", "actions")
    assert tt.RecoveryAction._fields == ("kind", "segment", "offset", "length", "quarantine_file", "reason", "file")
    assert tt.RecoveryReport.__module__ == tt.RetentionReport.__module__ == "tickertape._inspect"


def test_two_threads_share_one_writer(journal: Path) -> None:
    with tt.Writer(journal) as w:
        def appender(tag: bytes) -> None:
            for i in range(500):
                if i % 2:
                    w.append(tag + str(i).encode())
                else:
                    w.append_many([tag + str(i).encode()])

        threads = [threading.Thread(target=appender, args=(tag,)) for tag in (b"a", b"b")]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert w.next_seq == 1000
    with tt.Reader(journal) as r:
        records = list(r)
    assert [x.seq for x in records] == list(range(1000))
    for tag in (b"a", b"b"):
        mine = [x.payload for x in records if x.payload.startswith(tag)]
        assert mine == [tag + str(i).encode() for i in range(500)]


def test_two_threads_share_one_reader(journal: Path) -> None:
    """Each record is returned to exactly one caller, in order within each caller."""
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        r = tt.Reader(journal)
        seen: dict[str, list[int]] = {"next": [], "many": []}

        def consume(kind: str) -> None:
            while sum(map(len, seen.values())) < 3000:
                if kind == "next":
                    record = r.next(timeout=0.05)
                    if record is not None:
                        seen[kind].append(record.seq)
                else:
                    seen[kind].extend(x.seq for x in r.read_many(37, timeout=0.05))

        threads = [threading.Thread(target=consume, args=(kind,)) for kind in seen]
        for thread in threads:
            thread.start()
        for _ in range(30):
            append_pattern(w, 100)
            time.sleep(0.002)
        for thread in threads:
            thread.join(30)
            assert not thread.is_alive()
    assert sorted(seen["next"] + seen["many"]) == list(range(3000))
    for seqs in seen.values():
        assert seqs == sorted(seqs)
    assert seen["next"] and seen["many"]


RETAINED_CAPACITY = 6 << 20  # `RETAINED_CAPACITY` in src/reader.rs


@pytest.mark.parametrize("call", ["read_batch", "read_many", "next"])
def test_an_idle_reader_does_not_keep_a_large_delivered_batch(journal: Path, call: str) -> None:
    with tt.Writer(journal, segment_size=64 << 20) as w:
        w.append_many([bytes([i % 251]) * 4096 for i in range(2048)])  # 8 MiB
        w.append(b"z" * (8 << 20))  # one 8 MiB record
        w.append_many([b"small"] * 300)
    with tt.Reader(journal) as r:
        capacity = getattr(r, "_staging_capacity_for_testing")  # a test hook, not in the stubs
        if call == "next":
            r.seek(2048)
            record = r.next(timeout=0)
            assert record is not None and len(record.payload) == 8 << 20
        else:
            got = getattr(r, call)(2048)
            assert len(got) == 2048
        assert capacity() <= RETAINED_CAPACITY
        # Small reads keep their capacity for the next read.
        r.seek(2049)
        assert len(r.read_batch(100)) == 100
        kept = capacity()
        assert 0 < kept <= RETAINED_CAPACITY
        assert len(r.read_batch(100)) == 100
        assert capacity() == kept


def test_the_staging_bound_depends_on_capacity_not_on_the_record_count(journal: Path) -> None:
    """The figures documented in src/reader.rs. In a fresh reader, 100,000 empty records grow the staging columns to
    4,718,848 bytes, below the bound, so their buffer is kept, and 131,073 more then grow them past it, so theirs is
    freed. A reader that first read 2,049 empty records keeps the buffer of 131,073 more (4,721,408 bytes): the
    vectors grow from the capacity they had, so no record count decides."""
    with tt.Writer(journal, segment_size=64 << 20) as w:
        w.append_many([b""] * 300_000)
    with tt.Reader(journal) as r:
        capacity = getattr(r, "_staging_capacity_for_testing")  # a test hook, not in the stubs
        assert len(r.read_batch(100_000)) == 100_000
        assert capacity() == 4_718_848
        assert len(r.read_batch(131_073)) == 131_073
        assert capacity() == 0
    with tt.Reader(journal) as r:
        capacity = getattr(r, "_staging_capacity_for_testing")
        assert len(r.read_batch(2_049)) == 2_049
        assert len(r.read_batch(131_073)) == 131_073
        assert capacity() == 4_721_408 <= RETAINED_CAPACITY


@needs_test_hooks
def test_a_read_that_raised_keeps_its_staged_batch_until_it_is_delivered(journal: Path) -> None:
    """The bound applies once the staged records have been delivered: the buffer of a read that raised keeps them,
    however large, and so does a smaller read that delivers only some of them; the read that delivers the last frees
    it."""
    with tt.Writer(journal, segment_size=64 << 20) as w:
        w.append_many([b"x" * 7168] * 1024)  # 7 MiB: more than RETAINED_CAPACITY

    class Interrupt(Exception):
        pass

    def handler(signum: int, frame: object) -> None:
        raise Interrupt

    previous = signal.signal(signal.SIGUSR1, handler)
    try:
        with tt.Reader(journal) as r:
            capacity = getattr(r, "_staging_capacity_for_testing")  # a test hook, not in the stubs
            getattr(r, "_arm_commit_race_for_testing")()  # test hooks, not in the stubs
            race = getattr(r, "_race_commit_for_testing")
            raced: list[bool] = []
            helper = threading.Thread(target=lambda: raced.append(race(signal.SIGUSR1)))
            helper.start()
            with pytest.raises(Interrupt):
                r.read_batch(1024)
            helper.join()
            assert raced == [True]
            assert r.position == 0 and capacity() > RETAINED_CAPACITY
            assert len(r.read_batch(24)) == 24
            assert r.position == 24 and capacity() > RETAINED_CAPACITY
            assert len(r.read_batch(2000)) == 1000  # the rest of the staged batch, and nothing more
            assert r.position == 1024 and capacity() == 0
    finally:
        signal.signal(signal.SIGUSR1, previous)


@pytest.mark.parametrize("payload_len", [64, 200])
def test_common_batches_reuse_their_staging_buffer(journal: Path, payload_len: int) -> None:
    """`read_batch(10_000)` keeps its staging buffer for the next call instead of allocating a new one each time."""
    with tt.Writer(journal, segment_size=64 << 20) as w:
        w.append_many([bytes([i % 251]) * payload_len for i in range(50_000)])
    with tt.Reader(journal) as r:
        capacity = getattr(r, "_staging_capacity_for_testing")  # a test hook, not in the stubs
        assert len(r.read_batch(10_000)) == 10_000
        kept = capacity()
        assert 10_000 * (36 + payload_len) <= kept <= RETAINED_CAPACITY
        for _ in range(3):
            assert len(r.read_batch(10_000)) == 10_000
            assert capacity() == kept


def native_threads() -> int:
    """The number of OS threads in this process (not only the ones `threading` knows about)."""
    if sys.platform == "linux":
        return len(os.listdir("/proc/self/task"))
    if sys.platform == "darwin":
        import ctypes
        info = ctypes.create_string_buffer(96)  # struct proc_taskinfo
        libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
        assert libproc.proc_pidinfo(os.getpid(), 4, 0, info, 96) == 96  # PROC_PIDTASKINFO
        return int.from_bytes(info.raw[84:88], "little")  # pti_threadnum
    pytest.skip("counts threads only on Linux and macOS")


@pytest.mark.parametrize("call", ["read_batch", "read_many", "next"])
def test_freeing_a_large_delivered_batch_starts_no_thread(journal: Path, call: str) -> None:
    """A reader frees a staging buffer too large to keep before the read returns, in the calling thread.

    A thread that freed it would usually be gone when the count is taken, so the read is repeated: a binding that
    starts one per read is caught with near certainty (an earlier version was, in every trial, within 70 reads).
    """
    with tt.Writer(journal, segment_size=64 << 20) as w:
        w.append_many([b"x" * 7168] * 1024)  # 7 MiB: more than RETAINED_CAPACITY
        w.append(b"y" * (7 << 20))
    with tt.Reader(journal) as r:
        before = native_threads()
        for attempt in range(200):
            if call == "next":
                r.seek(1024)
                record = r.next(timeout=0)
                assert record is not None and len(record.payload) == 7 << 20
            else:
                r.seek(0)
                assert len(getattr(r, call)(1024)) == 1024
            assert native_threads() == before, f"a thread was running after read {attempt}"
