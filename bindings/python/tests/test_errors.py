"""The exception hierarchy, its completeness, and corruption surfaced as exceptions."""

from __future__ import annotations

import re
import struct
from pathlib import Path

import pytest

import tickertape as tt
from tickertape import _native
from helpers import SMALL_SEGMENT, append_pattern, pattern_tuple

from conftest import REPO

ERROR_RS = REPO / "crates" / "tickertape" / "src" / "error.rs"


def enum_variants(source: str, name: str) -> list[str]:
    """Variant names of `pub enum <name>` in a rustfmt-formatted file (four-space indented, capitalized)."""
    body = source.split(f"pub enum {name} {{", 1)[1]
    body = body[: body.index("\n}\n")]
    return re.findall(r"^    ([A-Z][A-Za-z0-9]*)\b", body, re.MULTILINE)


def test_every_rust_variant_is_mapped() -> None:
    """Fails when the core crate gains an `Error` or `CorruptReason` variant that the bindings don't map."""
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
        kinds = [a["kind"] for a in w.recovery_report["actions"]]
        assert "repaired_tail" in kinds
        repaired = [a for a in w.recovery_report["actions"] if a["kind"] == "repaired_tail"][0]
        assert repaired["reason"] == "bad_checksum" and Path(repaired["quarantine_file"]).exists()
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
