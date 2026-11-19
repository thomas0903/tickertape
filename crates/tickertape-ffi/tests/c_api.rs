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
use std::sync::{Mutex, OnceLock};
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

fn rustc() -> Command {
    Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
}

/// Where this test binary was built: the target directory, the `--target` triple if the build had one (from
/// `--target` or `CARGO_BUILD_TARGET`), and the profile directory's name (`debug`, `release`, or a custom profile).
struct OuterLayout {
    target_dir: PathBuf,
    triple: Option<String>,
    profile_dir: String,
}

fn outer_layout() -> &'static OuterLayout {
    static LAYOUT: OnceLock<OuterLayout> = OnceLock::new();
    LAYOUT.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let targets = rustc()
            .args(["--print", "target-list"])
            .output()
            .expect("run rustc --print target-list");
        let targets = String::from_utf8_lossy(&targets.stdout).into_owned();
        layout_of(&exe, |name| targets.lines().any(|t| t == name)).unwrap_or_else(|| {
            panic!(
                "cannot tell the profile directory from the test binary's path {}; set TICKERTAPE_FFI_LIB_DIR",
                exe.display()
            )
        })
    })
}

/// Works out the layout from the path of a test binary. Cargo has used two layouts:
/// `<target-dir>/[<triple>/]<profile>/deps/<name>-<hash>`, and (newer versions)
/// `<target-dir>/[<triple>/]<profile>/build/<package>/<hash>/out/<name>-<hash>`.
fn layout_of(exe: &Path, is_triple: impl Fn(&str) -> bool) -> Option<OuterLayout> {
    let name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    let dir = exe.parent()?;
    let profile_dir = match name(dir)?.as_str() {
        "deps" => dir.parent()?,
        "out" => {
            let build = dir.parent()?.parent()?.parent()?;
            if name(build)? != "build" {
                return None;
            }
            build.parent()?
        }
        _ => return None,
    };
    let parent = profile_dir.parent()?;
    let parent_name = name(parent).unwrap_or_default();
    let triple = is_triple(&parent_name).then_some(parent_name);
    Some(OuterLayout {
        target_dir: if triple.is_some() {
            parent.parent()?.to_path_buf()
        } else {
            parent.to_path_buf()
        },
        triple,
        profile_dir: name(profile_dir)?,
    })
}

#[test]
fn layout_is_found_in_both_cargo_layouts() {
    let is_triple = |name: &str| name == "aarch64-unknown-linux-gnu";
    for (exe, target_dir, triple, profile) in [
        ("/t/debug/deps/c_api-0123", "/t", None, "debug"),
        (
            "/t/debug/build/tickertape-ffi/0123/out/c_api-0123",
            "/t",
            None,
            "debug",
        ),
        (
            "/t/aarch64-unknown-linux-gnu/release/deps/c_api-0123",
            "/t",
            Some("aarch64-unknown-linux-gnu"),
            "release",
        ),
        (
            "/t/aarch64-unknown-linux-gnu/custom/build/tickertape-ffi/0123/out/c_api-0123",
            "/t",
            Some("aarch64-unknown-linux-gnu"),
            "custom",
        ),
    ] {
        let layout = layout_of(Path::new(exe), is_triple).unwrap_or_else(|| panic!("{exe}"));
        assert_eq!(layout.target_dir, Path::new(target_dir), "{exe}");
        assert_eq!(layout.triple.as_deref(), triple, "{exe}");
        assert_eq!(layout.profile_dir, profile, "{exe}");
    }
    assert!(layout_of(Path::new("/t/debug/elsewhere/c_api-0123"), is_triple).is_none());
    assert!(layout_of(Path::new("/t/debug/x/y/z/out/c_api-0123"), is_triple).is_none());
}

