//! The process that opened a handle. A `Writer` or `Reader` copied into a child by `fork()` shares the parent's
//! mappings, file descriptors and lock, but not its helper threads; using it there could overwrite records the
//! parent writes (or the reverse) and joining its threads fails. Every method checks the owner first.
//!
//! A process id alone does not identify the owner: a handle inherited down several generations of forks can meet a
//! process that was given the opening process's id. So the owner is the process id together with a fork generation,
//! a counter that `os.register_at_fork(after_in_child=...)` advances in every child forked from Python (registered
//! when the module is imported). A handle is foreign when either differs.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::errors;

/// Advanced in every child process forked from Python (see the module documentation).
static FORK_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Called by `os.register_at_fork` in a new child process.
#[pyfunction]
fn _after_fork_in_child() {
    FORK_GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Registers the fork hook. Called once, when the module is imported.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let hook = wrap_pyfunction!(_after_fork_in_child, m)?;
    let py = m.py();
    let kwargs = PyDict::new(py);
    kwargs.set_item("after_in_child", hook)?;
    py.import("os")?
        .getattr("register_at_fork")?
        .call((), Some(&kwargs))?;
    Ok(())
}

#[derive(Debug)]
pub struct Owner {
    /// Atomic only so that tests can simulate process-id reuse (`_set_owner_pid_for_testing`).
    pid: AtomicU32,
    generation: u64,
}

impl Owner {
    /// The current process.
    pub fn current() -> Self {
        Owner {
            pid: AtomicU32::new(std::process::id()),
            generation: FORK_GENERATION.load(Ordering::Acquire),
        }
    }

    /// The process that opened the handle.
    pub fn pid(&self) -> u32 {
        self.pid.load(Ordering::Relaxed)
    }

    /// Test only: pretends the handle was opened by process `pid`, keeping the fork generation.
    pub fn set_pid_for_testing(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
    }

    /// True in a process other than the one that opened the handle (a child created by `fork`).
    pub fn is_foreign(&self) -> bool {
        std::process::id() != self.pid()
            || FORK_GENERATION.load(Ordering::Acquire) != self.generation
    }

    /// Raises `UsedAfterForkError` in a foreign process. Call before taking any lock or touching the journal.
    pub fn check(&self, py: Python<'_>, what: &str) -> PyResult<()> {
        if self.is_foreign() {
            Err(errors::used_after_fork(what, self.pid(), std::process::id()).into_py(py))
        } else {
            Ok(())
        }
    }
}
