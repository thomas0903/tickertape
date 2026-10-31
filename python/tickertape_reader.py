"""Read-only, buffered tickertape format v1 decoder (Python 3.9+, no dependencies).

Live reads are observations during a scan, not an atomic snapshot. A zero commit word
ends the scan; validation failures reopen and retry the segment position once.
See README.md and ../docs/format.md for the contract and byte layout.
"""

import argparse
import base64
import json
import os
from pathlib import Path
import struct
import sys
from typing import Iterator, NamedTuple, Optional


def _make_crc32c_table():
    table = []
    for byte in range(256):
        value = byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
        table.append(value)
    return tuple(table)


_CRC32C_TABLE = _make_crc32c_table()


def _crc32c_python(data: bytes) -> int:
    value = 0xFFFFFFFF
    for byte in data:
        value = _CRC32C_TABLE[(value ^ byte) & 0xFF] ^ (value >> 8)
    return value ^ 0xFFFFFFFF


try:
    from crc32c import crc32c as _crc32c
except ImportError:
    _crc32c = _crc32c_python


class Record(NamedTuple):
    seq: int
    schema_id: int
    venue_ts_ns: int
    local_ts_ns: int
    payload: bytes


class Checkpoint(NamedTuple):
    generation: int
    next_seq: int
    segment_base_seq: int
    offset: int


class CorruptJournal(Exception):
    """Persistent invalid bytes/chain; offset names the frame start (0 for headers)."""

    def __init__(self, path, offset: int, reason: str):
        self.path = Path(path)
        self.offset = offset
        self.reason = reason
        super().__init__(f"{self.path}:{offset}: {reason}")


def read_checkpoint(path) -> Optional[Checkpoint]:
    """Read a journal directory's marker (or a marker path); ignore invalid slots.

    Return the valid slot with highest generation, or None if neither is valid or
    the file is missing. Other I/O errors propagate. Read no journal data for writing.
    """
    path = Path(path)
    if path.is_dir():
        path = path / "checkpoint"
    candidates = []
    try:
        with path.open("rb") as stream:
            for offset in (0, 65536):
                stream.seek(offset)
                slot = stream.read(64)
                if (len(slot) == 64 and slot[:8] == b"TICKCKPT"
                        and struct.unpack_from("<H", slot, 8)[0] == 1
                        and slot[10:16] == bytes(6) and slot[48:60] == bytes(12)
                        and _crc32c(slot[:60]) == struct.unpack_from("<I", slot, 60)[0]):
                    candidates.append(Checkpoint(*struct.unpack_from("<QQQQ", slot, 16)))
    except FileNotFoundError:
        return None
    # Highest generation wins; on a tie (never written by tickertape) the slot at offset 65,536 wins, as in Rust.
    best = None
    for candidate in candidates:
        if best is None or candidate.generation >= best.generation:
            best = candidate
    return best


def _validate_header(header, path, file_size, base):
    def require(condition, reason):
        if not condition:
            raise CorruptJournal(path, 0, reason)

    require(len(header) == 64, "short segment header")
    magic, version, length, flags, size, stored_base = struct.unpack_from("<8sHHIQQ", header)
    require(magic == b"TICKTAPE", "invalid segment magic")
    require(version == 1, "unsupported format version")
    require(length == 64, "invalid header length")
    require(flags == 0, "nonzero header flags")
    require(header[40:60] == bytes(20), "nonzero header reserved bytes")
    require(_crc32c(header[:60]) == struct.unpack_from("<I", header, 60)[0], "header CRC mismatch")
    require(65536 <= size <= (1 << 40) and size % 65536 == 0, "invalid segment size")
    require(size == file_size, "segment size differs from file size")
    require(stored_base == base, "header base sequence differs from file name")
    return size


def _read_frame(stream, path, size, offset, expected):
    """Return (record or None, is_seal, next_offset), validating before returning."""
    def invalid(reason):
        raise CorruptJournal(path, offset, reason)

    if offset < 64 or offset % 8 or offset > size - 8:
        invalid("frame offset out of bounds")
    stream.seek(offset)
    word = stream.read(4)  # Observe the commit word before reading the body.
    if len(word) != 4:
        invalid("short commit word")
    length = struct.unpack("<I", word)[0]
    if length == 0:
        return None, False, offset
    if length == 0xFFFFFFFF:
        if offset == 64:
            invalid("seal in empty segment")
        checksum = stream.read(4)
        if len(checksum) != 4:
            invalid("short seal")
        if _crc32c(word + struct.pack("<Q", expected)) != struct.unpack("<I", checksum)[0]:
            invalid("seal CRC mismatch")
        return None, True, offset + 8
    aligned = (length + 7) & ~7
    if length < 40 or offset + aligned > size - 8:
        invalid("record length out of bounds (including padding/trailer)")
    body = stream.read(length - 4)
    if len(body) != length - 4:
        invalid("short record body")
    checksum = struct.unpack_from("<I", body)[0]
    if _crc32c(word + body[4:]) != checksum:
        invalid("record CRC mismatch")
    seq, schema_id, flags, venue, local = struct.unpack_from("<QIIQQ", body, 4)
    if seq != expected or seq == 0xFFFFFFFFFFFFFFFF:
        invalid(f"unexpected sequence {seq}; expected {expected} (records cannot use u64::MAX)")
    if flags != 0:
        invalid("nonzero record flags")
    return Record(seq, schema_id, venue, local, body[36:]), False, offset + aligned