/// The target triple to compile the C programs for: the outer build's `--target`, or rustc's host.
fn target_triple() -> &'static str {
    static TRIPLE: OnceLock<String> = OnceLock::new();
    TRIPLE.get_or_init(|| {
        outer_layout().triple.clone().unwrap_or_else(|| {
            let output = rustc().arg("-vV").output().expect("run rustc -vV");
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
                .expect("rustc -vV prints the host triple")
        })
    })
}

/// The directory with `libtickertape.{a,so,dylib}`.
///
/// Cargo does not build a crate's `cdylib` or `staticlib` for its integration tests, so the first call runs
/// `cargo build -p tickertape-ffi --locked` with the outer build's profile and target into a separate target
/// directory (`<target-dir>/ffi-c-api`; the outer build holds the lock on the normal one), and returns the
/// matching directory there. `TICKERTAPE_FFI_LIB_DIR` points at prebuilt libraries instead.
fn lib_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        if let Ok(dir) = std::env::var("TICKERTAPE_FFI_LIB_DIR") {
            return PathBuf::from(dir);
        }
        let layout = outer_layout();
        let target_dir = layout.target_dir.join("ffi-c-api");
        let profile = match layout.profile_dir.as_str() {
            "debug" => "dev",
            name => name,
        };
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.args([
            "build",
            "-p",
            "tickertape-ffi",
            "--locked",
            "--profile",
            profile,
        ])
        .arg("--manifest-path")
        .arg(repo_root().join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target_dir);
        let mut dir = target_dir.clone();
        if let Some(triple) = &layout.triple {
            cmd.args(["--target", triple]);
            dir.push(triple);
        }
        dir.push(&layout.profile_dir);
        let status = cmd.status().expect("run cargo build");
        assert!(status.success(), "{cmd:?} failed");
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

/// System libraries a static link of the Rust library needs (`rustc --print native-static-libs`).
fn static_libs() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["-liconv", "-lc", "-lm"]
    } else if cfg!(target_env = "musl") {
        &["-lc"]
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
    /// No tickertape library: the program defines the C functions it uses itself (stubs).
    None,
}

