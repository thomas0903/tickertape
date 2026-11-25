//! CPU affinity of the benchmark threads, read back from the kernel: what is in effect, not what was requested.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::Result;
use crate::sustained::pin_current_thread;

/// Pins the calling thread to `cpu`, if given, or fails. Unlike [`pin_current_thread`], a platform without hard
/// affinity is an error: a run that asked for a CPU must not report a result without it.
pub fn pin_or_fail(cpu: Option<u64>) -> Result<()> {
    let Some(cpu) = cpu else { return Ok(()) };
    if !cfg!(target_os = "linux") {
        return Err("CPU pinning is unsupported on this OS".into());
    }
    pin_current_thread(cpu.try_into()?)?;
    Ok(())
}

/// Parses the kernel's CPU list format (`0-3,8`).
pub fn parse_kernel_cpu_list(text: &str) -> Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((from, to)) => cpus.extend(from.parse::<usize>()?..=to.parse::<usize>()?),
            None => cpus.push(part.parse()?),
        }
    }
    Ok(cpus)
}

fn status_field<'a>(status: &'a str, name: &str) -> Result<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
        .ok_or_else(|| format!("no {name} in /proc status").into())
}

fn thread_json(task: &Path) -> Result<Value> {
    let status = std::fs::read_to_string(task.join("status"))?;
    Ok(json!({"name": status_field(&status, "Name")?,
        "cpus_allowed": parse_kernel_cpu_list(status_field(&status, "Cpus_allowed_list")?)?}))
}

/// The kernel's id of the calling thread (Linux).
pub fn thread_id() -> Result<u64> {
    let me = std::fs::read_link("/proc/thread-self")?;
    let name = me.file_name().and_then(|name| name.to_str());
    Ok(name.ok_or("no thread id in /proc/thread-self")?.parse()?)
}

/// The affinity in effect for the calling thread (`cpus_allowed`, and `last_cpu`, where it ran last) and for every
/// other thread of the process, from `/proc`. Elsewhere than on Linux: that it is unsupported. Call it after the
/// measurement: if this thread is pinned, it first gives the comparison runner time to move threads that have just
/// started on its CPU (`compare/README.md`, Thread pinning).
pub fn report() -> Result<Value> {
    if !cfg!(target_os = "linux") {
        let os = match std::env::consts::OS {
            "macos" => "macOS",
            other => other,
        };
        return Ok(json!({"unsupported": format!("affinity: unsupported on {os}")}));
    }
    let tid = thread_id()?.to_string();
    let mut report = thread_json(Path::new("/proc/thread-self"))?;
    let stat = std::fs::read_to_string("/proc/thread-self/stat")?;
    // Field 39, `processor`; the name in field 2 may contain spaces, so count from the last parenthesis (field 3 on).
    let last_cpu = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(36))
        .ok_or("no processor field in /proc stat")?
        .parse::<usize>()?;
    report["tid"] = json!(tid.parse::<u64>()?);
    report["last_cpu"] = json!(last_cpu);
    if report["cpus_allowed"]
        .as_array()
        .is_some_and(|c| c.len() == 1)
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut others = Vec::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let task = task?;
        if task.file_name().to_str() != Some(tid.as_str()) {
            // A thread that exits between the listing and the read is gone, not misplaced.
            match thread_json(&task.path()) {
                Ok(thread) => others.push(thread),
                Err(_) if !task.path().exists() => {}
                Err(error) => return Err(error),
            }
        }
    }
    report["other_threads"] = json!(others);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kernel_cpu_lists() {
        assert_eq!(
            parse_kernel_cpu_list("0-2,5,7-8\n").unwrap(),
            [0, 1, 2, 5, 7, 8]
        );
        assert!(parse_kernel_cpu_list("").unwrap().is_empty());
        assert!(parse_kernel_cpu_list("a").is_err());
    }

    #[test]
    fn reports_the_affinity_in_effect_for_this_and_the_other_threads() {
        std::thread::spawn(|| {
            let before = report().unwrap();
            if !cfg!(target_os = "linux") {
                assert!(
                    before["unsupported"]
                        .as_str()
                        .unwrap()
                        .contains("unsupported")
                );
                assert!(pin_or_fail(Some(0)).is_err() && pin_or_fail(None).is_ok());
                return;
            }
            let cpu = before["cpus_allowed"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .as_u64()
                .unwrap();
            pin_or_fail(Some(cpu)).unwrap();
            let after = report().unwrap();
            assert_eq!(after["cpus_allowed"], json!([cpu]));
            assert_eq!(after["last_cpu"], json!(cpu));
            assert!(!after["other_threads"].as_array().unwrap().is_empty());
            assert_ne!(after["tid"], json!(std::process::id()));
        })
        .join()
        .unwrap();
    }
}
