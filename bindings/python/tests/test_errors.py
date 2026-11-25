"""The exception hierarchy, its completeness, and corruption surfaced as exceptions."""

from __future__ import annotations

import inspect
import os
import pickle
import re
import struct
from pathlib import Path

import pytest

import tickertape as tt
from tickertape import _native
from helpers import SMALL_SEGMENT, append_pattern, pattern_tuple

from conftest import REPO

CORE_SRC = REPO / "crates" / "tickertape" / "src"
ERROR_RS = CORE_SRC / "error.rs"


def enum_variants(source: str, name: str) -> list[str]:
    """Variant names of `pub enum <name>` in a rustfmt-formatted file (four-space indented, capitalized)."""
    body = source.split(f"pub enum {name} {{", 1)[1]
    body = body[: body.index("\n}\n")]
    return re.findall(r"^    ([A-Z][A-Za-z0-9]*)\b", body, re.MULTILINE)


def test_every_rust_variant_is_listed() -> None:
    """Fails when the core crate gains an `Error` or `CorruptReason` variant that the bindings don't list.

    The Rust unit tests in `src/errors.rs` check that every listed variant converts to the listed class or code."""
    source = ERROR_RS.read_text()
    errors = enum_variants(source, "Error")
    reasons = enum_variants(source, "CorruptReason")
    assert len(errors) >= 11 and len(reasons) >= 13, "the parser no longer understands error.rs"
    mapped_errors = dict(_native.ERROR_VARIANTS)
    mapped_reasons = dict(_native.CORRUPT_REASONS)
    assert sorted(errors) == sorted(mapped_errors), "update ERROR_VARIANTS and try_to_py in src/errors.rs"
    assert sorted(reasons) == sorted(mapped_reasons), "update CORRUPT_REASONS and reason_fields in src/errors.rs"
    for class_name in mapped_errors.values():
        cls = getattr(tt, class_name)
        assert issubclass(cls, tt.TickertapeError)
        assert cls.__module__ == "tickertape"
    assert len(set(mapped_reasons.values())) == len(mapped_reasons)


def test_every_rust_enum_variant_of_the_reports_is_listed() -> None:
    """The same for `RecoveryAction` (`RecoveryAction.kind`) and `SegmentEnd` (`SegmentReport.end`); the Rust unit
    tests in `src/reports.rs` check the conversion of every listed variant."""
    actions = enum_variants((CORE_SRC / "writer.rs").read_text(), "RecoveryAction")
    ends = enum_variants((CORE_SRC / "inspect.rs").read_text(), "SegmentEnd")
    assert len(actions) >= 7 and len(ends) >= 3, "the parser no longer understands the sources"
    assert sorted(actions) == sorted(dict(_native.RECOVERY_ACTIONS)), "update RECOVERY_ACTIONS in src/reports.rs"
    assert sorted(ends) == sorted(dict(_native.SEGMENT_ENDS)), "update SEGMENT_ENDS in src/reports.rs"
    # Every code is documented where the named tuple field is declared.
    source = Path(tt._inspect.__file__).read_text()
    for _, code in _native.RECOVERY_ACTIONS + _native.SEGMENT_ENDS:
        assert f'``"{code}"``' in source, code


def test_every_rust_status_enum_variant_is_listed() -> None:
    """The same for the status enums (ADR 0032); the Rust unit tests in `src/status.rs` check every listed variant's
    code, and every code is documented where the named tuple field is declared."""
    source = (CORE_SRC / "status.rs").read_text()
    listed = dict(_native.STATUS_ENUMS)
    assert sorted(listed) == sorted(
        ["HeadEnd", "CheckpointProblem", "AgeUnknown", "LagUnknown", "BeaconState", "BeaconDirState"]
    )
    documented = Path(tt._status.__file__).read_text()
    for name, variants in listed.items():
        found = enum_variants(source, name)
        assert len(found) >= 3, f"the parser no longer understands status.rs ({name})"
        assert sorted(found) == sorted(dict(variants)), f"update STATUS_ENUMS in src/status.rs for {name}"
        for _, code in variants:
            assert f'``"{code}"``' in documented, code


