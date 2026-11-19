"""End-to-end example: write a journal, follow it live from another process, replay it, and analyse it.

Run it with ``python examples/end_to_end.py [JOURNAL_DIR]``. Only ``tickertape`` is required; the analysis step uses
pandas and DuckDB when they are installed (``pip install pandas duckdb pyarrow``) and is skipped otherwise.
"""

from __future__ import annotations

import json
import struct
import subprocess
import sys
import tempfile
import textwrap
import time
from pathlib import Path

import tickertape as tt

TRADE, QUOTE = 1, 2
TRADE_FORMAT = "<qq"  # price in hundredths, quantity: an application-defined payload layout
# The segment size is fixed when a journal is created (later opens reuse it), so pass it at the first open: the
# default of 128 MiB would preallocate far more than this example needs.
SEGMENT_SIZE = 1 << 20

# A live follower, run as a separate process: it waits for records as the writer appends them.
FOLLOWER = textwrap.dedent("""
    import sys
    import tickertape as tt

    trades = 0
    with tt.Reader(sys.argv[1]) as reader:
        print("following", flush=True)
        while True:
            record = reader.next(timeout=5.0)  # waits with the GIL released; Ctrl-C interrupts it
            if record is None or record.schema_id == 0:  # schema 0 marks the end in this example
                break
            trades += record.schema_id == 1
    print(trades, flush=True)
""")


def write(journal: Path, follower: subprocess.Popen[str]) -> int:
    trades = 0
    with tt.Writer(journal, segment_size=SEGMENT_SIZE) as writer:
        assert follower.stdout is not None and follower.stdout.readline().strip() == "following"
        for second in range(5):
            payloads, schemas, venue = [], [], []
            for i in range(2_000):
                is_trade = i % 5 == 0
                trades += is_trade
                payloads.append(struct.pack(TRADE_FORMAT, 10_000 + i % 50, 1 + i % 7))
                schemas.append(TRADE if is_trade else QUOTE)
                venue.append(1_700_000_000_000_000_000 + (second * 2_000 + i) * 1_000)
            # One call per batch. Each record is visible to readers when append_many returns...
            seqs = writer.append_many(payloads, schema_id=schemas, venue_ts_ns=venue, local_ts_ns=time.time_ns())
            # ...and durable once a later checkpoint returns.
            writer.checkpoint()
            print(f"appended {seqs.start}..{seqs.stop - 1}, checkpointed")
        writer.append(b"", schema_id=0)  # end marker for the follower
        writer.checkpoint()
    return trades


def replay(journal: Path) -> tt.RecordBatch:
    with tt.Reader(journal, durable_only=True) as reader:
        reader.seek(5_000)  # replay from any sequence number
        first = reader.try_next()
        assert first is not None
        price, qty = struct.unpack(TRADE_FORMAT, first.payload)
        print(f"record {first.seq}: schema {first.schema_id}, price {price / 100}, qty {qty}")
        reader.seek(0)
        return reader.read_batch(1_000_000)  # one call; columns ready for NumPy, Arrow or pandas


def analyse(batch: tt.RecordBatch) -> None:
    try:
        import duckdb
        import pandas  # noqa: F401  (needed by to_pandas)
    except ImportError:
        print("pandas or duckdb not installed: skipping the analysis step")
        return
    frame = batch.to_pandas()
    frame["price"] = [struct.unpack_from(TRADE_FORMAT, p)[0] / 100 if p else None for p in frame["payload"]]
    summary = duckdb.sql(
        "SELECT schema_id, count(*) AS records, round(avg(price), 2) AS avg_price "
        "FROM frame WHERE schema_id <> 0 GROUP BY schema_id ORDER BY schema_id"
    ).fetchall()
    print("schema_id, records, avg_price:", json.dumps(summary))


def main() -> None:
    journal = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(tempfile.mkdtemp()) / "journal"
    tt.Writer(journal, segment_size=SEGMENT_SIZE).close()  # create the journal so the follower can open it
    follower = subprocess.Popen([sys.executable, "-c", FOLLOWER, str(journal)], stdout=subprocess.PIPE, text=True)
    trades = write(journal, follower)
    out, _ = follower.communicate(timeout=60)
    assert int(out.strip().splitlines()[-1]) == trades, "the live follower saw every trade"
    print(f"live follower saw all {trades} trades")
    report = tt.verify(journal)
    print(f"verify: {report.records} records, ok={report.ok}, checkpoint={tt.read_checkpoint(journal)}")
    batch = replay(journal)
    print(f"replayed {len(batch)} durable records in one batch")
    analyse(batch)


if __name__ == "__main__":
    main()
