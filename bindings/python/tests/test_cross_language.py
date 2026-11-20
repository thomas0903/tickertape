"""Cross-language fixtures.

- Journals written by Rust (`tt-durable-only`, `demo-producer`) are read identically by the bindings and by the
  pure-Python decoder (`python/tickertape_reader.py`).
- Journals written by the bindings are read identically by the Rust reader (`tt-durable-only read` and
  `tickertape export`), by `tickertape verify`, and by the pure-Python decoder.

Each case covers payloads (including empty and segment-filling ones), metadata (including extreme values),
checksums, sequence numbers across segment rolls, and the durable-only boundary.
"""

from __future__ import annotations

import json
import struct
import subprocess
from pathlib import Path

import tickertape as tt
import tickertape_reader as pure
from helpers import SMALL_SEGMENT, append_pattern, as_tuples, jsonl, pattern_tuple

CHECKPOINTED, UNCHECKPOINTED = 700, 300


def bindings_read(journal: Path, durable_only: bool = False) -> list[tuple[int, int, int, int, bytes]]:
    with tt.Reader(journal, durable_only=durable_only) as r:
        return as_tuples(r)


def pure_read(journal: Path, durable_only: bool = False) -> list[tuple[int, int, int, int, bytes]]:
    return as_tuples(pure.read_journal(journal, durable_only=durable_only))


def cli_export(cli: Path, journal: Path, *args: str) -> bytes:
    return subprocess.check_output([str(cli), "export", str(journal), "--format", "jsonl", *args])


def cli_verify(cli: Path, journal: Path) -> dict[str, object]:
    result: dict[str, object] = json.loads(subprocess.check_output([str(cli), "verify", str(journal), "--json"]))
    return result


def crc32c(data: bytes) -> int:
    """Bitwise CRC32C, independent of both the Rust code and the pure-Python decoder's table."""
    value = 0xFFFFFFFF
    for byte in data:
        value ^= byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
    return value ^ 0xFFFFFFFF


def check_frames(segment: Path, expected: list[tuple[int, int, int, int, bytes]]) -> int:
    """Decode a segment's record frames by hand, check their CRC32C and fields, and return how many there were."""
    data = segment.read_bytes()
    offset, count = 64, 0
    while True:
        (length,) = struct.unpack_from("<I", data, offset)
        if length in (0, 0xFFFFFFFF):
            return count
        (stored_crc,) = struct.unpack_from("<I", data, offset + 4)
        assert crc32c(data[offset:offset + 4] + data[offset + 8:offset + length]) == stored_crc
        seq, schema_id, flags, venue, local = struct.unpack_from("<QIIQQ", data, offset + 8)
        assert flags == 0
        assert (seq, schema_id, venue, local, data[offset + 40:offset + length]) == expected[seq]
        offset += (length + 7) & ~7
        count += 1


def assert_segments_roll(journal: Path, total: int) -> None:
    segments = tt.list_segments(journal)
    assert len(segments) >= 3, "the fixture should span several segments"
    bases = [s.base_seq for s in segments]
    assert bases[0] == 0 and bases == sorted(bases) and bases[-1] < total


def test_rust_written_journal_is_read_identically(journal: Path, durable_helper: Path, cli: Path) -> None:
    subprocess.run([str(durable_helper), "write", str(journal), str(CHECKPOINTED), str(UNCHECKPOINTED)],
                   check=True)
    total = CHECKPOINTED + UNCHECKPOINTED
    expected = [pattern_tuple(s) for s in range(total)]
    assert_segments_roll(journal, total)
    assert bindings_read(journal) == expected
    assert pure_read(journal) == expected
    assert bindings_read(journal, durable_only=True) == expected[:CHECKPOINTED]
    assert pure_read(journal, durable_only=True) == expected[:CHECKPOINTED]
    # Sequence numbers at every segment boundary, through seek in the bindings and bounds in the decoder.
    for segment in tt.list_segments(journal)[1:]:
        base = segment.base_seq
        with tt.Reader(journal) as r:
            r.seek(base - 1)
            assert as_tuples(r.read_many(2)) == expected[base - 1:base + 1]
        assert as_tuples(pure.read_journal(journal, start_seq=base - 1, end_seq=base)) == expected[base - 1:base + 1]
    assert pure.read_checkpoint(journal) == tuple(tt.read_checkpoint(journal))  # type: ignore[arg-type]
    assert cli_export(cli, journal) == jsonl(expected)
    batch_rows = []
    with tt.Reader(journal) as r:
        batch = r.read_batch(total)
        batch_rows = as_tuples(batch.records())
    assert batch_rows == expected


