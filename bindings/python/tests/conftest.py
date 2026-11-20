"""Shared fixtures for the binding tests.

Optional environment variables (the CI job sets them; tests that need one are skipped without it):

- ``TICKERTAPE_BIN``: the ``tickertape`` CLI (``cargo build --release -p tickertape-cli``).
- ``TICKERTAPE_PRODUCER``: ``demo-producer`` (``cargo build --release -p tickertape-examples``).
- ``TICKERTAPE_DURABLE_HELPER``: ``tt-durable-only`` (``cargo build --release -p tickertape-tests --bins``), a Rust
  writer/reader of the deterministic record pattern in ``helpers.py``.

Journals are created under pytest's temporary directory, so point ``TMPDIR`` at a local filesystem (not tmpfs).
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[3]
# The dependency-free decoder lives in the repository, not in the wheel.
sys.path.insert(0, str(REPO / "python"))


def _binary(variable: str) -> Path:
    value = os.environ.get(variable)
    if not value:
        pytest.skip(f"set {variable} to run this test")
    path = Path(value).resolve()
    if not path.is_file():
        pytest.fail(f"{variable}={value} does not exist")
    return path


@pytest.fixture
def journal(tmp_path: Path) -> Path:
    return tmp_path / "journal"


@pytest.fixture
def cli() -> Path:
    return _binary("TICKERTAPE_BIN")


@pytest.fixture
def producer() -> Path:
    return _binary("TICKERTAPE_PRODUCER")


@pytest.fixture
def durable_helper() -> Path:
    return _binary("TICKERTAPE_DURABLE_HELPER")
