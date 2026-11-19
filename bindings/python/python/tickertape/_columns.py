"""Conversions of a ``RecordBatch`` to NumPy, Arrow and pandas. Each library is imported only when used."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ._native import RecordBatch

_NUMERIC = (("seq", "<u8"), ("schema_id", "<u4"), ("venue_ts_ns", "<u8"), ("local_ts_ns", "<u8"))


def _require(module: str, method: str) -> Any:
    try:
        return __import__(module)
    except ImportError as error:
        raise ImportError(f"RecordBatch.{method}() needs the optional package {module!r}") from error


def to_numpy(batch: RecordBatch) -> dict[str, Any]:
    np = _require("numpy", "to_numpy")
    # frombuffer shares the immutable bytes (read-only arrays that keep them alive): no copy.
    columns = {name: np.frombuffer(getattr(batch, name), dtype=dtype) for name, dtype in _NUMERIC}
    offsets = np.frombuffer(batch.payload_offsets, dtype="<i8")
    data = batch.payload_data
    payload = np.empty(len(batch), dtype=object)
    for i in range(len(batch)):
        payload[i] = data[offsets[i]:offsets[i + 1]]
    columns["payload"] = payload
    return columns


def to_arrow(batch: RecordBatch) -> Any:
    pa = _require("pyarrow", "to_arrow")
    n = len(batch)
    types = {"<u8": pa.uint64(), "<u4": pa.uint32()}
    arrays = [pa.Array.from_buffers(types[dtype], n, [None, pa.py_buffer(getattr(batch, name))])
              for name, dtype in _NUMERIC]
    arrays.append(pa.Array.from_buffers(pa.large_binary(), n, [
        None, pa.py_buffer(batch.payload_offsets), pa.py_buffer(batch.payload_data)]))
    return pa.Table.from_arrays(arrays, names=[name for name, _ in _NUMERIC] + ["payload"])


def to_pandas(batch: RecordBatch) -> Any:
    pd = _require("pandas", "to_pandas")
    return pd.DataFrame(to_numpy(batch))
