//! The process that opened a handle. A `Writer` or `Reader` copied into a child by `fork()` shares the parent's
//! mappings, file descriptors and lock, but not its helper threads; using it there could overwrite records the
//! parent writes (or the reverse) and joining its threads fails. Every method checks the owner first.

use pyo3::prelude::*;

use crate::errors;

#[derive(Debug, Clone, Copy)]
pub struct Owner {
    pid: u32,
}

impl Owner {
    /// The current process.
    pub fn current() -> Self {
        Owner {
            pid: std::process::id(),
        }
    }

    /// The process that opened the handle.
    pub fn pid(self) -> u32 {
        self.pid
    }

    /// True in a process other than the one that opened the handle (a child created by `fork`).
    pub fn is_foreign(self) -> bool {
        std::process::id() != self.pid
    }

    /// Raises `UsedAfterForkError` in a foreign process. Call before taking any lock or touching the journal.
    pub fn check(self, py: Python<'_>, what: &str) -> PyResult<()> {
        let pid = std::process::id();
        if pid == self.pid {
            Ok(())
        } else {
            Err(errors::used_after_fork(what, self.pid, pid).into_py(py))
        }
    }
}