def test_hierarchy_matches_builtins() -> None:
    assert issubclass(tt.JournalIOError, OSError)
    assert issubclass(tt.StorageFullError, tt.JournalIOError)
    assert issubclass(tt.RecordTooLargeError, ValueError)
    assert issubclass(tt.InvalidConfigError, ValueError)
    assert issubclass(tt.SeqUnavailableError, LookupError)
    for name in tt._errors.__all__:
        assert issubclass(getattr(tt, name), tt.TickertapeError)


def corrupt_byte(path: Path, offset: int) -> None:
    with path.open("r+b") as f:
        f.seek(offset)
        value = f.read(1)[0]
        f.seek(offset)
        f.write(bytes([value ^ 0xFF]))


def record_offsets(segment: Path) -> list[int]:
    """Offsets of the records in a segment, following each commit word (independently of the bindings)."""
    data = segment.read_bytes()
    offsets: list[int] = []
    offset = 64
    while True:
        (length,) = struct.unpack_from("<I", data, offset)
        if length in (0, 0xFFFFFFFF):
            return offsets
        offsets.append(offset)
        offset += (length + 7) & ~7


def test_corrupt_record_raises_and_does_not_advance(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 20)
    segment = tt.list_segments(journal)[0].path
    bad = record_offsets(segment)[10]
    corrupt_byte(segment, bad + 40)  # first payload byte of record 10 (record 10 has a payload)
    with tt.Reader(journal) as r:
        with pytest.raises(tt.CorruptJournalError) as raised:
            list(r)
        error = raised.value
        assert (error.path, error.offset, error.reason, error.details) == (segment, bad, "bad_checksum", {})
        assert r.position == 10
        with pytest.raises(tt.CorruptJournalError):
            r.try_next()  # retried, still corrupt
        assert r.position == 10
    report = tt.verify(journal)
    assert not report.ok and report.records == 10
    assert [(p.path, p.offset, p.reason) for p in report.problems] == [(segment, bad, "bad_checksum")]
    assert "checksum" in report.problems[0].message


def test_read_many_returns_records_before_an_error_then_raises(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 20)
    segment = tt.list_segments(journal)[0].path
    corrupt_byte(segment, record_offsets(segment)[5] + 8)  # the stored sequence number of record 5
    with tt.Reader(journal) as r:
        got = r.read_many(100)
        assert [x.as_tuple() for x in got] == [pattern_tuple(s) for s in range(5)]
        with pytest.raises(tt.CorruptJournalError) as raised:
            r.read_many(100)  # the held-back error
        assert raised.value.reason in ("bad_checksum", "bad_sequence")
        with pytest.raises(tt.CorruptJournalError):
            r.read_batch(100)  # retried: still corrupt
    with tt.Reader(journal) as r:
        batch = r.read_batch(100)
        assert len(batch) == 5
        r.seek(0)  # seek clears a held-back error
        assert len(r.read_many(3)) == 3


def test_emptied_commit_word_raises_instead_of_returning_none(journal: Path) -> None:
    """A zeroed commit word that the checkpoint covers (ADR 0050)."""
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 20)
        w.checkpoint()
    segment = next(journal.glob("*.tape"))
    at = record_offsets(segment)[10]
    data = bytearray(segment.read_bytes())
    data[at : at + 4] = bytes(4)
    segment.write_bytes(data)
    for durable_only in (False, True):
        with tt.Reader(journal, durable_only=durable_only) as r:
            for seq in range(10):
                assert r.try_next().seq == seq
            for read in (r.try_next, lambda: r.next(timeout=0.2), lambda: r.read_batch(10)):
                with pytest.raises(tt.CorruptJournalError) as raised:
                    read()
                assert raised.value.reason == "empty_commit_word"
                assert raised.value.offset == at
                assert raised.value.details["seq"] == 20
            assert r.position == 10


