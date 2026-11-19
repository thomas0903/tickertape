//! Helpers for the cross-process tests: locate binaries, unique service names, child processes, journal scans.

#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tickertape::{Reader, ReaderConfig};
use tickertape_iceoryx2::layout::{self, Decoded, Descriptor, LossRecord};

/// The processes of different tests compete for CPU; run the cross-process tests one at a time so that
/// rate-dependent assertions stay meaningful on shared CI runners.
pub static SERIAL: Mutex<()> = Mutex::new(());

pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `tickertape-iox2` binary.
pub fn cli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tickertape-iox2"))
}

/// An example binary, built by `cargo test` next to the test executables.
pub fn example(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("test executable path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target directory");
    let path = dir.join("examples").join(name);
    assert!(
        path.exists(),
        "{} not built; run the tests with `cargo test --all-features`",
        path.display()
    );
    path
}

/// A service name no other test or run uses.
pub fn unique(name: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "tt-test/{}-{}-{}/{name}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// A child process that is killed if the test fails before it exits.
pub struct Proc {
    pub child: Option<Child>,
    pub what: String,
    // Output is drained by threads as it comes, so a chatty child never blocks on a full pipe.
    out: Option<std::thread::JoinHandle<String>>,
    err: Option<std::thread::JoinHandle<String>>,
}

fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = r.read_to_string(&mut s);
        s
    })
}

impl Proc {
    pub fn spawn(program: &Path, args: &[&str]) -> Proc {
        let mut child = Command::new(program)
            .args(args)
            .env(
                "IOX2_LOG_LEVEL",
                std::env::var("TT_TEST_IOX2_LOG").unwrap_or_else(|_| "Error".into()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", program.display()));
        let out = drain(child.stdout.take().expect("stdout"));
        let err = drain(child.stderr.take().expect("stderr"));
        Proc {
            child: Some(child),
            what: format!("{} {}", program.display(), args.join(" ")),
            out: Some(out),
            err: Some(err),
        }
    }

    pub fn pid(&self) -> i32 {
        self.child.as_ref().expect("running").id() as i32
    }

    pub fn signal(&self, sig: &str) {
        let status = Command::new("kill")
            .args([&format!("-{sig}"), &self.pid().to_string()])
            .status()
            .expect("kill");
        assert!(status.success(), "kill -{sig} {}", self.what);
    }

    /// Waits for the process to exit, at most `timeout`; returns status, stdout, stderr.
    pub fn wait(mut self, timeout: Duration) -> (ExitStatus, String, String) {
        let mut child = self.child.take().expect("running");
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let err = self
                    .err
                    .take()
                    .map(|h| h.join().unwrap_or_default())
                    .unwrap_or_default();
                panic!(
                    "{} did not exit within {timeout:?}; stderr:\n{err}",
                    self.what
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let out = self
            .out
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        let err = self
            .err
            .take()
            .map(|h| h.join().unwrap_or_default())
            .unwrap_or_default();
        (status, out, err)
    }

    /// Whether it has exited.
    pub fn exited(&mut self) -> bool {
        self.child
            .as_mut()
            .expect("running")
            .try_wait()
            .expect("try_wait")
            .is_some()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Runs a command to completion.
pub fn run(program: &Path, args: &[&str], timeout: Duration) -> (ExitStatus, String, String) {
    Proc::spawn(program, args).wait(timeout)
}

/// Runs a command that must succeed; returns stdout.
pub fn run_ok(program: &Path, args: &[&str], timeout: Duration) -> String {
    let (status, out, err) = run(program, args, timeout);
    assert!(
        status.success(),
        "{} {} failed ({status}):\n{err}",
        program.display(),
        args.join(" ")
    );
    out
}

/// Polls `f` until it returns true or `timeout` passes.
pub fn wait_until(timeout: Duration, what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(
            Instant::now() < deadline,
            "timed out after {timeout:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// One record of a recording, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum Rec {
    Descriptor(u64, Descriptor),
    Data {
        seq: u64,
        descriptor_seq: u64,
        local_ts_ns: u64,
        venue_ts_ns: u64,
        header: Vec<u8>,
        payload: Vec<u8>,
    },
    Loss(u64, LossRecord),
}

/// Reads every record of the journal, checking that sequence numbers start at 0 and have no gaps.
pub fn scan(dir: &Path) -> Vec<Rec> {
    let mut reader = Reader::open(dir, ReaderConfig::default()).expect("open journal");
    let mut out = Vec::new();
    let mut expected = 0;
    while let Some(r) = reader.try_next().expect("read journal") {
        assert_eq!(
            r.seq, expected,
            "tickertape sequence numbers must have no gaps or duplicates"
        );
        expected += 1;
        out.push(
            match layout::decode(r.schema_id, r.payload).expect("decode") {
                Decoded::Descriptor(d) => Rec::Descriptor(r.seq, *d),
                Decoded::Data(d) => Rec::Data {
                    seq: r.seq,
                    descriptor_seq: d.descriptor_seq,
                    local_ts_ns: r.local_ts_ns,
                    venue_ts_ns: r.venue_ts_ns,
                    header: d.user_header.to_vec(),
                    payload: d.payload.to_vec(),
                },
                Decoded::Loss(l) => Rec::Loss(r.seq, l),
                Decoded::Other => panic!("unexpected schema {:#x} at {}", r.schema_id, r.seq),
            },
        );
    }
    out
}

/// Number of records currently committed.
pub fn count(dir: &Path) -> u64 {
    match Reader::open(dir, ReaderConfig::default()) {
        Ok(mut r) => {
            let mut n = 0;
            while let Ok(Some(_)) = r.try_next() {
                n += 1;
            }
            n
        }
        Err(_) => 0,
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// A `data` line of the `follow` example.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followed {
    pub seq: u64,
    pub service: String,
    pub local_ts_ns: u64,
    pub venue_ts_ns: u64,
    pub header: Vec<u8>,
    pub payload: Vec<u8>,
}

pub fn parse_follow(out: &str) -> (Vec<(u64, String)>, Vec<Followed>) {
    let mut descriptors = Vec::new();
    let mut data = Vec::new();
    for line in out.lines() {
        let mut words = line.split_whitespace();
        let kind = words.next().unwrap_or("");
        let fields: std::collections::HashMap<&str, &str> =
            words.filter_map(|w| w.split_once('=')).collect();
        match kind {
            "descriptor" => descriptors.push((
                fields["seq"].parse().unwrap(),
                fields["service"].to_string(),
            )),
            "data" => data.push(Followed {
                seq: fields["seq"].parse().unwrap(),
                service: fields["service"].to_string(),
                local_ts_ns: fields["local_ts"].parse().unwrap(),
                venue_ts_ns: fields["venue_ts"].parse().unwrap(),
                header: unhex(fields["header"]),
                payload: unhex(fields["payload"]),
            }),
            _ => {}
        }
    }
    (descriptors, data)
}

/// Lines `<header hex> <payload hex>` of the `subscriber` example.
pub fn parse_subscriber(out: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    out.lines()
        .map(|l| {
            let (h, p) = l.split_once(' ').expect("two fields");
            (unhex(h), unhex(p))
        })
        .collect()
}

pub fn counter_of(header: &[u8]) -> u64 {
    u64::from_le_bytes(header[0..8].try_into().expect("header has a counter"))
}

pub const SECS: fn(u64) -> Duration = Duration::from_secs;
