# 0013: Python read-only format v1 decoder

- **Status:** accepted
- **Date:** 2026-11-01

## Context

Python users need direct access to format v1 journals without building a Rust extension or exporting Parquet.
The format specification is sufficient to implement a separate, read-only decoder.

## Options considered

1. **Rust bindings:** faster and share validation, but need compilation and a packaging/dependency surface.
2. **Standard-library Python module:** portable and independently checks the specification, at lower throughput.

## Decision

Add a single Python 3.9+ module using buffered binary reads, with a table-based CRC32C fallback and optional
`crc32c` acceleration. Capture segment names and the checkpoint boundary once per iteration. Validate the chain
from the oldest segment present (like the Rust reader), including records skipped by the inclusive lower bound; stop at the upper bound without
examining later records. Missing seals/successors and disconnected segments are errors.
An empty slot ends the scan only in the newest listed segment. This deliberately refuses incomplete rolls;
readers do not perform recovery.

On a validation failure, reopen the segment, validate its header again and retry the failing position once.
Never replay records already yielded. This handles transient buffered/torn reads without hiding persistent
corruption. Detect a segment replacement before following its seal or retrying; report corruption rather than
mixing old and repaired histories. The result is an observation during the scan, not an atomic live snapshot;
newly listed segments require another invocation. Checkpoint reads accept a journal directory or marker path;
short/invalid slots are ignored independently, and equal generations select the slot at offset 65,536 (as the
Rust decoder does; `docs/format.md` now states this).

## Consequences

The module never opens journal files for writing or takes the writer lock. Live appends can end a scan at an
uncommitted slot; concurrent repair requires restarting the scan. The README explains these limits. Unittest
fixtures cover malformed input and retry behavior; integration tests compare exact JSONL with the Rust CLI.
