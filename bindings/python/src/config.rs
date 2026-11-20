//! Configuration classes: `Backoff` and `Retention`, plus argument parsing shared by `Writer` and `Reader`.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyInt;

/// A Python hash from a Rust hashable value (`-1` is reserved by CPython; PyO3 maps it).
fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

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

/// A timeout argument: `None`, `math.inf` or any value too large to represent waits forever; otherwise non-negative
/// seconds (an int, a float or a `datetime.timedelta`). Negative values and NaN raise `ValueError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeout(pub Option<Duration>);

impl Timeout {
    pub const FOREVER: Timeout = Timeout(None);
    pub const ZERO: Timeout = Timeout(Some(Duration::ZERO));

    fn from_secs_f64(secs: f64) -> PyResult<Timeout> {
        if secs.is_nan() || secs < 0.0 {
            return Err(PyValueError::new_err(
                "timeout must be None or a non-negative number of seconds",
            ));
        }
        // Infinity, and anything beyond Duration's range (about 5.8e11 years), means forever.
        Ok(Timeout(Duration::try_from_secs_f64(secs).ok()))
    }

    /// The instant at which the wait ends, or `None` for forever (also when `now + timeout` is not representable).
    pub fn deadline(self, now: Instant) -> Option<Instant> {
        self.0.and_then(|t| now.checked_add(t))
    }
}

impl<'a, 'py> FromPyObject<'a, 'py> for Timeout {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        if obj.is_none() {
            return Ok(Timeout::FOREVER);
        }
        if obj.is_instance_of::<PyInt>() {
            if let Ok(secs) = obj.extract::<u64>() {
                return Ok(Timeout(Some(Duration::from_secs(secs))));
            }
            // Too large for u64 (forever) or negative (an error).
            let negative = obj.lt(0)?;
            return Timeout::from_secs_f64(if negative { -1.0 } else { f64::INFINITY });
        }
        let secs: f64 = if obj.hasattr("total_seconds")? {
            obj.call_method0("total_seconds")?.extract()?
        } else {
            obj.extract().map_err(|_| {
                PyTypeError::new_err(format!(
                    "timeout must be None, a number of seconds or a datetime.timedelta, not {}",
                    obj.get_type()
                        .name()
                        .map_or_else(|_| "?".into(), |n| n.to_string())
                ))
            })?
        };
        Timeout::from_secs_f64(secs)
    }
}

/// The longest `Backoff` sleep accepted, in seconds. The bindings wait in slices of at most 50 ms anyway, so longer
/// sleeps would change nothing.
const MAX_SLEEP_SECS: f64 = 3600.0;

/// How a `Reader` waits between polls before blocking on the writer's wake-up region (ADR 0017).
///
/// Each poll that finds nothing moves one step: `spins` busy polls, then `yields` polls that yield the CPU, then
/// sleeps from `min_sleep` doubling to `max_sleep` seconds. With wake-ups enabled, the sleep phase is replaced by
/// blocking until the writer publishes. All waiting happens with the GIL released. Sleeps are at most 3600 seconds.
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
        let sleep = |v: &Bound<'_, PyAny>, what: &str| -> PyResult<Duration> {
            let d = seconds(v, what)?;
            if d.as_secs_f64() > MAX_SLEEP_SECS {
                return Err(PyValueError::new_err(format!(
                    "{what} must be at most {MAX_SLEEP_SECS} seconds"
                )));
            }
            Ok(d)
        };
        if let Some(v) = min_sleep {
            inner.min_sleep = sleep(v, "min_sleep")?;
        }
        if let Some(v) = max_sleep {
            inner.max_sleep = sleep(v, "max_sleep")?;
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

    /// Value equality; other types compare as `NotImplemented`.
    fn __eq__(&self, other: PyRef<'_, Self>) -> bool {
        other.inner == self.inner
    }

    fn __hash__(&self) -> u64 {
        let b = &self.inner;
        hash_of(&(b.spins, b.yields, b.min_sleep, b.max_sleep))
    }
}

/// A retention policy and what to do with removed segments (ADR 0016). Build with `Retention.delete(...)` or
/// `Retention.move_to(directory, ...)`.
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
    fn move_to(
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

    /// The directory segments are moved to, or `None` for deletion.
    #[getter]
    fn move_to_directory(&self) -> Option<PathBuf> {
        match &self.action {
            tickertape::retention::Action::MoveTo(dir) => Some(dir.clone()),
            _ => None,
        }
    }

    /// Value equality (policy and action); other types compare as `NotImplemented`.
    fn __eq__(&self, other: PyRef<'_, Self>) -> bool {
        other.policy == self.policy && other.action == self.action
    }

    fn __hash__(&self) -> u64 {
        let p = &self.policy;
        hash_of(&(
            p.max_segments,
            p.max_bytes,
            p.max_age,
            self.move_to_directory(),
        ))
    }

    fn __repr__(&self) -> String {
        fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "None".to_string(), |v| v.to_string())
        }
        let action = match &self.action {
            tickertape::retention::Action::MoveTo(dir) => {
                format!("move_to({:?}, ", dir.display().to_string())
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

/// The `helper_cpus` argument of `Writer`: `"process"`, `"inherited"`, or an iterable of CPU numbers
/// (`HelperCpus::List`). A `str` other than the two names is refused, not iterated.
#[derive(Debug)]
pub struct HelperCpusArg(pub tickertape::HelperCpus);

impl HelperCpusArg {
    pub const PROCESS: HelperCpusArg = HelperCpusArg(tickertape::HelperCpus::Process);
}

impl<'a, 'py> FromPyObject<'a, 'py> for HelperCpusArg {
    type Error = PyErr;

    fn extract(obj: Borrowed<'a, 'py, PyAny>) -> PyResult<Self> {
        const EXPECTED: &str =
            "helper_cpus must be 'process', 'inherited' or a sequence of CPU numbers";
        if let Ok(name) = obj.extract::<String>() {
            return match name.as_str() {
                "process" => Ok(HelperCpusArg(tickertape::HelperCpus::Process)),
                "inherited" => Ok(HelperCpusArg(tickertape::HelperCpus::Inherited)),
                other => Err(PyValueError::new_err(format!("{EXPECTED}, not {other:?}"))),
            };
        }
        if obj.is_instance_of::<PyInt>() || obj.is_instance_of::<pyo3::types::PyBytes>() {
            return Err(PyTypeError::new_err(EXPECTED));
        }
        let mut cpus = Vec::new();
        for item in obj.try_iter().map_err(|_| PyTypeError::new_err(EXPECTED))? {
            cpus.push(item?.extract::<usize>()?);
        }
        Ok(HelperCpusArg(tickertape::HelperCpus::List(cpus)))
    }
}
