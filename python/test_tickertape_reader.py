"""Independent format fixtures plus optional comparisons with the real Rust writer."""

import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import tickertape_reader as reader


def crc(data):
    # Deliberately bitwise, independent of the decoder's lookup table/optional package.
    value = 0xFFFFFFFF
    for byte in data:
        value ^= byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
    return value ^ 0xFFFFFFFF


def frame(seq, payload=b"abc", flags=0, schema_id=7, venue=11, local=13):
    length = 40 + len(payload)
    body = struct.pack("<QIIQQ", seq, schema_id, flags, venue, local) + payload
    data = struct.pack("<II", length, crc(struct.pack("<I", length) + body)) + body
    return data + bytes((-length) % 8)


def segment(base=0, contents=b"", **fields):
    data = bytearray(65536)
    struct.pack_into("<8sHHIQQQ20s", data, 0,
                     fields.get("magic", b"TICKTAPE"), fields.get("version", 1),
                     fields.get("header_len", 64), fields.get("flags", 0),
                     fields.get("size", len(data)), fields.get("base", base),
                     0, fields.get("reserved", bytes(20)))
    struct.pack_into("<I", data, 60, crc(data[:60]))
    data[64:64 + len(contents)] = contents
    return data


def slot(generation, next_seq, base=0, offset=64, **fields):
    data = bytearray(64)
    struct.pack_into("<8sH6sQQQQ12s", data, 0, fields.get("magic", b"TICKCKPT"),
                     fields.get("version", 1), fields.get("reserved", bytes(6)),
                     generation, next_seq, base, offset, bytes(12))
    struct.pack_into("<I", data, 60, crc(data[:60]))
    return data


def marker(first, second):
    data = bytearray(131072)
    data[:len(first)] = first
    data[65536:65536 + len(second)] = second
    return data


class FormatTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name)

    def write_segment(self, base=0, contents=b"", **fields):
        path = self.path / f"{base:020}.tape"
        path.write_bytes(segment(base, contents, **fields))
        return path

    def assert_corrupt(self, offset, reason=None):
        with self.assertRaises(reader.CorruptJournal) as raised:
            list(reader.read_journal(self.path))
        self.assertEqual(raised.exception.offset, offset)
        self.assertEqual(raised.exception.path.parent, self.path)
        if reason:
            self.assertIn(reason, raised.exception.reason)

    def test_crc32c_known_answer(self):
        self.assertEqual(len(reader._CRC32C_TABLE), 256)
        for implementation in (reader._crc32c_python, reader._crc32c):
            self.assertEqual(implementation(b"123456789"), 0xE3069283)
            self.assertEqual(implementation(b""), 0)
            self.assertEqual(implementation(bytes(range(256))), crc(bytes(range(256))))

    def test_checkpoint_selection_and_missing(self):
        self.assertIsNone(reader.read_checkpoint(self.path))
        path = self.path / "checkpoint"
        for a, b, expected in ((slot(3, 10), slot(4, 20), 20),
                               (slot(5, 30), slot(4, 20), 30),
                               (bytes(64), bytes(64), None),
                               (slot(3, 10), slot(4, 20, version=2), 10),
                               (slot(3, 10, reserved=b"\1" * 6), slot(4, 20), 20),
                               (slot(3, 10, magic=b"BADCKPT!"), bytes(64), None)):
            with self.subTest(expected=expected):
                path.write_bytes(marker(a, b))
                result = reader.read_checkpoint(self.path)
                self.assertEqual(None if result is None else result.next_seq, expected)
                self.assertEqual(reader.read_checkpoint(path), result)
        damaged = bytearray(slot(6, 40))
        damaged[24] ^= 1
        path.write_bytes(marker(slot(5, 30), damaged))
        self.assertEqual(reader.read_checkpoint(self.path).next_seq, 30)
        path.write_bytes(slot(5, 30))  # A short second slot does not destroy the first.
        self.assertEqual(reader.read_checkpoint(self.path).generation, 5)
        path.write_bytes(bytes(63))
        self.assertIsNone(reader.read_checkpoint(self.path))

    def test_durable_only_and_startup_boundary(self):
        self.write_segment(contents=b"".join(frame(i) for i in range(4)))
        self.assertEqual(list(reader.read_journal(self.path, durable_only=True)), [])
        path = self.path / "checkpoint"
        path.write_bytes(marker(slot(1, 2, offset=160), bytes(64)))
        records = reader.read_journal(self.path, durable_only=True)
        self.assertEqual(next(records).seq, 0)
        path.write_bytes(marker(slot(1, 2), slot(2, 4)))
        self.assertEqual([r.seq for r in records], [1])
        self.assertEqual([r.seq for r in reader.read_journal(self.path)], [0, 1, 2, 3])
        self.assertEqual([r.seq for r in reader.read_journal(
            self.path, start_seq=1, end_seq=2, durable_only=True)], [1, 2])

    def test_record_fields_empty_payload_and_unsigned_values(self):
        self.write_segment(contents=frame(0, b"") + frame(
            1, b"\x00\xff", schema_id=(1 << 32) - 1, venue=(1 << 64) - 1, local=1 << 63))
        records = list(reader.read_journal(self.path))
        self.assertEqual(records[0], reader.Record(0, 7, 11, 13, b""))
        self.assertEqual(records[1].payload, b"\x00\xff")
        self.assertEqual(records[1][1:4], ((1 << 32) - 1, (1 << 64) - 1, 1 << 63))
        with self.assertRaises(AttributeError):
            records[0].seq = 2

    def test_headers(self):
        for fields in ({"magic": b"BADMAGIC"}, {"version": 2}, {"header_len": 32},
                       {"flags": 1}, {"size": 131072}, {"size": 65535},
                       {"size": 1 << 41}, {"base": 1}, {"reserved": b"\1" * 20}):
            with self.subTest(fields=fields):
                self.write_segment(**fields)
                self.assert_corrupt(0)
        path = self.write_segment()
        data = bytearray(path.read_bytes())
        data[32] ^= 1
        path.write_bytes(data)
        self.assert_corrupt(0, "CRC")
        path.write_bytes(bytes(63))
        self.assert_corrupt(0, "header")

    def test_record_validation_and_valid_prefix(self):
        for bad in (struct.pack("<I", 39), struct.pack("<I", 65536),
                    frame(9), frame(1, flags=1)):
            with self.subTest(bad=bad[:8]):
                self.write_segment(contents=frame(0) + bad)
                records = reader.read_journal(self.path)
                self.assertEqual(next(records).seq, 0)
                with self.assertRaises(reader.CorruptJournal) as raised:
                    next(records)
                self.assertEqual(raised.exception.offset, 112)
        damaged = bytearray(frame(1))
        damaged[40] ^= 1
        self.write_segment(contents=frame(0) + damaged)
        self.assert_corrupt(112, "CRC")
        # Range caps stop before later corruption, but the lower bound does not skip validation.
        self.assertEqual([r.seq for r in reader.read_journal(self.path, end_seq=0)], [0])
        with self.assertRaises(reader.CorruptJournal):
            list(reader.read_journal(self.path, start_seq=2))

    def test_seals_and_chain(self):
        seal = struct.pack("<II", 0xFFFFFFFF, crc(b"\xff" * 4 + struct.pack("<Q", 1)))
        self.write_segment(contents=frame(0) + seal)
        self.write_segment(1, frame(1))
        self.assertEqual([r.seq for r in reader.read_journal(self.path)], [0, 1])
        (self.path / f"{1:020}.tape").unlink()
        self.assert_corrupt(112, "successor")
        self.write_segment(2, frame(2))
        self.assert_corrupt(112, "successor")
        (self.path / f"{2:020}.tape").unlink()
        self.write_segment(contents=seal)
        self.assert_corrupt(64, "empty segment")
        self.write_segment(contents=frame(0) + b"\xff" * 8)
        self.assert_corrupt(112, "seal CRC")

    def test_seal_in_trailer(self):
        payload = bytes(65536 - 64 - 8 - 40)
        seal = struct.pack("<II", 0xFFFFFFFF, crc(b"\xff" * 4 + struct.pack("<Q", 1)))
        self.write_segment(contents=frame(0, payload) + seal)
        self.write_segment(1, frame(1))
        self.assertEqual([r.seq for r in reader.read_journal(self.path)], [0, 1])
        # Even an unpadded frame that fits cannot have its alignment overlap the trailer.
        path = self.write_segment(contents=frame(0, payload))
        with path.open("r+b") as out:
            out.seek(64)
            out.write(struct.pack("<I", len(payload) + 41))
        self.assert_corrupt(64, "bounds")

    def test_empty_and_disconnected_segments(self):
        self.assertEqual(list(reader.read_journal(self.path)), [])
        self.write_segment()
        (self.path / "00000000000000000001.tape.tmp").write_bytes(b"ignored")
        (self.path / "not-a-segment.tape").write_bytes(b"ignored")
        self.assertEqual(list(reader.read_journal(self.path)), [])
        self.write_segment(1)
        self.assert_corrupt(64, "empty slot")
        (self.path / f"{0:020}.tape").unlink()
        self.assert_corrupt(0, "first segment")

    def test_retry_once_without_duplicates(self):
        self.write_segment(contents=frame(0) + frame(1))
        original = reader._read_frame
        calls = []

        def torn(stream, path, size, offset, expected):
            calls.append(offset)
            if offset == 112 and calls.count(offset) == 1:
                raise reader.CorruptJournal(path, offset, "injected torn read")
            return original(stream, path, size, offset, expected)

        with patch.object(reader, "_read_frame", side_effect=torn):
            self.assertEqual([r.seq for r in reader.read_journal(self.path)], [0, 1])
        self.assertEqual(calls.count(112), 2)
        with patch.object(reader, "_read_frame", side_effect=reader.CorruptJournal(
                self.path / f"{0:020}.tape", 64, "persistent")) as failing:
            self.assert_corrupt(64, "persistent")
            self.assertEqual(failing.call_count, 2)

    def test_retry_header_and_repair_detection(self):
        self.write_segment(contents=frame(0))
        original = reader._validate_header
        with patch.object(reader, "_validate_header", side_effect=[
                reader.CorruptJournal(self.path, 0, "torn header"),
                original(segment(contents=frame(0))[:64], self.path, 65536, 0)]):
            self.assertEqual([r.seq for r in reader.read_journal(self.path)], [0])
        records = reader.read_journal(self.path)
        self.assertEqual(next(records).seq, 0)
        path = self.path / f"{0:020}.tape"
        replacement = self.path / "replacement"
        replacement.write_bytes(segment(contents=frame(0) + b"\xff" * 8))
        replacement.replace(path)
        # Force a validation failure in the old open file to exercise the inode guard on retry.
        with patch.object(reader, "_read_frame", side_effect=reader.CorruptJournal(
                path, 112, "injected")):
            with self.assertRaisesRegex(reader.CorruptJournal, "replaced"):
                next(records)

    def test_bad_ranges(self):
        for kwargs in ({"start_seq": -1}, {"end_seq": 1 << 64},
                       {"start_seq": 2, "end_seq": 1}):
            with self.subTest(kwargs=kwargs), self.assertRaises(ValueError):
                list(reader.read_journal(self.path, **kwargs))


