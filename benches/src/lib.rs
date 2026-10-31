//! Shared validation, scheduling and reporting for the benchmark tools.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use hdrhistogram::Histogram;
use serde_json::{Value, json};
use tickertape::format;

/// Fallible benchmark operation.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Strict, deliberately small parser for long options.
#[derive(Debug)]
pub struct Options {
    values: BTreeMap<String, String>,
    flags: BTreeSet<String>,
}

impl Options {
    /// Rejects unknown, repeated and missing-value options.
    pub fn parse(args: &[String], values: &[&str], flags: &[&str]) -> Result<Self> {
        let mut parsed = Self {
            values: BTreeMap::new(),
            flags: BTreeSet::new(),
        };
        let mut args = args.iter();
        while let Some(key) = args.next() {
            if flags.contains(&key.as_str()) {
                if !parsed.flags.insert(key.clone()) {
                    return Err(format!("repeated option {key}").into());
                }
            } else if values.contains(&key.as_str()) {
                let value = args
                    .next()
                    .filter(|v| !v.starts_with("--"))
                    .ok_or_else(|| format!("missing value for {key}"))?;
                if parsed.values.insert(key.clone(), value.clone()).is_some() {
                    return Err(format!("repeated option {key}").into());
                }
            } else {
                return Err(format!("unknown option {key}").into());
            }
        }
        Ok(parsed)
    }

    /// Returns an option's value, or an error if absent.
    pub fn required(&self, key: &str) -> Result<&str> {
        self.values
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| format!("missing {key}").into())
    }

    /// Parses an unsigned integer, using a default only if the option is absent.
    pub fn number(&self, key: &str, default: Option<u64>) -> Result<u64> {
        match self.values.get(key) {
            Some(value) => value
                .parse()
                .map_err(|_| format!("invalid integer for {key}: {value}").into()),
            None => default.ok_or_else(|| format!("missing {key}").into()),
        }
    }

    /// Whether a flag was present.
    pub fn flag(&self, key: &str) -> bool {
        self.flags.contains(key)
    }

    /// Returns an optional filesystem path.
    pub fn path(&self, key: &str) -> Option<PathBuf> {
        self.values.get(key).map(PathBuf::from)
    }
}

/// Creates a journal directory or accepts an empty one; never removes existing data.
pub fn fresh_directory(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    if std::fs::read_dir(dir)?.next().is_some() {
        return Err("journal directory must be empty; use a new --dir for each run".into());
    }
    Ok(())
}

/// Validates fixed-size records and returns the number that fit in one segment.
pub fn segment_capacity(segment_size: u64, payload: usize) -> Result<u64> {
    format::validate_segment_size(segment_size)
        .map_err(|e| format!("invalid segment size: {e}"))?;
    let frame = format::frame_len(payload).ok_or("payload too large")?;
    if !format::record_fits(segment_size, format::SEGMENT_HEADER_LEN as u64, frame) {
        return Err("payload does not fit in segment".into());
    }
    Ok(
        (segment_size - format::SEGMENT_HEADER_LEN as u64 - format::SEGMENT_TRAILER_LEN as u64)
            / format::align8(u64::from(frame)),
    )
}

/// Scheduled timestamp: floor(i * 1e9 / rate), with no cumulative rounding drift or rebasing.
pub fn intended_ns(t0: u64, index: u64, rate: u64) -> Result<u64> {
    if rate == 0 {
        return Err("rate must be positive".into());
    }
    let offset = u64::try_from(u128::from(index) * 1_000_000_000 / u128::from(rate))?;
    t0.checked_add(offset)
        .ok_or_else(|| "schedule timestamp overflow".into())
}

/// Converts an elapsed duration to histogram nanoseconds without truncating overflow.
pub fn elapsed_ns(duration: Duration) -> Result<u64> {
    Ok(duration.as_nanos().try_into()?)
}

/// Histogram output in microseconds; empty categories have null statistics.
pub fn histogram_json(hist: &Histogram<u64>) -> Value {
    let stat = |v: u64| {
        if hist.is_empty() {
            Value::Null
        } else {
            json!(v as f64 / 1000.0)
        }
    };
    json!({"count": hist.len(), "min_us": stat(hist.min()), "p50_us": stat(hist.value_at_quantile(0.5)),
        "p90_us": stat(hist.value_at_quantile(0.9)), "p99_us": stat(hist.value_at_quantile(0.99)),
        "p99_9_us": stat(hist.value_at_quantile(0.999)), "p99_99_us": stat(hist.value_at_quantile(0.9999)),
        "max_us": stat(hist.max())})
}

/// Prints a histogram with explicit units and category count.
pub fn print_histogram(label: &str, stats: &Value) {
    println!(
        "{label} (µs, count={}): min={} p50={} p90={} p99={} p99.9={} p99.99={} max={}",
        stats["count"],
        stats["min_us"],
        stats["p50_us"],
        stats["p90_us"],
        stats["p99_us"],
        stats["p99_9_us"],
        stats["p99_99_us"],
        stats["max_us"]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickertape::{NewRecord, Writer, WriterConfig};

    #[test]
    fn schedule_keeps_backlog_and_avoids_rounding_drift() {
        assert_eq!(intended_ns(100, 3, 3).unwrap(), 1_000_000_100);
        assert_eq!(intended_ns(100, 1, 3).unwrap(), 333_333_433);
        assert!(intended_ns(0, 1, 0).is_err());
        assert!(intended_ns(u64::MAX, 1, 1).is_err());
    }

    #[test]
    fn roll_prediction_matches_actual_segment_creation() {
        for payload in [0, 64, 1024, 65_000] {
            let dir = tempfile::tempdir().unwrap();
            let size = 65_536;
            let capacity = segment_capacity(size, payload).unwrap();
            let mut writer =
                Writer::open(dir.path(), WriterConfig::default().segment_size(size)).unwrap();
            let bytes = vec![0; payload];
            for i in 0..capacity * 2 + 1 {
                writer
                    .append(&NewRecord {
                        schema_id: 1,
                        venue_ts_ns: 0,
                        local_ts_ns: 0,
                        payload: &bytes,
                    })
                    .unwrap();
                let segments = std::fs::read_dir(dir.path())
                    .unwrap()
                    .filter(|p| {
                        p.as_ref()
                            .unwrap()
                            .path()
                            .extension()
                            .is_some_and(|e| e == "tape")
                    })
                    .count();
                assert_eq!(segments as u64, i / capacity + 1);
            }
        }
    }

    #[test]
    fn histogram_units_and_empty_categories() {
        let mut hist = Histogram::<u64>::new(3).unwrap();
        assert_eq!(histogram_json(&hist)["min_us"], Value::Null);
        hist.record(1000).unwrap();
        assert_eq!(histogram_json(&hist)["p50_us"], 1.0);
        assert_eq!(histogram_json(&hist)["count"], 1);
    }

    #[test]
    fn refuses_existing_data_and_bad_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep"), b"keep").unwrap();
        assert!(fresh_directory(dir.path()).is_err());
        assert!(segment_capacity(65_536, 65_536).is_err());
        for args in [
            vec!["--wat"],
            vec!["--count"],
            vec!["--count", "1", "--count", "2"],
        ] {
            let args: Vec<_> = args.into_iter().map(String::from).collect();
            assert!(Options::parse(&args, &["--count"], &[]).is_err());
        }
    }
}