def test_corrupt_header_and_unsupported_version(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        w.append(b"a")
    segment = tt.list_segments(journal)[0].path
    corrupt_byte(segment, 0)
    with pytest.raises(tt.CorruptJournalError) as raised:
        tt.Reader(journal)
    assert raised.value.reason == "bad_magic" and raised.value.offset == 0
    corrupt_byte(segment, 0)  # restore
    with segment.open("r+b") as f:
        f.seek(8)
        f.write(struct.pack("<H", 2))
    with pytest.raises(tt.UnsupportedVersionError) as version:
        tt.Reader(journal)
    assert (version.value.found, version.value.supported) == (2, 1)


def test_fail_policy_refuses_corrupt_tail(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 5)
    segment = tt.list_segments(journal)[0].path
    corrupt_byte(segment, record_offsets(segment)[4] + 40)
    with pytest.raises(tt.CorruptJournalError):
        tt.Writer(journal, recovery="fail")
    with tt.Writer(journal) as w:  # the default policy quarantines the tail
        kinds = [a.kind for a in w.recovery_report.actions]
        assert "repaired_tail" in kinds
        repaired = [a for a in w.recovery_report.actions if a.kind == "repaired_tail"][0]
        assert isinstance(repaired, tt.RecoveryAction)
        assert repaired.reason == "bad_checksum" and repaired.quarantine_file is not None
        assert repaired.quarantine_file.exists() and repaired.segment == segment
        assert repaired.offset is not None and repaired.length is not None and repaired.file is None
        assert w.next_seq == 4


def test_discarded_after_recovery_replaces_segment(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, 5)
    segment = tt.list_segments(journal)[0].path
    reader = tt.Reader(journal)
    assert reader.try_next() is not None
    corrupt_byte(segment, record_offsets(segment)[4] + 40)
    with tt.Writer(journal) as w:  # repairs by replacing the segment file
        w.append(b"new history")
    with pytest.raises((tt.DiscardedError, tt.CorruptJournalError)) as raised:
        for _ in range(10):
            reader.next(timeout=0.15)
    if isinstance(raised.value, tt.DiscardedError):
        assert raised.value.seq >= 1
    reader.seek(0)
    assert [x.seq for x in reader] == [0, 1, 2, 3, 4]
    reader.close()


def test_exception_messages_and_attributes(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        with pytest.raises(tt.RecordTooLargeError) as raised:
            w.append(b"x" * SMALL_SEGMENT)
        assert raised.value.length == SMALL_SEGMENT and raised.value.max < SMALL_SEGMENT
        assert "exceeds the maximum" in str(raised.value)
        assert repr(raised.value).startswith("RecordTooLargeError(")


def sample_value(annotation: object) -> object:
    """A distinctive value of the annotated type."""
    text = str(annotation)
    if "Path" in text:
        return Path("/journal/00000000000000000000.tape")
    if "dict" in text:
        return {"expected": 5, "found": 7}
    if "bool" in text:
        return True
    if "str" in text:
        return "a value"
    return 12345


def sample_fields(cls: type[tt.TickertapeError]) -> dict[str, object]:
    """A distinctive value for every documented attribute of `cls` except `message`."""
    fields: dict[str, object] = {}
    for klass in reversed(cls.__mro__):
        if klass.__module__ == "tickertape":
            for attr, annotation in inspect.get_annotations(klass).items():
                if attr != "message":
                    fields[attr] = sample_value(annotation)
    if issubclass(cls, OSError):
        fields.update(errno=28, strerror=os.strerror(28), filename=Path("/journal"))
    return fields


def test_every_exception_survives_pickle_with_its_attributes() -> None:
    for name in tt._errors.__all__:
        cls = getattr(tt, name)
        fields = sample_fields(cls)
        original = cls("message text", **fields)
        original.add_note("a note")
        copy = pickle.loads(pickle.dumps(original))
        assert type(copy) is cls and str(copy) == "message text" and copy.message == "message text"
        for attr, value in fields.items():
            assert getattr(copy, attr) == value, (name, attr)
        assert copy.__notes__ == ["a note"]
        if issubclass(cls, OSError):
            assert (copy.errno, copy.strerror, copy.filename) == (28, os.strerror(28), Path("/journal"))


class UnprintableStr(str):
    """A `str` whose `__str__` raises."""

    def __str__(self) -> str:
        raise RuntimeError("no str for you")


class ClassRaises:
    """An object with a working `__str__` whose `__class__` attribute raises (`isinstance` reads it)."""

    def __str__(self) -> str:
        return "printable"

    @property  # type: ignore[misc]
    def __class__(self) -> type:
        raise RuntimeError("no class for you")


class StrClassRaises(str):
    """A `str` whose `__class__` attribute raises."""

    @property  # type: ignore[misc]
    def __class__(self) -> type:
        raise RuntimeError("no class for you")


def test_pickle_tolerates_deleted_attributes() -> None:
    """Deleting any attribute, on any exception class, does not break pickling; the copy lacks it too."""
    # With the message deleted, the copy's arguments come from the original's: a str is copied without calling a
    # subclass's __str__, and anything else is replaced by a fixed text.
    for message, expected in ((UnprintableStr("message text"), "message text"), (b"bytes", "(message deleted)"),
                              (ClassRaises(), "(message deleted)"), (StrClassRaises("text"), "text")):
        original = tt.TickertapeError("placeholder")
        original.args = (message,)
        del original.message
        copy = pickle.loads(pickle.dumps(original))
        assert type(copy) is tt.TickertapeError and not hasattr(copy, "message")
        assert copy.args == (expected,) and type(copy.args[0]) is str
    checked = 0
    for name in tt._errors.__all__:
        cls = getattr(tt, name)
        fields = sample_fields(cls)
        for deleted in [*fields, "message"]:
            original = cls("message text", **fields)
            delattr(original, deleted)
            copy = pickle.loads(pickle.dumps(original))
            assert type(copy) is cls, (name, deleted)
            if deleted in ("errno", "strerror", "filename"):
                assert getattr(copy, deleted) is None, (name, deleted)  # OSError reads a deleted one as None
            else:
                assert not hasattr(copy, deleted), (name, deleted)
            for attr, value in fields.items():
                if attr != deleted:
                    assert getattr(copy, attr) == value, (name, deleted, attr)
            if deleted != "message":
                assert copy.message == "message text"
            checked += 1
    assert checked > 2 * len(tt._errors.__all__)


def test_pickle_keeps_the_current_attribute_values() -> None:
    error = tt.BatchAppendError("batch failed", appended=3, first_seq=10, failed_index=3)
    error.failed_index = 7
    error.first_seq = None
    setattr(error, "extra", "added later")
    copy = pickle.loads(pickle.dumps(error))
    assert (copy.appended, copy.first_seq, copy.failed_index, getattr(copy, "extra")) == (3, None, 7, "added later")
    error.message = "changed message"
    assert pickle.loads(pickle.dumps(error)).message == "changed message"
    io_error = tt.JournalIOError("io failed", op="open", storage_full=False, errno=2, strerror="gone",
                                 filename=Path("/a"))
    io_error.op = "changed"
    io_error.filename = Path("/b")
    copy = pickle.loads(pickle.dumps(io_error))
    assert (copy.op, copy.errno, copy.strerror, copy.filename) == ("changed", 2, "gone", Path("/b"))


def test_real_io_error_survives_pickle_and_has_the_os_message(journal: Path) -> None:
    with pytest.raises(tt.JournalIOError) as raised:
        tt.Reader(journal / "missing")
    error = raised.value
    assert error.errno is not None and error.strerror == os.strerror(error.errno)
    assert "os error" not in error.strerror
    copy = pickle.loads(pickle.dumps(error))
    assert (copy.errno, copy.strerror, copy.filename, copy.op, copy.storage_full, str(copy)) == (
        error.errno, error.strerror, error.filename, error.op, error.storage_full, str(error))


def test_seek_that_fails_keeps_the_held_back_error(journal: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT, retention=tt.Retention.delete(max_segments=2)) as w:
        append_pattern(w, 2000)
        w.checkpoint()
    segments = tt.list_segments(journal)
    first, last = segments[0].base_seq, segments[-1]
    assert first > 0
    bad = record_offsets(last.path)[5]
    corrupt_byte(last.path, bad + 8)
    with tt.Reader(journal) as r:
        r.seek(last.base_seq)
        assert len(r.read_many(100)) == 5  # the error at the sixth record is held back
        with pytest.raises(tt.SeqUnavailableError):
            r.seek(0)
        corrupt_byte(last.path, bad + 8)  # repaired: a retry would now succeed
        with pytest.raises(tt.CorruptJournalError):
            r.read_many(100)  # the failed seek did not discard the held-back error
        assert r.read_many(1)[0].seq == last.base_seq + 5
