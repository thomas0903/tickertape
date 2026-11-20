//! Placement of the writer's helper threads ([`HelperCpus`], ADR 0038 → Helper threads and CPU affinity).
//!
//! Each helper calls [`place_helper`] first thing when it starts. On Linux that sets the thread's affinity; on other
//! platforms it does nothing.

use crate::config::HelperCpus;

/// Places the calling helper thread as `cpus` asks. Best effort: a refused mask falls back to
/// [`HelperCpus::Process`], and a refused `Process` mask leaves the thread where it is.
pub(crate) fn place_helper(cpus: &HelperCpus) {
    #[cfg(target_os = "linux")]
    {
        use crate::mmap::{AFFINITY_CPUS, set_thread_affinity};
        match cpus {
            HelperCpus::Inherited => {}
            HelperCpus::List(list) => {
                if list.is_empty() || set_thread_affinity(list.iter().copied()).is_err() {
                    place_helper(&HelperCpus::Process);
                }
            }
            HelperCpus::Process => {
                let isolated = std::fs::read_to_string("/sys/devices/system/cpu/isolated")
                    .ok()
                    .and_then(|text| parse_cpu_list(&text))
                    .unwrap_or_default();
                let all = (0..AFFINITY_CPUS).filter(|cpu| !isolated.contains(cpu));
                // Every CPU: the kernel keeps those of the cpuset. If the cpuset holds only isolated CPUs, the first
                // mask leaves nothing (`EINVAL`), and the thread gets the whole cpuset.
                if set_thread_affinity(all).is_err() {
                    let _ = set_thread_affinity(0..AFFINITY_CPUS);
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cpus;
}

/// Parses the kernel's CPU list format (`0-3,8,10-11`; empty for none). `None` if malformed.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((from, to)) => {
                let (from, to) = (from.parse::<usize>().ok()?, to.parse::<usize>().ok()?);
                if from > to || to >= 1 << 16 {
                    return None;
                }
                cpus.extend(from..=to);
            }
            None => cpus.push(part.parse().ok()?),
        }
    }
    Some(cpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_as_the_kernel_writes_them() {
        assert_eq!(parse_cpu_list(""), Some(vec![]));
        assert_eq!(parse_cpu_list("\n"), Some(vec![]));
        assert_eq!(parse_cpu_list("3\n"), Some(vec![3]));
        assert_eq!(parse_cpu_list("0-2,5,7-8"), Some(vec![0, 1, 2, 5, 7, 8]));
        assert_eq!(parse_cpu_list("2-1"), None);
        assert_eq!(parse_cpu_list("a"), None);
    }

    /// A helper started from a thread pinned to one CPU gets the process's CPUs with the default, keeps the one CPU
    /// with `Inherited`, and gets the named CPU with `List` (and the process's CPUs for a list the kernel refuses).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_helper_started_from_a_pinned_thread_is_placed_as_configured() {
        use crate::mmap::{set_thread_affinity, thread_affinity};
        let allowed = thread_affinity().unwrap();
        if allowed.len() < 2 {
            eprintln!("skipped: one CPU available");
            return;
        }
        let pinned = allowed[0];
        let other = allowed[1];
        let process = std::thread::spawn(|| {
            place_helper(&HelperCpus::Process);
            thread_affinity().unwrap()
        })
        .join()
        .unwrap();
        let isolated = std::fs::read_to_string("/sys/devices/system/cpu/isolated")
            .ok()
            .and_then(|t| parse_cpu_list(&t))
            .unwrap_or_default();
        let expected: Vec<usize> = allowed
            .iter()
            .copied()
            .filter(|c| !isolated.contains(c))
            .collect();
        let expected = if expected.is_empty() {
            allowed.clone()
        } else {
            expected
        };
        assert_eq!(process, expected);
        std::thread::spawn(move || {
            set_thread_affinity([pinned]).unwrap();
            let start = |cpus: HelperCpus| {
                std::thread::spawn(move || {
                    place_helper(&cpus);
                    thread_affinity().unwrap()
                })
                .join()
                .unwrap()
            };
            assert_eq!(start(HelperCpus::Process), expected);
            assert_eq!(start(HelperCpus::Inherited), vec![pinned]);
            assert_eq!(start(HelperCpus::List(vec![other])), vec![other]);
            assert_eq!(start(HelperCpus::List(vec![])), expected);
            assert_eq!(
                start(HelperCpus::List(vec![AFFINITY_CPUS_PLUS_ONE])),
                expected
            );
        })
        .join()
        .unwrap();
    }

    #[cfg(target_os = "linux")]
    const AFFINITY_CPUS_PLUS_ONE: usize = crate::mmap::AFFINITY_CPUS + 1;
}