def test_demo_producer_journal_is_read_identically(journal: Path, producer: Path, cli: Path) -> None:
    subprocess.run([str(producer), "--dir", str(journal), "--count", "20000", "--rate", "1000000"],
                   check=True, capture_output=True)
    records = bindings_read(journal)
    assert len(records) == 20000
    assert records == pure_read(journal)
    assert bindings_read(journal, durable_only=True) == records  # the producer checkpoints at the end
    assert cli_export(cli, journal) == jsonl(records)


def test_bindings_written_journal_is_read_identically(journal: Path, durable_helper: Path, cli: Path) -> None:
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        append_pattern(w, CHECKPOINTED)
        w.checkpoint()
        append_pattern(w, UNCHECKPOINTED)
        # Closing does not checkpoint the suffix.
    total = CHECKPOINTED + UNCHECKPOINTED
    expected = [pattern_tuple(s) for s in range(total)]
    assert_segments_roll(journal, total)
    # The Rust reader checks every field of every durable record and that nothing after the checkpoint is returned.
    subprocess.run([str(durable_helper), "read", str(journal), str(CHECKPOINTED)], check=True)
    assert cli_export(cli, journal) == jsonl(expected)
    assert cli_export(cli, journal, "--durable-only") == jsonl(expected[:CHECKPOINTED])
    assert cli_export(cli, journal, "--from", "299", "--to", "301") == jsonl(expected[299:302])
    report = cli_verify(cli, journal)
    assert (report["records"], report["first_seq"], report["last_seq"], report["problems"]) == (
        total, 0, total - 1, [])
    assert report["segments"] == len(tt.list_segments(journal))
    assert pure_read(journal) == expected
    assert pure_read(journal, durable_only=True) == expected[:CHECKPOINTED]
    checkpoint = pure.read_checkpoint(journal)
    assert checkpoint is not None and checkpoint.next_seq == CHECKPOINTED
    frames = sum(check_frames(s.path, expected) for s in tt.list_segments(journal))
    assert frames == total


def test_edge_payloads_and_metadata(journal: Path, cli: Path) -> None:
    sizes = [0, 1, 7, 8, 9, 63, 64, 65, 4095, 4096, 4097]
    payloads = [bytes((i * 7 + n) & 0xFF for n in range(size)) for i, size in enumerate(sizes)]
    largest = SMALL_SEGMENT - 64 - 40 - 8  # header, record header, room for a seal
    with tt.Writer(journal, segment_size=SMALL_SEGMENT) as w:
        w.append_many(payloads, schema_id=[0, 2**32 - 1] * 5 + [1],
                      venue_ts_ns=[2**64 - 1] * len(sizes), local_ts_ns=list(range(len(sizes))))
        big = w.append(b"\xa5" * largest, schema_id=9)  # fills a fresh segment
        tail = w.append(b"after the big one", schema_id=10)
        w.checkpoint()
    expected = [(i, [0, 2**32 - 1][i % 2] if i < 10 else 1, 2**64 - 1, i, p) for i, p in enumerate(payloads)]
    expected += [(big, 9, 0, 0, b"\xa5" * largest), (tail, 10, 0, 0, b"after the big one")]
    assert bindings_read(journal) == expected
    assert pure_read(journal) == expected
    assert pure_read(journal, durable_only=True) == expected
    assert cli_export(cli, journal) == jsonl(expected)
    assert cli_verify(cli, journal)["problems"] == []
