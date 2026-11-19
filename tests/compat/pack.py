#!/usr/bin/env python3
"""Pack compatibility fixture journals and maintain manifest.json (see tests/compat/README.md).

Pack format ("TTFXPACK", version 1, all integers little-endian):

    magic       8 bytes  b"TTFXPACK"
    version     u32      1
    file_count  u32
    per file, sorted by name:
        name_len  u16, name (UTF-8, relative, "/"-separated)
        file_len  u64    the file's exact length
        run_count u32
        per run, ascending and non-overlapping:
            offset u64, len u32, then len bytes

Every byte not inside a run is zero. Runs keep every non-zero byte and zero gaps shorter than 32 bytes, so the
pack stores the files' exact contents, only without their long zero stretches (mostly preallocated, unwritten
segment space). Unpacking writes each run and sets the length; manifest.json holds each file's SHA-256 and length,
computed by the generator before packing, and the tests check them after unpacking.

Commands:
    pack.py pack <journal_dir> <out.ttfx>
    pack.py unpack <file.ttfx> <out_dir>
    pack.py add <manifest.json> <entry.json> --name N --pack FILE --tag T --commit C --os OS --arch A
                --platform TEXT --rustc TEXT --command TEXT
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys

MAGIC = b"TTFXPACK"
VERSION = 1
# A run: a non-zero byte, then non-zero bytes or zero gaps of 1 to 31 bytes that are followed by a non-zero byte.
RUN = re.compile(rb"[^\x00](?:[^\x00]|\x00{1,31}(?=[^\x00]))*")


def journal_files(root):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort()
        for name in filenames:
            path = Path(dirpath) / name
            if not path.is_file() or path.is_symlink():
                raise SystemExit(f"unexpected file type: {path}")
            out.append((path.relative_to(root).as_posix(), path))
    return sorted(out)


def encode(root):
    parts = [MAGIC, struct.pack("<II", VERSION, 0)]
    count = 0
    for name, path in journal_files(root):
        data = path.read_bytes()
        runs = [(m.start(), m.group()) for m in RUN.finditer(data)]
        encoded = name.encode("utf-8")
        parts.append(struct.pack("<H", len(encoded)) + encoded)
        parts.append(struct.pack("<QI", len(data), len(runs)))
        for offset, chunk in runs:
            parts.append(struct.pack("<QI", offset, len(chunk)) + chunk)
        count += 1
    parts[1] = struct.pack("<II", VERSION, count)
    return b"".join(parts)


def decode(blob):
    """Return {name: bytes}; reject anything malformed."""
    if blob[:8] != MAGIC:
        raise ValueError("bad magic")
    version, count = struct.unpack_from("<II", blob, 8)
    if version != VERSION:
        raise ValueError(f"unsupported pack version {version}")
    pos = 16
    files = {}
    for _ in range(count):
        (name_len,) = struct.unpack_from("<H", blob, pos)
        name = blob[pos + 2:pos + 2 + name_len].decode("utf-8")
        pos += 2 + name_len
        if name.startswith("/") or ".." in name.split("/") or name in files:
            raise ValueError(f"bad file name {name!r}")
        size, run_count = struct.unpack_from("<QI", blob, pos)
        pos += 12
        data = bytearray(size)
        end = 0
        for _ in range(run_count):
            offset, length = struct.unpack_from("<QI", blob, pos)
            pos += 12
            if offset < end or offset + length > size:
                raise ValueError(f"bad run in {name}")
            data[offset:offset + length] = blob[pos:pos + length]
            pos += length
            end = offset + length
        files[name] = bytes(data)
    if pos != len(blob):
        raise ValueError("trailing bytes")
    return files


def cmd_pack(args):
    root = Path(args.journal)
    blob = encode(root)
    # Round trip before writing: the pack must reproduce every file exactly.
    restored = decode(blob)
    originals = {name: path.read_bytes() for name, path in journal_files(root)}
    if restored != originals:
        raise SystemExit("pack round trip failed")
    Path(args.out).write_bytes(blob)
    raw = sum(len(v) for v in originals.values())
    print(f"{args.out}: {len(originals)} files, {raw} bytes -> {len(blob)} bytes", file=sys.stderr)


def cmd_unpack(args):
    out = Path(args.out)
    for name, data in decode(Path(args.pack).read_bytes()).items():
        path = out / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)


def cmd_add(args):
    manifest_path = Path(args.manifest)
    entry = json.loads(Path(args.entry).read_text())
    pack = Path(args.pack)
    restored = decode(pack.read_bytes())
    for f in entry["files"]:
        data = restored[f["name"]]
        if len(data) != f["len"] or hashlib.sha256(data).hexdigest() != f["sha256"]:
            raise SystemExit(f"{pack}: {f['name']} does not match the generator's digest")
    if sorted(restored) != sorted(f["name"] for f in entry["files"]):
        raise SystemExit(f"{pack}: file list differs from the generator's")
    entry = {
        "name": args.name,
        "pack": pack.name,
        "tag": args.tag,
        "commit": args.commit,
        "os": args.os,
        "arch": args.arch,
        "platform": args.platform,
        "rustc": args.rustc,
        "generator_command": args.command,
        **entry,
    }
    if manifest_path.exists():
        manifest = json.loads(manifest_path.read_text())
    else:
        manifest = {
            "description": "Format-v1 journals written by released tickertape versions. See tests/compat/README.md.",
            "format_version": 1,
            "pack_format": "TTFXPACK v1, see tests/compat/pack.py",
            "stream_digest": "SHA-256 over every record in sequence order: seq u64 LE, schema_id u32 LE, "
                             "venue_ts_ns u64 LE, local_ts_ns u64 LE, payload length u64 LE, payload",
            "fixtures": [],
        }
    manifest["fixtures"] = sorted(
        [f for f in manifest["fixtures"] if f["name"] != args.name] + [entry], key=lambda f: f["name"])
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command_name", required=True)
    p = sub.add_parser("pack")
    p.add_argument("journal")
    p.add_argument("out")
    p.set_defaults(func=cmd_pack)
    p = sub.add_parser("unpack")
    p.add_argument("pack")
    p.add_argument("out")
    p.set_defaults(func=cmd_unpack)
    p = sub.add_parser("add")
    p.add_argument("manifest")
    p.add_argument("entry")
    for flag in ("name", "pack", "tag", "commit", "os", "arch", "platform", "rustc", "command"):
        p.add_argument(f"--{flag}", required=True)
    p.set_defaults(func=cmd_add)
    args = parser.parse_args(argv)
    args.func(args)


if __name__ == "__main__":
    main()
