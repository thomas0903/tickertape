#!/usr/bin/env python3
"""Checks that the C header keeps every frozen declaration of the ABI baseline (ADR 0028 -> Versioning).

The baseline (crates/tickertape-ffi/abi/tickertape-baseline.h) is the header of the latest stable release: the release
checklist replaces it with the header as shipped at 1.0 and at every later stable release, so that a declaration added
in 1.1 is frozen from 1.1 on (a baseline kept at 1.0 would let 1.2 remove it). Before 1.0 it declares nothing. Every
declaration in it must still be in the current header, unchanged:

- functions, typedefs and opaque types: the same declaration;
- constants (#define): the same value, except the library version macros, which change with every release;
- enumerators: the same value (new enumerators may be added);
- structs: the same fields in the same order, except that an extensible struct (one whose first field is
  `uint64_t struct_size`) may gain fields at the end.

Additions pass. Both headers are run through the C preprocessor (`$CC`, default `cc`) with empty stand-ins for the
system headers, which removes comments and resolves cbindgen's conditionals the same way for both.

Usage: check_abi_baseline.py BASELINE HEADER
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

# Macros whose values change with every release; they must stay defined.
VERSION_MACROS = {
    "TICKERTAPE_VERSION_MAJOR",
    "TICKERTAPE_VERSION_MINOR",
    "TICKERTAPE_VERSION_PATCH",
    "TICKERTAPE_VERSION_NUMBER",
}
INCLUDE_GUARD = "TICKERTAPE_H"
EXTENSIBLE_FIRST_FIELD = "uint64_t struct_size"


def preprocess(path: Path) -> str:
    """The header after the C preprocessor, with its #defines kept."""
    cc = os.environ.get("CC", "cc")
    with tempfile.TemporaryDirectory() as stubs:
        for name in ("stddef.h", "stdint.h"):
            Path(stubs, name).touch()
        result = subprocess.run(
            [cc, "-E", "-P", "-dD", "-std=c11", "-nostdinc", "-I", stubs, "-x", "c", str(path)],
            capture_output=True,
            text=True,
        )
    if result.returncode != 0:
        raise SystemExit(f"preprocessing {path} failed:\n{result.stderr}")
    return result.stdout


def normalize(text: str) -> str:
    text = re.sub(r"\s+", " ", text).strip()
    return re.sub(r"\s*([*(),\[\]{}=])\s*", r"\1", text)


def split_top_level(text: str, separator: str) -> list[str]:
    """Splits at `separator` outside braces."""
    parts, depth, current = [], 0, []
    for char in text:
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        if char == separator and depth == 0:
            parts.append("".join(current))
            current = []
        else:
            current.append(char)
    parts.append("".join(current))
    return [p.strip() for p in parts if p.strip()]


def declarations(path: Path) -> dict[tuple[str, str], object]:
    """The header's declarations, keyed by (kind, name)."""
    found: dict[tuple[str, str], object] = {}
    code = []
    for line in preprocess(path).splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            match = re.match(r"#\s*define\s+(TICKERTAPE_\w+)\b(.*)", stripped)
            if match and match.group(1) != INCLUDE_GUARD:
                found[("constant", match.group(1))] = normalize(match.group(2))
            continue
        code.append(line)
    for statement in split_top_level(" ".join(code), ";"):
        statement = normalize(statement)
        if match := re.fullmatch(r"enum (\w+)\{(.*)\}", statement):
            for item in split_top_level(match.group(2), ","):
                name, _, value = item.partition("=")
                found[("enumerator", name)] = value
        elif match := re.fullmatch(r"typedef struct (\w+)\{(.*)\}(\w+)", statement):
            found[("struct", match.group(3))] = split_top_level(match.group(2), ";")
        elif statement.startswith("typedef "):
            names = re.findall(r"\w+", statement.split("(")[1] if "(*" in statement else statement)
            found[("typedef", names[-1] if "(*" not in statement else names[0])] = statement
        elif match := re.match(r".*?(\w+)\(", statement):
            found[("function", match.group(1))] = statement
        else:
            raise SystemExit(f"{path}: cannot classify declaration: {statement}")
    return found


def problems(baseline: dict, header: dict) -> list[str]:
    """Every way `header` breaks a declaration of `baseline`."""
    out = []
    for (kind, name), frozen in sorted(baseline.items()):
        current = header.get((kind, name))
        if current is None:
            out.append(f"{kind} {name} was removed")
        elif kind == "constant" and name in VERSION_MACROS:
            continue
        elif kind == "struct" and frozen and frozen[0] == EXTENSIBLE_FIRST_FIELD:
            if current[: len(frozen)] != frozen:
                out.append(f"extensible struct {name} changed its frozen fields: {frozen} -> {current}")
        elif current != frozen:
            out.append(f"{kind} {name} changed: {frozen!r} -> {current!r}")
    return out


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[-1], file=sys.stderr)
        return 2
    baseline = declarations(Path(argv[1]))
    header = declarations(Path(argv[2]))
    found = problems(baseline, header)
    for problem in found:
        print(f"ABI break: {problem}", file=sys.stderr)
    if found:
        print(
            "The header no longer provides a declaration of the frozen ABI baseline (ADR 0028 -> Versioning).",
            file=sys.stderr,
        )
        return 1
    print(f"ABI baseline: {len(baseline)} frozen declarations kept; the header has {len(header)}.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