@unittest.skipUnless(os.environ.get("TICKERTAPE_BIN") and os.environ.get("TICKERTAPE_PRODUCER"),
                     "set TICKERTAPE_BIN and TICKERTAPE_PRODUCER to built Rust binaries")
class RealJournalTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temp.cleanup)
        cls.path = Path(cls.temp.name) / "journal"
        cls.binary = str(Path(os.environ["TICKERTAPE_BIN"]).resolve())
        producer = str(Path(os.environ["TICKERTAPE_PRODUCER"]).resolve())
        subprocess.run([producer, "--dir", str(cls.path), "--count", "20000", "--rate", "1000000"],
                       check=True, capture_output=True)
        cls.export = subprocess.check_output([cls.binary, "export", str(cls.path), "--format", "jsonl"])

    def python_cli(self, *args):
        return subprocess.check_output([sys.executable, str(Path(reader.__file__)), str(self.path), *args])

    def test_full_journal_matches_rust_jsonl(self):
        records = list(reader.read_journal(self.path))
        self.assertEqual(len(records), 20000)
        for record, line in zip(records, self.export.splitlines()):
            self.assertEqual(reader._json_line(record).encode(), line)
        self.assertEqual(self.python_cli(), self.export)
        self.assertEqual(len(list(reader.read_journal(self.path, durable_only=True))), 20000)

    def test_inclusive_ranges(self):
        for first, last in ((10, 20), (0, 0), (19999, 20010), (20000, 20001)):
            with self.subTest(first=first, last=last):
                args = ["--from", str(first), "--to", str(last)]
                rust = subprocess.check_output([self.binary, "export", str(self.path),
                                                "--format", "jsonl", *args])
                self.assertEqual(self.python_cli(*args), rust)
                self.assertEqual([r.seq for r in reader.read_journal(self.path, first, last)],
                                 [json.loads(line)["seq"] for line in rust.splitlines()])

    def test_corrupt_committed_record_offset(self):
        copied = Path(self.temp.name) / "damaged"
        shutil.copytree(self.path, copied)
        path = sorted(copied.glob("*.tape"))[0]
        with path.open("r+b") as out:
            out.seek(64)
            length = struct.unpack("<I", out.read(4))[0]
            offset = 64 + ((length + 7) & ~7)
            out.seek(offset + 40)
            byte = out.read(1)
            out.seek(offset + 40)
            out.write(bytes([byte[0] ^ 1]))
        records = reader.read_journal(copied)
        self.assertEqual(next(records).seq, 0)
        with self.assertRaises(reader.CorruptJournal) as raised:
            next(records)
        self.assertEqual(raised.exception.path, path)
        self.assertEqual(raised.exception.offset, offset)


if __name__ == "__main__":
    unittest.main()