class _Segment:
    """Keep a buffered stream, reopening only on a validation failure."""

    def __init__(self, path, base):
        self.path = path
        self.base = base
        self.stream = None
        self.identity = None
        self.size = None

    def close(self):
        if self.stream is not None:
            self.stream.close()
            self.stream = None

    def check_identity(self, offset):
        try:
            stat = self.path.stat()
        except FileNotFoundError:
            raise CorruptJournal(self.path, offset, "segment removed during scan") from None
        if (stat.st_dev, stat.st_ino) != self.identity:
            raise CorruptJournal(self.path, offset, "segment replaced during scan; restart after recovery")

    def read(self, offset, expected, successor):
        for attempt in range(2):
            try:
                if self.stream is None:
                    if self.identity is not None:
                        self.check_identity(offset)
                    self.stream = self.path.open("rb")
                    stat = os.fstat(self.stream.fileno())
                    identity = (stat.st_dev, stat.st_ino)
                    if self.identity is not None and identity != self.identity:
                        raise CorruptJournal(self.path, offset, "segment replaced during scan")
                    self.identity = identity
                    self.size = _validate_header(self.stream.read(64), self.path, stat.st_size, self.base)
                result = _read_frame(self.stream, self.path, self.size, offset, expected)
                if result[1]:
                    self.check_identity(offset)
                    if successor != expected:
                        raise CorruptJournal(self.path, offset,
                                             f"seal successor {expected:020}.tape missing or disconnected")
                elif result[0] is None and successor is not None:
                    raise CorruptJournal(self.path, offset, "empty slot before newest segment (missing seal)")
                return result
            except CorruptJournal:
                self.close()
                if attempt:
                    raise


def read_journal(path, start_seq=None, end_seq=None, durable_only=False) -> Iterator[Record]:
    """Yield validated records in an inclusive range, reading files only in 'rb' mode.

    Scan from sequence zero, validating the prefix before start_seq. Capture segment
    names and (if requested) the checkpoint once when iteration begins. With no valid
    checkpoint, durable_only yields nothing. End at the newest segment's empty slot
    or the requested boundary. A live append may be that empty slot; invoke again for
    later records. CorruptJournal is raised only after one retry of a failing segment
    read. This is not an atomic snapshot or a reader that follows live appends.
    """
    for name, value in (("start_seq", start_seq), ("end_seq", end_seq)):
        if value is not None and (not isinstance(value, int) or not 0 <= value <= 0xFFFFFFFFFFFFFFFF):
            raise ValueError(f"{name} must be an unsigned 64-bit integer")
    if start_seq is not None and end_seq is not None and end_seq < start_seq:
        raise ValueError("end_seq must be greater than or equal to start_seq")
    path = Path(path)
    segments = sorted((int(entry.name[:20]), entry) for entry in path.iterdir()
                      if len(entry.name) == 25 and entry.name[20:] == ".tape"
                      and all("0" <= char <= "9" for char in entry.name[:20]))
    limit = None if end_seq is None else end_seq + 1
    if durable_only:
        checkpoint = read_checkpoint(path)
        durable_end = 0 if checkpoint is None else checkpoint.next_seq
        limit = durable_end if limit is None else min(limit, durable_end)
    if not segments or limit == 0:
        return
    # Like the Rust reader, start at the oldest segment present: operators may archive or delete old segments.
    expected = segments[0][0]
    if start_seq is not None and start_seq < expected:
        raise ValueError(f"sequence {start_seq} is before the first available record {expected}")
    for index, (base, segment_path) in enumerate(segments):
        segment = _Segment(segment_path, base)
        offset = 64
        successor = segments[index + 1][0] if index + 1 < len(segments) else None
        try:
            while limit is None or expected < limit:
                record, sealed, next_offset = segment.read(offset, expected, successor)
                if sealed:
                    break
                if record is None:
                    return
                expected += 1
                offset = next_offset
                if start_seq is None or record.seq >= start_seq:
                    yield record
            else:
                return  # Reached the range/checkpoint cap without reading uncovered bytes.
        finally:
            segment.close()


def _json_line(record):
    return json.dumps({"seq": record.seq, "schema_id": record.schema_id,
                       "venue_ts_ns": record.venue_ts_ns, "local_ts_ns": record.local_ts_ns,
                       "payload_b64": base64.b64encode(record.payload).decode("ascii")},
                      separators=(",", ":"))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("journal")
    parser.add_argument("--from", dest="start_seq", type=int, help="first sequence (inclusive)")
    parser.add_argument("--to", dest="end_seq", type=int, help="last sequence (inclusive)")
    parser.add_argument("--durable-only", action="store_true", help="stop at the startup checkpoint")
    args = parser.parse_args(argv)
    try:
        for record in read_journal(args.journal, args.start_seq, args.end_seq, args.durable_only):
            print(_json_line(record))
    except CorruptJournal as error:
        print(error, file=sys.stderr)
        return 2
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
