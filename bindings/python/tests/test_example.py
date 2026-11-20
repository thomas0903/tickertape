"""The documented end-to-end example runs to completion."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

EXAMPLE = Path(__file__).resolve().parents[1] / "examples" / "end_to_end.py"


def test_end_to_end_example(tmp_path: Path) -> None:
    out = subprocess.run([sys.executable, str(EXAMPLE), str(tmp_path / "journal")], check=True,
                         capture_output=True, text=True, timeout=120).stdout
    assert "live follower saw all 2000 trades" in out
    assert "verify: 10001 records, ok=True" in out
    assert "replayed 10001 durable records in one batch" in out
