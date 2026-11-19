//! Compiles the C99 and C++11 test programs and the examples against the committed header and the libraries Cargo
//! built for this crate, runs them, and checks the cross-language fixtures:
//!
//! - a journal written through the C ABI reads back identically with the Rust reader and the Python decoder
//!   (`python/tickertape_reader.py`), including durable-only reads and a segment roll;
//! - the C reader reads a journal written by the Rust writer.
//!
//! Environment:
//! - `TICKERTAPE_FFI_SANITIZE=1`: compile the C/C++ programs with AddressSanitizer and UBSan.
//! - `TICKERTAPE_FFI_VALGRIND=1`: run the C/C++ programs under Valgrind (Linux).
//! - `TICKERTAPE_FFI_REQUIRE_PYTHON=1`: fail instead of skipping the Python checks when no `python3` is found
//!   (`TICKERTAPE_PYTHON` overrides the interpreter).
//! - `CC`, `CXX`, `CFLAGS`, `CXXFLAGS`: as for the `cc` crate.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tickertape::{NewRecord, Reader, ReaderConfig, Writer, WriterConfig};

const FIXTURE_RECORDS: u64 = 1500;
const FIXTURE_CHECKPOINT_AT: u64 = 1000;
const FIXTURE_SEGMENT_SIZE: u64 = 65_536;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn repo_root() -> PathBuf {
    manifest_dir().join("../..")
}

/// The directory with `libtickertape.{a,so,dylib}`.
///
/// Cargo does not build a crate's `cdylib` or `staticlib` for its integration tests, so the first call runs
/// `cargo build -p tickertape-ffi` into a separate target directory (`target/ffi-c-api`; the outer build holds the
/// lock on the normal one). `TICKERTAPE_FFI_LIB_DIR` points at prebuilt libraries instead.
fn lib_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(dir) = std::env::var("TICKERTAPE_FFI_LIB_DIR") {
            return PathBuf::from(dir);
        }
        // target/<profile>/deps/c_api-<hash>
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap().join("ffi-c-api");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-p", "tickertape-ffi", "--manifest-path"])
            .arg(repo_root().join("Cargo.toml"))
            .arg("--target-dir")
            .arg(&target)
            .status()
            .expect("run cargo build");
        assert!(status.success(), "cargo build -p tickertape-ffi failed");
        let dir = target.join("debug");
        assert!(
            dir.join("libtickertape.a").exists(),
            "no libtickertape.a in {}",
            dir.display()
        );
        dir
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

fn target_triple() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        other => panic!("unsupported platform {other:?}"),
    }
}

/// System libraries a static link of the Rust library needs (`rustc --print native-static-libs`).
fn static_libs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["-liconv", "-lc", "-lm"]
    } else {
        &[
            "-lgcc_s",
            "-lutil",
            "-lrt",
            "-lpthread",
            "-lm",
            "-ldl",
            "-lc",
        ]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Link {
    Static,
    Shared,
}

/// Compiles `sources` into the executable `out`.
fn compile(sources: &[PathBuf], cpp: bool, link: Link, extra_includes: &[PathBuf], out: &Path) {
    let tool = cc::Build::new()
        .cargo_metadata(false)
        .cargo_warnings(false)
        .target(target_triple())
        .host(target_triple())
        .opt_level(1)
        .debug(true)
        .cpp(cpp)
        .get_compiler();
    let mut cmd = tool.to_command();
    cmd.args(if cpp {
        ["-std=c++11", "-pedantic"]
    } else {
        ["-std=c99", "-pedantic"]
    });
    cmd.args(["-Wall", "-Wextra", "-Werror"]);
    if env_flag("TICKERTAPE_FFI_SANITIZE") {
        cmd.args([
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-fno-omit-frame-pointer",
        ]);
    }
    cmd.arg("-I").arg(manifest_dir().join("include"));
    for include in extra_includes {
        cmd.arg("-I").arg(include);
    }
    cmd.args(sources);
    let libs = lib_dir();
    match link {
        Link::Static => {
            cmd.arg(libs.join("libtickertape.a"));
            cmd.args(static_libs());
        }
        Link::Shared => {
            cmd.arg("-L").arg(libs).arg("-ltickertape");
            cmd.arg(format!("-Wl,-rpath,{}", libs.display()));
        }
    }
    cmd.arg("-o").arg(out);
    let output = cmd.output().expect("run the C compiler");
    assert!(
        output.status.success(),
        "compiling {sources:?} failed: {cmd:?}\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The compiled programs, built once per test process.
struct Programs {
    _dir: tempfile::TempDir,
    c_test: PathBuf,
    cpp_test: PathBuf,
    writer: PathBuf,
    reader: PathBuf,
    example: PathBuf,
}

fn programs() -> &'static Programs {
    static PROGRAMS: OnceLock<Programs> = OnceLock::new();
    PROGRAMS.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let tests = manifest_dir().join("tests");
        let examples = repo_root().join("examples/c");
        let p = Programs {
            c_test: dir.path().join("test_api"),
            cpp_test: dir.path().join("test_wrapper"),
            writer: dir.path().join("writer"),
            reader: dir.path().join("reader"),
            example: dir.path().join("example"),
            _dir: dir,
        };
        compile(
            &[tests.join("c/test_api.c")],
            false,
            Link::Static,
            &[],
            &p.c_test,
        );
        compile(
            &[tests.join("cpp/test_wrapper.cpp")],
            true,
            Link::Shared,
            std::slice::from_ref(&examples),
            &p.cpp_test,
        );
        compile(
            &[examples.join("writer.c")],
            false,
            Link::Static,
            &[],
            &p.writer,
        );
        compile(
            &[examples.join("reader.c")],
            false,
            Link::Shared,
            &[],
            &p.reader,
        );
        compile(
            &[examples.join("example.cpp")],
            true,
            Link::Shared,
            std::slice::from_ref(&examples),
            &p.example,
        );
        p
    })
}

