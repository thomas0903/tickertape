#!/usr/bin/env python3
"""Folds `perf script` output into one line per distinct stack (`outermost;...;innermost count`), most frequent first.

Part of profile-sustained.sh. The input has been written with
`perf script -F comm,ip,sym,dso,dsoff,event --no-inline`, and perf does not name the functions the compiler inlined, so the
frames in binaries with line tables are resolved with one `addr2line -a -i -f -C` call per binary, which does: a frame
then becomes its chain of inlined functions. Frames in the kernel are marked `_[k]`. The output is what flamegraph.pl
and speedscope read.

    fold-stacks.py profile.script > profile.folded
    fold-stacks.py --self profile.folded          # share of samples by innermost function
    fold-stacks.py --has futex_wake,memcpy profile.folded   # share of samples with that function anywhere in the stack
"""

import argparse
import collections
import re
import subprocess
import sys

FRAME = re.compile(r"^\s+([0-9a-f]+)\s+(.*?)\s+\(([^()]*(?:\([^()]*\))?)\)$")
HASH = re.compile(r"::h[0-9a-f]{16}$")
DSO_OFFSET = re.compile(r"^(.*)\+0x([0-9a-f]+)$")
LOAD = re.compile(r"^\s+LOAD\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x[0-9a-f]+\s+0x([0-9a-f]+)")


def parse(lines):
    """Each sample's frames, innermost first: (offset in the binary, symbol, binary)."""
    samples, frames = [], None
    for line in lines:
        line = line.rstrip("\n")
        if not line.strip():
            if frames:
                samples.append(frames)
            frames = None
            continue
        if not line[0].isspace():
            frames = []
            continue
        match = FRAME.match(line)
        if match and frames is not None:
            _, symbol, binary = match.groups()
            placed = DSO_OFFSET.match(binary)
            offset = int(placed.group(2), 16) if placed else 0
            frames.append((offset, HASH.sub("", symbol), placed.group(1) if placed else binary))
        elif frames is not None:
            frames.append((0, line.strip(), "?"))
    if frames:
        samples.append(frames)
    return samples


def inline_chains(wanted):
    """The chain of functions (innermost first) at each wanted (binary, offset), as far as addr2line knows it."""
    chains = {}
    for binary, offsets in wanted.items():
        try:
            headers = subprocess.run(["readelf", "-lW", binary], capture_output=True, text=True, check=True).stdout
            loads = [tuple(int(v, 16) for v in m.groups()) for m in map(LOAD.match, headers.splitlines()) if m]
            # addr2line wants the linked address, perf gives the offset in the file.
            linked = {}
            for offset in offsets:
                for start, address, size in loads:
                    if start <= offset < start + size:
                        linked[offset - start + address] = offset
            out = subprocess.run(
                ["addr2line", "-a", "-i", "-f", "-C", "-e", binary],
                input="\n".join(f"{a:#x}" for a in linked), capture_output=True, text=True, check=True).stdout
        except (OSError, subprocess.CalledProcessError) as error:
            print(f"fold-stacks: {binary}: {error}", file=sys.stderr)
            continue
        rows, chain, i = out.splitlines(), [], 0
        while i < len(rows):
            if rows[i].startswith("0x"):
                chain = chains.setdefault((binary, linked[int(rows[i], 16)]), [])
                i += 1
            else:
                chain.append(HASH.sub("", rows[i]))
                i += 2  # the function's name, then its file and line
    return chains


def short(name):
    """A function's name without generic arguments, spaces and semicolons (which separate frames)."""
    depth, out = 0, []
    for char in name:
        if char == "<":
            depth += 1
        elif char == ">" and depth:
            depth -= 1
        elif not depth:
            out.append(char)
    return re.sub(r"::::", "::", "".join(out)).strip(":").replace(" ", "").replace(";", ":") or name.replace(" ", "")


def fold(path):
    with open(path, encoding="utf-8", errors="replace") as lines:
        samples = parse(lines)
    wanted = collections.defaultdict(set)
    for frames in samples:
        for depth, (offset, _, binary) in enumerate(frames):
            if binary.endswith("tt-sustained"):
                # A return address belongs to the instruction before it.
                wanted[binary].add(offset - (1 if depth else 0))
    chains = inline_chains(wanted)
    counts = collections.Counter()
    for frames in samples:
        names = []
        for depth, (offset, symbol, binary) in enumerate(frames):
            chain = chains.get((binary, offset - (1 if depth else 0)))
            if chain and "??" not in chain:
                names.extend(short(n) for n in chain)
            elif "kernel" in binary or binary.endswith(".ko"):
                names.append(short(symbol) + "_[k]")
            else:
                names.append(short(symbol))
        counts[";".join(reversed(names))] += 1
    for stack, count in counts.most_common():
        print(stack, count)


def read_folded(path):
    with open(path, encoding="utf-8") as lines:
        for line in lines:
            stack, _, count = line.rstrip("\n").rpartition(" ")
            yield stack.split(";"), int(count)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("path")
    parser.add_argument("--self", action="store_true", dest="by_self")
    parser.add_argument("--has", help="comma-separated function names (substrings)")
    parser.add_argument("--top", type=int, default=25)
    args = parser.parse_args()
    if not args.by_self and not args.has:
        fold(args.path)
        return
    total, leaf, has = 0, collections.Counter(), collections.Counter()
    wanted = args.has.split(",") if args.has else []
    for frames, count in read_folded(args.path):
        total += count
        leaf[frames[-1]] += count
        for name in wanted:
            if any(name in frame for frame in frames):
                has[name] += count
    print(f"{total} samples")
    if args.by_self:
        for name, count in leaf.most_common(args.top):
            print(f"{100 * count / total:6.2f} %  {name}")
    for name in wanted:
        print(f"{100 * has[name] / total:6.2f} %  {name} (anywhere in the stack)")


if __name__ == "__main__":
    sys.exit(main())