/// Compiles `sources` as language standard `std` (`c99`, `c++20`, ...) into the executable `out`.
fn compile(sources: &[PathBuf], std: &str, link: Link, extra_includes: &[PathBuf], out: &Path) {
    let cpp = std.starts_with("c++");
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
    cmd.arg(format!("-std={std}"));
    cmd.args(["-Wall", "-Wextra", "-Werror", "-pedantic"]);
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
    match link {
        Link::Static => {
            cmd.arg(lib_dir().join("libtickertape.a"));
            cmd.args(static_libs());
        }
        Link::Shared => {
            let libs = lib_dir();
            cmd.arg("-L").arg(libs).arg("-ltickertape");
            cmd.arg(format!("-Wl,-rpath,{}", libs.display()));
        }
        Link::None => {}
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
    /// `tests/c/test_api.c` as C99 (linked statically) and as C11 (linked dynamically).
    c_test: PathBuf,
    c11_test: PathBuf,
    /// `tests/cpp/test_wrapper.cpp` as C++11 and C++20.
    cpp_test: PathBuf,
    cpp20_test: PathBuf,
    /// `tests/cpp/test_wrapper_stub.cpp` as C++11, with stubbed C functions instead of the library.
    cpp_stub_test: PathBuf,
    writer: PathBuf,
    reader: PathBuf,
    example: PathBuf,
}

/// Owns the temporary directory with the compiled programs. A `static` is never dropped, so a `TempDir` kept in
/// `Programs` would outlive the process and leave the programs behind (about 12 MB per run); `remove_programs`
/// drops it when the process exits.
static PROGRAMS_DIR: Mutex<Option<tempfile::TempDir>> = Mutex::new(None);

extern "C" fn remove_programs() {
    // `try_lock`: never block process exit. A poisoned lock still holds the directory.
    let dir = match PROGRAMS_DIR.try_lock() {
        Ok(mut dir) => dir.take(),
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().take(),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    drop(dir);
}

unsafe extern "C" {
    /// C's `atexit`: runs `callback` when the process exits normally (the test harness returns from `main` or
    /// calls `exit`).
    fn atexit(callback: extern "C" fn()) -> std::ffi::c_int;
}

fn programs() -> &'static Programs {
    static PROGRAMS: OnceLock<Programs> = OnceLock::new();
    PROGRAMS.get_or_init(|| {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        *PROGRAMS_DIR.lock().unwrap() = Some(tmp);
        // SAFETY: `remove_programs` is an `extern "C" fn()` that never unwinds (it only drops a `TempDir`, whose
        // destructor ignores errors) and stays valid for the life of the process.
        let registered = unsafe { atexit(remove_programs) };
        assert_eq!(registered, 0, "atexit failed");
        let tests = manifest_dir().join("tests");
        let examples = repo_root().join("examples/c");
        let p = Programs {
            c_test: dir.join("test_api"),
            c11_test: dir.join("test_api_c11"),
            cpp_test: dir.join("test_wrapper"),
            cpp20_test: dir.join("test_wrapper_cpp20"),
            cpp_stub_test: dir.join("test_wrapper_stub"),
            writer: dir.join("writer"),
            reader: dir.join("reader"),
            example: dir.join("example"),
        };
        let c_test = [tests.join("c/test_api.c")];
        let cpp_test = [tests.join("cpp/test_wrapper.cpp")];
        let wrapper = std::slice::from_ref(&examples);
        compile(&c_test, "c99", Link::Static, &[], &p.c_test);
        compile(&c_test, "c11", Link::Shared, &[], &p.c11_test);
        compile(&cpp_test, "c++11", Link::Shared, wrapper, &p.cpp_test);
        compile(&cpp_test, "c++20", Link::Static, wrapper, &p.cpp20_test);
        compile(
            &[tests.join("cpp/test_wrapper_stub.cpp")],
            "c++11",
            Link::None,
            wrapper,
            &p.cpp_stub_test,
        );
        compile(
            &[examples.join("writer.c")],
            "c99",
            Link::Static,
            &[],
            &p.writer,
        );
        compile(
            &[examples.join("reader.c")],
            "c99",
            Link::Shared,
            &[],
            &p.reader,
        );
        compile(
            &[examples.join("example.cpp")],
            "c++11",
            Link::Shared,
            wrapper,
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
    for program in [&programs().c_test, &programs().c11_test] {
        let scratch = tempfile::tempdir().unwrap();
        run_ok(command(program).arg("basic").arg(scratch.path()));
    }
}

#[test]
fn cpp_wrapper() {
    for program in [&programs().cpp_test, &programs().cpp20_test] {
        let scratch = tempfile::tempdir().unwrap();
        run_ok(command(program).arg(scratch.path()));
    }
}

/// The wrapper's `append_with` when the fill throws: a clean abort rethrows the fill's exception; a failed clean-up
/// throws the library error with the fill's exception nested (stubbed C functions produce the failure).
#[test]
fn cpp_wrapper_reports_a_failed_cleanup_after_a_throwing_fill() {
    run_ok(&mut command(&programs().cpp_stub_test));
}

/// Documents one platform's outcome, not a promise: an exception escaping a fill callback is unsupported, and Rust
/// leaves it unspecified whether a foreign exception reaching `catch_unwind` aborts the process or is caught as an
/// opaque error. On the platforms CI runs (Linux and macOS with the system C++ runtime) the process aborts; if a
/// toolchain update changes that, update the docs' description of the observed behaviour, not the contract.
#[test]
fn cpp_exception_escaping_a_fill_callback_aborts() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = tempfile::tempdir().unwrap();
    let output = command(&programs().cpp_test)
        .arg(scratch.path())
        .arg("throw-from-fill")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("caught"),
        "the exception reached the caller"
    );
    assert!(
        output.status.signal() == Some(6) || stderr.contains("foreign exception"),
        "{}: {stderr}",
        output.status
    );
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
