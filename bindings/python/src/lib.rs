//! Python bindings for tickertape (ADR 0029). Built with maturin into the `tickertape._native` extension module;
//! the public API is re-exported by the `tickertape` package (`python/tickertape/__init__.py`).
//!
//! This crate contains no `unsafe` code (`unsafe_code` is denied below). Records cross into Python as owned
//! `bytes`; the core crate's zero-copy mapped reader is deliberately not exposed.

#![deny(unsafe_code)]

mod config;
mod errors;
mod inspect;
mod owner;
mod reader;
mod reports;
mod status;
mod writer;

use pyo3::prelude::*;

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("CORE_VERSION", tickertape::VERSION)?;
    m.add("FORMAT_VERSION", tickertape::format::FORMAT_VERSION)?;
    m.add(
        "DEFAULT_SEGMENT_SIZE",
        tickertape::WriterConfig::DEFAULT_SEGMENT_SIZE,
    )?;
    m.add("ERROR_VARIANTS", errors::ERROR_VARIANTS.to_vec())?;
    m.add("CORRUPT_REASONS", errors::CORRUPT_REASONS.to_vec())?;
    m.add("RECOVERY_ACTIONS", reports::RECOVERY_ACTIONS.to_vec())?;
    m.add("SEGMENT_ENDS", reports::SEGMENT_ENDS.to_vec())?;
    m.add(
        "STATUS_ENUMS",
        status::STATUS_ENUMS
            .iter()
            .map(|(name, variants)| (*name, variants.to_vec()))
            .collect::<Vec<_>>(),
    )?;
    m.add_class::<writer::Writer>()?;
    m.add_class::<reader::Reader>()?;
    m.add_class::<reader::Record>()?;
    m.add_class::<reader::RecordBatch>()?;
    m.add_class::<config::Backoff>()?;
    m.add_class::<config::Retention>()?;
    m.add_function(wrap_pyfunction!(inspect::list_segments, m)?)?;
    m.add_function(wrap_pyfunction!(inspect::read_checkpoint, m)?)?;
    m.add_function(wrap_pyfunction!(inspect::verify, m)?)?;
    m.add_class::<status::StatusSample>()?;
    m.add_function(wrap_pyfunction!(status::sample_status, m)?)?;
    owner::register(m)?;
    Ok(())
}