/// A command for a compiled program, under Valgrind if requested.
fn command(program: &Path) -> Command {
    let mut cmd = if env_flag("TICKERTAPE_FFI_VALGRIND") {
        let mut c = Command::new("valgrind");
        c.args([
            "--quiet",
            "--error-exitcode=99",
            "--leak-check=full",
            "--errors-for-leak-kinds=definite",
        ]);
        c.arg(program);
        c
    } else {
        Command::new(program)
    };
    // Leak checking is part of AddressSanitizer on Linux (macOS does not support it); make UBSan reports fatal.
    let leaks = if cfg!(target_os = "linux") { 1 } else { 0 };
    cmd.env("ASAN_OPTIONS", format!("detect_leaks={leaks}"))
        .env("UBSAN_OPTIONS", "print_stacktrace=1:halt_on_error=1");
    cmd
}

fn run_ok(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("run program");
    assert!(
        output.status.success(),
        "{cmd:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Waits for a child with a deadline, so a hung follower fails the test instead of hanging CI.
fn wait_with_deadline(mut child: Child, limit: Duration) -> Output {
    let deadline = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().ok();
            panic!("child did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "child failed with {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

// ---- The fixture pattern, shared with tests/c/test_api.c ----

fn pattern_payload(seq: u64) -> Vec<u8> {
    let len = (seq * 37 % 300) as usize;
    (0..len)
        .map(|i| ((seq as usize + 7 * i) & 0xFF) as u8)
        .collect()
}

fn check_pattern(record: &tickertape::Record<'_>, seq: u64) {
    assert_eq!(record.seq, seq);
    assert_eq!(record.schema_id, (seq % 5 + 1) as u32, "schema of {seq}");
    assert_eq!(record.venue_ts_ns, 1_000_000 + seq, "venue ts of {seq}");
    assert_eq!(record.local_ts_ns, 2_000_000 + 3 * seq, "local ts of {seq}");
    assert_eq!(record.payload, pattern_payload(seq), "payload of {seq}");
}

/// Reads every record with the Rust reader and checks it against the pattern; returns them as Python-style JSONL.
fn rust_read(dir: &Path, durable_only: bool) -> Vec<String> {
    let mut reader = Reader::open(dir, ReaderConfig::default().durable_only(durable_only)).unwrap();
    let mut lines = Vec::new();
    let mut seq = 0;
    while let Some(record) = reader.try_next().unwrap() {
        check_pattern(&record, seq);
        lines.push(format!(
            r#"{{"seq":{},"schema_id":{},"venue_ts_ns":{},"local_ts_ns":{},"payload_b64":"{}"}}"#,
            record.seq,
            record.schema_id,
            record.venue_ts_ns,
            record.local_ts_ns,
            base64(record.payload)
        ));
        seq += 1;
    }
    lines
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The JSONL the Python decoder prints for `dir`, or `None` if Python is unavailable (and not required).
fn python_read(dir: &Path, durable_only: bool) -> Option<Vec<String>> {
    let python = std::env::var("TICKERTAPE_PYTHON").unwrap_or_else(|_| "python3".into());
    if Command::new(&python).arg("--version").output().is_err() {
        assert!(
            !env_flag("TICKERTAPE_FFI_REQUIRE_PYTHON"),
            "{python} not found but TICKERTAPE_FFI_REQUIRE_PYTHON=1"
        );
        eprintln!("skipping the Python decoder checks: {python} not found");
        return None;
    }
    let mut cmd = Command::new(&python);
    cmd.arg("-I")
        .arg(repo_root().join("python/tickertape_reader.py"))
        .arg(dir);
    if durable_only {
        cmd.arg("--durable-only");
    }
    let output = run_ok(&mut cmd);
    Some(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
    )
}

/// Checks a fixture journal with the Rust reader, `verify`, the checkpoint marker and the Python decoder.
fn check_fixture_from_rust_and_python(dir: &Path) {
    let segments = tickertape::inspect::list_segments(dir).unwrap();
    assert!(segments.len() >= 2, "the fixture must roll");
    let verified = tickertape::inspect::verify(dir).unwrap();
    assert!(verified.problems.is_empty(), "{:?}", verified.problems);
    assert_eq!(verified.records, FIXTURE_RECORDS);
    let checkpoint = tickertape::inspect::read_checkpoint(dir).unwrap().unwrap();
    assert_eq!(checkpoint.next_seq, FIXTURE_CHECKPOINT_AT);

    let all = rust_read(dir, false);
    assert_eq!(all.len() as u64, FIXTURE_RECORDS);
    let durable = rust_read(dir, true);
    assert_eq!(durable.len() as u64, FIXTURE_CHECKPOINT_AT);
    assert_eq!(durable[..], all[..FIXTURE_CHECKPOINT_AT as usize]);
    if let Some(python) = python_read(dir, false) {
        assert_eq!(python.len(), all.len());
        for (p, r) in python.iter().zip(&all) {
            assert_eq!(p, r, "Python and Rust decoded a record differently");
        }
        assert_eq!(python_read(dir, true).unwrap(), durable);
    }
}

fn write_rust_fixture(dir: &Path) {
    let config = WriterConfig::default()
        .segment_size(FIXTURE_SEGMENT_SIZE)
        .prefault(false);
    let mut writer = Writer::open(dir, config).unwrap();
    for seq in 0..FIXTURE_RECORDS {
        if seq == FIXTURE_CHECKPOINT_AT {
            writer.checkpoint().unwrap();
        }
        let payload = pattern_payload(seq);
        let record = NewRecord {
            schema_id: (seq % 5 + 1) as u32,
            venue_ts_ns: 1_000_000 + seq,
            local_ts_ns: 2_000_000 + 3 * seq,
            payload: &payload,
        };
        let got = if seq % 2 == 0 {
            writer.append(&record).unwrap()
        } else {
            writer
                .append_with(&record, payload.len(), |buf| buf.copy_from_slice(&payload))
                .unwrap()
        };
        assert_eq!(got, seq);
    }
}

// ---- Tests ----

#[test]
fn c_api_basic() {
    let scratch = tempfile::tempdir().unwrap();
    run_ok(command(&programs().c_test).arg("basic").arg(scratch.path()));
}

#[test]
fn cpp_wrapper() {
    let scratch = tempfile::tempdir().unwrap();
    run_ok(command(&programs().cpp_test).arg(scratch.path()));
}

#[test]
fn journal_written_through_c_reads_back_in_rust_and_python() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("from-c");
    run_ok(command(&programs().c_test).arg("write-fixture").arg(&dir));
    check_fixture_from_rust_and_python(&dir);
    // The C reader agrees too.
    run_ok(
        command(&programs().c_test)
            .arg("read-fixture")
            .arg(&dir)
            .arg(FIXTURE_RECORDS.to_string())
            .arg(FIXTURE_CHECKPOINT_AT.to_string()),
    );
}

#[test]
fn journal_written_by_rust_reads_back_in_c() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("from-rust");
    write_rust_fixture(&dir);
    check_fixture_from_rust_and_python(&dir);
    run_ok(
        command(&programs().c_test)
            .arg("read-fixture")
            .arg(&dir)
            .arg(FIXTURE_RECORDS.to_string())
            .arg(FIXTURE_CHECKPOINT_AT.to_string()),
    );
}

#[test]
fn examples_record_follow_live_and_replay() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("example");
    let p = programs();
    // The follower starts before the journal exists and waits for the writer, in another process.
    let follower = command(&p.reader)
        .arg(&dir)
        .arg("follow")
        .arg("30")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    run_ok(command(&p.writer).arg(&dir).arg("30").arg("5"));
    let followed = wait_with_deadline(follower, Duration::from_secs(60));
    let followed = String::from_utf8(followed.stdout).unwrap();
    let lines: Vec<&str> = followed.lines().collect();
    assert_eq!(lines.len(), 30, "{followed}");
    assert!(lines[0].starts_with("0 schema=1 "), "{}", lines[0]);
    assert!(
        lines[29].ends_with("payload=tick 29 price 100.29"),
        "{}",
        lines[29]
    );

    let replay = run_ok(command(&p.reader).arg(&dir).arg("replay").arg("10"));
    let replay = String::from_utf8(replay.stdout).unwrap();
    assert_eq!(replay.lines().count(), 20);
    assert_eq!(replay.lines().collect::<Vec<_>>()[..], lines[10..]);

    let example = run_ok(command(&p.example).arg(&dir));
    let example = String::from_utf8(example.stdout).unwrap();
    let expected = [
        "30 order 0",
        "31 order 1",
        "32 order 2",
        "33 order 3",
        "34 order 4",
        "35 zero-copy",
    ];
    assert_eq!(example.lines().collect::<Vec<_>>(), expected);
    // The writer checkpointed at the end, so a durable-only replay sees everything.
    let durable = run_ok(
        command(&p.reader)
            .arg(&dir)
            .arg("replay")
            .env("TICKERTAPE_DURABLE_ONLY", "1"),
    );
    assert_eq!(
        String::from_utf8(durable.stdout).unwrap().lines().count(),
        36
    );
}
