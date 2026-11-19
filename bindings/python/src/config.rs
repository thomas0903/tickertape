//! Configuration classes: `Backoff` and `Retention`, plus argument parsing shared by `Writer` and `Reader`.

use std::path::PathBuf;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// Converts seconds (a float, an int or a `datetime.timedelta`) to a `Duration`.
pub fn seconds(value: &Bound<'_, PyAny>, what: &str) -> PyResult<Duration> {
    let secs: f64 = if value.hasattr("total_seconds")? {
        value.call_method0("total_seconds")?.extract()?
    } else {
        value.extract()?
    };
    Duration::try_from_secs_f64(secs).map_err(|_| {
        PyValueError::new_err(format!(
            "{what} must be a finite, non-negative number of seconds"
        ))
    })
}

/// A timeout argument: `None` (or `math.inf`) waits forever; otherwise non-negative seconds (a number or a
/// `datetime.timedelta`).
#[derive(Debug, Clone, Copy)]
pub struct Timeout(pub Option<Duration>);

impl<'a, 'py> FromPyObject<'a, 'py> for Timeout {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if obj.is_none() {
            return Ok(Timeout(None));
        }
        if let Ok(f) = obj.extract::<f64>() {
            if f == f64::INFINITY {
                return Ok(Timeout(None));
            }
        }
        seconds(&obj.to_owned(), "timeout").map(|d| Timeout(Some(d)))
    }
}

/// How a `Reader` waits between polls before blocking on the writer's wake-up region (ADR 0017).
///
/// Each poll that finds nothing moves one step: `spins` busy polls, then `yields` polls that yield the CPU, then
/// sleeps from `min_sleep` doubling to `max_sleep` seconds. With wake-ups enabled, the sleep phase is replaced by
/// blocking until the writer publishes. All waiting happens with the GIL released.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug, Clone)]
pub struct Backoff {
    pub inner: tickertape::Backoff,
}

#[pymethods]
impl Backoff {
    #[new]
    #[pyo3(signature = (*, spins=100, yields=10, min_sleep=None, max_sleep=None))]
    fn new(
        spins: u32,
        yields: u32,
        min_sleep: Option<&Bound<'_, PyAny>>,
        max_sleep: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let mut inner = tickertape::Backoff::default();
        inner.spins = spins;
        inner.yields = yields;
        if let Some(v) = min_sleep {
            inner.min_sleep = seconds(v, "min_sleep")?;
        }
        if let Some(v) = max_sleep {
            inner.max_sleep = seconds(v, "max_sleep")?;
        }
        if inner.min_sleep > inner.max_sleep {
            return Err(PyValueError::new_err("min_sleep must not exceed max_sleep"));
        }
        Ok(Backoff { inner })
    }

    /// Never sleep or yield: lowest latency, one core at 100 % while waiting.
    #[staticmethod]
    fn busy_spin() -> Self {
        Backoff {
            inner: tickertape::Backoff::busy_spin(),
        }
    }

    #[getter]
    fn spins(&self) -> u32 {
        self.inner.spins
    }

    #[getter]
    fn yields(&self) -> u32 {
        self.inner.yields
    }

    /// First sleep, in seconds.
    #[getter]
    fn min_sleep(&self) -> f64 {
        self.inner.min_sleep.as_secs_f64()
    }

    /// Longest sleep, in seconds.
    #[getter]
    fn max_sleep(&self) -> f64 {
        self.inner.max_sleep.as_secs_f64()
    }

    fn __repr__(&self) -> String {
        format!(
            "Backoff(spins={}, yields={}, min_sleep={}, max_sleep={})",
            self.inner.spins,
            self.inner.yields,
            self.min_sleep(),
            self.max_sleep()
        )
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        other
            .cast::<Backoff>()
            .is_ok_and(|o| o.get().inner == self.inner)
    }
}

/// A retention policy and what to do with removed segments (ADR 0016). Build with `Retention.delete(...)` or
/// `Retention.archive(directory, ...)`.
///
/// The writer runs it after open and after each successful checkpoint. It only ever removes a sealed prefix of
/// segments strictly before the checkpoint segment. Failures never fail open or checkpoint; they are reported by
/// `Writer.last_retention_error`.
#[pyclass(frozen, module = "tickertape")]
#[derive(Debug, Clone)]
pub struct Retention {
    pub policy: tickertape::retention::Policy,
    pub action: tickertape::retention::Action,
}

fn policy(
    max_segments: Option<usize>,
    max_bytes: Option<u64>,
    max_age: Option<&Bound<'_, PyAny>>,
) -> PyResult<tickertape::retention::Policy> {
    let mut policy = tickertape::retention::Policy::default();
    if let Some(n) = max_segments {
        policy = policy.max_segments(n);
    }
    if let Some(b) = max_bytes {
        policy = policy.max_bytes(b);
    }
    if let Some(age) = max_age {
        policy = policy.max_age(seconds(age, "max_age")?);
    }
    Ok(policy)
}

#[pymethods]
impl Retention {
    /// Permanently delete eligible segments.
    #[staticmethod]
    #[pyo3(signature = (*, max_segments=None, max_bytes=None, max_age=None))]
    fn delete(
        max_segments: Option<usize>,
        max_bytes: Option<u64>,
        max_age: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Ok(Retention {
            policy: policy(max_segments, max_bytes, max_age)?,
            action: tickertape::retention::Action::Delete,
        })
    }

    /// Move eligible segments into `directory`, which must already exist. Existing files there are never replaced.
    #[staticmethod]
    #[pyo3(signature = (directory, *, max_segments=None, max_bytes=None, max_age=None))]
    fn archive(
        directory: PathBuf,
        max_segments: Option<usize>,
        max_bytes: Option<u64>,
        max_age: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Ok(Retention {
            policy: policy(max_segments, max_bytes, max_age)?,
            action: tickertape::retention::Action::MoveTo(directory),
        })
    }

    #[getter]
    fn max_segments(&self) -> Option<usize> {
        self.policy.max_segments
    }

    #[getter]
    fn max_bytes(&self) -> Option<u64> {
        self.policy.max_bytes
    }

    /// In seconds.
    #[getter]
    fn max_age(&self) -> Option<f64> {
        self.policy.max_age.map(|d| d.as_secs_f64())
    }

    /// The archive directory, or `None` for deletion.
    #[getter]
    fn archive_dir(&self) -> Option<PathBuf> {
        match &self.action {
            tickertape::retention::Action::MoveTo(dir) => Some(dir.clone()),
            _ => None,
        }
    }

    fn __repr__(&self) -> String {
        fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "None".to_string(), |v| v.to_string())
        }
        let action = match &self.action {
            tickertape::retention::Action::MoveTo(dir) => {
                format!("archive({:?}, ", dir.display().to_string())
            }
            _ => "delete(".to_string(),
        };
        format!(
            "Retention.{action}max_segments={}, max_bytes={}, max_age={})",
            opt(self.policy.max_segments),
            opt(self.policy.max_bytes),
            opt(self.max_age())
        )
    }
}

/// Parses `recovery="quarantine" | "fail"`.
pub fn recovery(value: &str) -> PyResult<tickertape::RecoveryPolicy> {
    match value {
        "quarantine" => Ok(tickertape::RecoveryPolicy::Quarantine),
        "fail" => Ok(tickertape::RecoveryPolicy::Fail),
        other => Err(PyValueError::new_err(format!(
            "recovery must be 'quarantine' or 'fail', not {other:?}"
        ))),
    }
}
