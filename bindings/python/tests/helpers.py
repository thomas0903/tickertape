"""Helpers shared by the tests: the deterministic record pattern of the Rust test helpers, and small utilities."""

from __future__ import annotations

import base64
import json
from collections.abc import Iterable
from typing import Any

import tickertape as tt

SMALL_SEGMENT = 65536
"""The Rust tests' `SMALL_SEGMENT`: small enough that a few hundred records roll segments."""


def pattern_payload(seq: int) -> bytes:
    """`tickertape_tests::payload(seq, payload_len(seq))`: 0 to 300 bytes derived from `seq`."""
    length = (seq * 37) % 301
    return bytes(((seq * 31) + i) & 0xFF for i in range(length))


def pattern_tuple(seq: int) -> tuple[int, int, int, int, bytes]:
    """The record `tickertape_tests::append_n` writes at `seq`, as `(seq, schema_id, venue, local, payload)`."""
    return (seq, seq % 7, seq * 10, seq * 10 + 1, pattern_payload(seq))


def append_pattern(writer: tt.Writer, count: int) -> None:
    """Append `count` pattern records in one batch (the same records as `append_n`)."""
    start = writer.next_seq
    seqs = range(start, start + count)
    got = writer.append_many(
        [pattern_payload(s) for s in seqs],
        schema_id=[s % 7 for s in seqs],
        venue_ts_ns=[s * 10 for s in seqs],
        local_ts_ns=[s * 10 + 1 for s in seqs],
    )
    assert got == seqs


def as_tuples(records: Iterable[Any]) -> list[tuple[int, int, int, int, bytes]]:
    """Normalize binding `Record`s and decoder `Record`s (named tuples) to plain tuples."""
    out = []
    for r in records:
        out.append(r.as_tuple() if isinstance(r, tt.Record) else tuple(r))
    return out


def jsonl(records: Iterable[tuple[int, int, int, int, bytes]]) -> bytes:
    """Records in the exact `tickertape export --format jsonl` form."""
    lines = []
    for seq, schema_id, venue, local, payload in records:
        lines.append(json.dumps({"seq": seq, "schema_id": schema_id, "venue_ts_ns": venue, "local_ts_ns": local,
                                 "payload_b64": base64.b64encode(payload).decode("ascii")}, separators=(",", ":")))
    return b"".join(line.encode() + b"\n" for line in lines)
