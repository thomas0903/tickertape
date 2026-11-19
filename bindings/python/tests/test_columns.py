"""Columnar batches: layout, lifetimes and optional conversions to NumPy, Arrow and pandas."""

from __future__ import annotations

import gc
import struct
import sys
from pathlib import Path

import pytest

import tickertape as tt
from helpers import SMALL_SEGMENT, append_pattern, pattern_tuple


def write(journal: Path, count: int) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, count)


def test_layout_is_little_endian_and_arrow_compatible(journal: Path) -> None:
    write(journal, 300)
    with tt.Reader(journal) as r:
        r.seek(10)
        batch = r.read_batch(200)
        rest = r.read_batch(1000)
        assert r.position == 300
    assert len(batch) == 200 and len(rest) == 90
    n = len(batch)
    seqs = struct.unpack(f"<{n}Q", batch.seq)
    schema_ids = struct.unpack(f"<{n}I", batch.schema_id)
    venue = struct.unpack(f"<{n}Q", batch.venue_ts_ns)
    local = struct.unpack(f"<{n}Q", batch.local_ts_ns)
    offsets = struct.unpack(f"<{n + 1}q", batch.payload_offsets)
    assert offsets[0] == 0 and offsets[-1] == len(batch.payload_data)
    for i in range(n):
        payload = batch.payload_data[offsets[i]:offsets[i + 1]]
        assert (seqs[i], schema_ids[i], venue[i], local[i], payload) == pattern_tuple(10 + i)
    assert batch[0].as_tuple() == pattern_tuple(10)
    assert batch[-1].as_tuple() == pattern_tuple(209)
    with pytest.raises(IndexError):
        batch[200]
    assert [x.seq for x in batch.records()] == list(range(10, 210))
    assert "200 records" in repr(batch)


def test_empty_batch(journal: Path) -> None:
    write(journal, 1)
    with tt.Reader(journal) as r:
        r.seek(1)
        batch = r.read_batch(10)
    assert len(batch) == 0 and batch.payload_offsets == b"\0" * 8 and batch.records() == []
    assert "empty" in repr(batch)


def test_batches_iterate_and_refuse_slices(journal: Path) -> None:
    write(journal, 5)
    with tt.Reader(journal) as r:
        batch = r.read_batch(10)
    records = [record for record in batch]  # mypy --strict accepts iteration (__iter__ is declared)
    assert [x.seq for x in records] == list(range(5)) and records == batch.records()
    it = iter(batch)
    assert iter(it) is it and len(list(it)) == 5 and list(it) == []
    with pytest.raises(TypeError, match="does not support slicing"):
        batch[1:3]  # type: ignore[index]
    with pytest.raises(IndexError):
        batch[2**100]
    assert batch[-5].seq == 0 and batch[4].seq == 4


def test_empty_batch_conversions(journal: Path) -> None:
    write(journal, 1)
    with tt.Reader(journal) as r:
        r.seek(1)
        batch = r.read_batch(10)
    assert list(batch) == []
    np = pytest.importorskip("numpy")
    columns = batch.to_numpy()
    assert sorted(columns) == sorted(["seq", "schema_id", "venue_ts_ns", "local_ts_ns", "payload"])
    assert all(len(column) == 0 for column in columns.values()) and columns["seq"].dtype == np.dtype("<u8")
    pa = pytest.importorskip("pyarrow")
    table = batch.to_arrow()
    assert table.num_rows == 0 and table.schema.field("payload").type == pa.large_binary()
    pytest.importorskip("pandas")
    frame = batch.to_pandas()
    assert len(frame) == 0 and list(frame.columns) == ["seq", "schema_id", "venue_ts_ns", "local_ts_ns", "payload"]


def test_numpy_arrays_share_and_outlive_the_batch(journal: Path) -> None:
    np = pytest.importorskip("numpy")
    write(journal, 100)
    with tt.Reader(journal) as r:
        batch = r.read_batch(100)
    seq_bytes = batch.seq
    columns = batch.to_numpy()
    assert columns["seq"].dtype == np.dtype("<u8") and columns["schema_id"].dtype == np.dtype("<u4")
    assert not columns["seq"].flags.writeable  # views of immutable bytes
    assert columns["seq"].base is seq_bytes or bytes(columns["seq"].base) == seq_bytes
    del batch, seq_bytes
    gc.collect()
    # The arrays keep their buffers alive after the batch is gone.
    assert columns["seq"].tolist() == list(range(100))
    assert columns["venue_ts_ns"][5] == 50
    assert columns["payload"][7] == pattern_tuple(7)[4]


def test_arrow_table(journal: Path) -> None:
    pa = pytest.importorskip("pyarrow")
    write(journal, 100)
    with tt.Reader(journal) as r:
        table = r.read_batch(100).to_arrow()
    gc.collect()
    assert table.schema.names == ["seq", "schema_id", "venue_ts_ns", "local_ts_ns", "payload"]
    assert table.schema.field("payload").type == pa.large_binary()
    assert table.schema.field("schema_id").type == pa.uint32()
    rows = list(zip(*(table.column(name).to_pylist() for name in table.schema.names)))
    assert rows == [pattern_tuple(s) for s in range(100)]


def test_pandas_and_duckdb(journal: Path) -> None:
    pytest.importorskip("pandas")
    write(journal, 100)
    with tt.Reader(journal) as r:
        frame = r.read_batch(100).to_pandas()
    assert list(frame.columns) == ["seq", "schema_id", "venue_ts_ns", "local_ts_ns", "payload"]
    assert frame["schema_id"].value_counts().sum() == 100
    assert frame.loc[3, "payload"] == pattern_tuple(3)[4]
    duckdb = pytest.importorskip("duckdb")
    counts = duckdb.sql("SELECT schema_id, count(*) AS n FROM frame GROUP BY schema_id ORDER BY schema_id").fetchall()
    assert sum(n for _, n in counts) == 100 and counts[0][0] == 0


def test_missing_optional_package_is_explained(journal: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    write(journal, 1)
    with tt.Reader(journal) as r:
        batch = r.read_batch(1)
    monkeypatch.setitem(sys.modules, "pyarrow", None)
    with pytest.raises(ImportError, match="optional package 'pyarrow'"):
        batch.to_arrow()
