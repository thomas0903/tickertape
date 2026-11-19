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

    /// The way out of every method of a handle that returns a result: runs `body`, then makes the owner check the
    /// last thing the method does before it hands the result or the exception back to the interpreter.
    ///
    /// Python code can run anywhere in a call (a signal handler, or a finalizer run by the garbage collector when
    /// something is allocated, the exception object included), and if it forks, the child resumes the call where
    /// the parent was. So an exception is first built here if it was created lazily (`PyErr::new` only records its
    /// type and arguments, and PyO3 would otherwise build it once the method has returned, after any check); then
    /// the owner is checked, and in a child the result, whatever it is, is replaced by `UsedAfterForkError`.
    /// Everything `body` allocated or released has happened by then (its locals are dropped when it returns), and
    /// the value returned must already be a Python object or need no garbage-collected allocation to become one.
    /// What the interpreter does after the method returns (adding a traceback entry, storing the result, running a
    /// signal handler) is after the call.
    ///
    /// `body` is a closure so that every `?` and `return` in a method passes through here; a source check in the
    /// tests below keeps every method of `Reader`, `ReaderIterator` and `Writer` that returns a `PyResult` in this
    /// form.
    pub fn settle<T>(
        &self,
        py: Python<'_>,
        what: &str,
        body: impl FnOnce() -> PyResult<T>,
    ) -> PyResult<T> {
        let result = body();
        if let Err(error) = &result {
            // Builds the exception object now (a no-op if it exists): nothing is allocated for it after the check.
            error.value(py);
        }
        if self.is_foreign() {
            // Released before the replacement is built, so that the replacement is built last.
            drop(result);
            return Err(errors::used_after_fork(what, self.pid(), std::process::id()).into_py(py));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    /// The methods (in `#[pymethods]` blocks) of `file` that return a `PyResult`, each with its body.
    fn methods_returning_results<'a>(file: &'a str, class: &str) -> Vec<(&'a str, &'a str)> {
        let header = format!("#[pymethods]\nimpl {class} {{\n");
        let start = file
            .find(&header)
            .unwrap_or_else(|| panic!("no #[pymethods] block for {class}"))
            + header.len();
        let block = &file[start..start + matching_brace(&file[start..])];
        let mut methods = Vec::new();
        let mut rest = block;
        while let Some(at) = rest.find("\n    fn ") {
            let item = &rest[at + 8..];
            let name = &item[..item.find(['(', '<']).expect("a method name")];
            let open = item.find('{').expect("a method body");
            let body = &item[open + 1..open + 1 + matching_brace(&item[open + 1..])];
            if item[..open].contains("PyResult") {
                methods.push((name, body.trim()));
            }
            rest = &item[open + 1 + body.len()..];
        }
        methods
    }

    /// The length of `text` up to the `}` that closes a block opened just before it. Braces in string literals and
    /// comments of the checked files are balanced, so they need no special case.
    fn matching_brace(text: &str) -> usize {
        matching(text, '{', '}')
    }

    /// Like `matching_brace`, for any pair of delimiters.
    fn matching(text: &str, open: char, close: char) -> usize {
        let mut depth = 0usize;
        for (i, c) in text.char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                if depth == 0 {
                    return i;
                }
                depth -= 1;
            }
        }
        panic!("unbalanced {open}{close}")
    }

    /// Every method of a handle that returns a `PyResult` (constructors excepted: they have no owner yet) is one
    /// call of `settle` whose body is a closure, optionally after `let` statements that cannot fail or return. So
    /// every value and every exception it returns passes through `Owner::settle`, and the owner check is the last
    /// thing the method does.
    #[test]
    fn every_method_returns_through_settle() {
        let reader = include_str!("reader.rs");
        let writer = include_str!("writer.rs");
        let mut checked = Vec::new();
        for (file, class) in [
            (reader, "Reader"),
            (reader, "ReaderIterator"),
            (writer, "Writer"),
        ] {
            for (name, body) in methods_returning_results(file, class) {
                if name == "new" {
                    continue;
                }
                // The statement that calls `settle`, and the statements before it.
                let call = body.find(".settle(py, ").unwrap_or(body.len());
                let line = body[..call].rfind('\n').map_or(0, |i| i + 1);
                let (prefix, receiver) = (&body[..line], body[line..call].trim());
                let args = &body[(call + 12).min(body.len())..];
                // The call must end the body: its closing parenthesis is the body's last character.
                let end =
                    (call < body.len()).then(|| call + 8 + matching(&body[call + 8..], '(', ')'));
                assert!(
                    !receiver.is_empty()
                        && receiver
                            .chars()
                            .all(|c| c.is_alphanumeric() || "_.()".contains(c))
                        && (args.starts_with("|| ") || args.starts_with("move || "))
                        && end == Some(body.len() - 1)
                        && !prefix.contains('?')
                        && !prefix.contains("return")
                        && prefix.lines().all(|l| l.trim().starts_with("let ")),
                    "{class}::{name} must be `<receiver>.settle(py, || ...)`, after `let` statements without `?` \
                     or `return`:\n{body}"
                );
                checked.push(format!("{class}::{name}"));
            }
        }
        for expected in [
            "Reader::seek",
            "Reader::read_many",
            "Reader::position",
            "Reader::__iter__",
            "ReaderIterator::__next__",
            "Writer::append",
            "Writer::append_many",
            "Writer::next_seq",
        ] {
            assert!(
                checked.iter().any(|c| c == expected),
                "the parser missed {expected}: {checked:?}"
            );
        }
    }
}
