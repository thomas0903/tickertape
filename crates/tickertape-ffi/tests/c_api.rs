//! Compiles the C test programs, the C++ binding's test programs (`include/tickertape.hpp`, ADR 0036) and the
//! examples against the committed headers and the libraries Cargo built for this crate, runs them, and checks the
//! cross-language fixtures:
//!
//! - a journal written through the C ABI or the C++ binding reads back identically with the Rust reader, the Python
//!   decoder (`python/tickertape_reader.py`) and the other language, including durable-only reads and a segment roll;
//! - the C and C++ readers read a journal written by the Rust writer.
//!
//! It also checks that the C++ binding covers every C function and status, compiles at every language level, and
//! allocates nothing on the hot path.
//!
//! Environment:
//! - `TICKERTAPE_FFI_SANITIZE=1`: compile the C/C++ programs with AddressSanitizer and UBSan.
//! - `TICKERTAPE_FFI_VALGRIND=1`: run the C/C++ programs under Valgrind (Linux).
//! - `TICKERTAPE_FFI_CXX_COMPILERS`: the C++ compilers for the language-level matrix (space-separated; default: the
//!   `cc` crate's, plus `g++` and `clang++` if found).
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

/// The warnings the C++ binding promises to compile without (ADR 0036), on top of `-Wall -Wextra -Werror`.
const CPP_BINDING_WARNINGS: &[&str] = &["-Wpedantic", "-Wconversion", "-Wshadow"];

/// The C or C++ compiler the `cc` crate picks for the target (honours `CC`, `CXX`, `CFLAGS`, `CXXFLAGS`).
fn compiler(cpp: bool) -> cc::Tool {
    cc::Build::new()
        .cargo_metadata(false)
        .cargo_warnings(false)
        .target(target_triple())
        .host(target_triple())
        .opt_level(1)
        .debug(true)
        .cpp(cpp)
        .get_compiler()
}

/// Compiles `sources` as language standard `std` (`c99`, `c++17`, ...) with the extra `flags` into the executable
/// `out`.
fn compile(sources: &[PathBuf], std: &str, link: Link, flags: &[&str], out: &Path) {
    let cpp = std.starts_with("c++");
    let mut cmd = compiler(cpp).to_command();
    cmd.arg(format!("-std={std}"));
    cmd.args(["-Wall", "-Wextra", "-Werror", "-pedantic"]);
    cmd.args(flags);
    if env_flag("TICKERTAPE_FFI_SANITIZE") {
        cmd.args([
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-fno-omit-frame-pointer",
        ]);
    }
    cmd.arg("-I").arg(manifest_dir().join("include"));
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

/// Sources, language standard, link mode, extra flags and output of one program.
type CompileJob<'a> = (Vec<PathBuf>, &'a str, Link, &'a [&'a str], &'a Path);

/// The compiled programs, built once per test process.
struct Programs {
    /// `tests/c/test_api.c` as C99 (linked statically) and as C11 (linked dynamically).
    c_test: PathBuf,
    c11_test: PathBuf,
    /// `tests/cpp/test_c_header.cpp`: the C header alone from C++11 and C++20.
    c_header_cpp11: PathBuf,
    c_header_cpp20: PathBuf,
    /// `tests/cpp/test_binding.cpp` as C++17 (shared), C++20 (static) and C++17 without exceptions (shared).
    binding: PathBuf,
    binding_cpp20: PathBuf,
    binding_no_exceptions: PathBuf,
    /// `tests/cpp/test_binding_stub.cpp` as C++17, with stubbed C functions instead of the library.
    binding_stub: PathBuf,
    /// `tests/cpp/test_no_alloc.cpp` as C++17.
    no_alloc: PathBuf,
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
            c_header_cpp11: dir.join("test_c_header_cpp11"),
            c_header_cpp20: dir.join("test_c_header_cpp20"),
            binding: dir.join("test_binding"),
            binding_cpp20: dir.join("test_binding_cpp20"),
            binding_no_exceptions: dir.join("test_binding_no_exceptions"),
            binding_stub: dir.join("test_binding_stub"),
            no_alloc: dir.join("test_no_alloc"),
            writer: dir.join("writer"),
            reader: dir.join("reader"),
            example: dir.join("example"),
        };
        let c_test = vec![tests.join("c/test_api.c")];
        let c_header = vec![tests.join("cpp/test_c_header.cpp")];
        let binding = vec![tests.join("cpp/test_binding.cpp")];
        let strict = CPP_BINDING_WARNINGS;
        let no_exceptions = [CPP_BINDING_WARNINGS, &["-fno-exceptions"]].concat();
        let jobs: Vec<CompileJob<'_>> = vec![
            (c_test.clone(), "c99", Link::Static, &[], &p.c_test),
            (c_test, "c11", Link::Shared, &[], &p.c11_test),
            (
                c_header.clone(),
                "c++11",
                Link::Shared,
                &[],
                &p.c_header_cpp11,
            ),
            (c_header, "c++20", Link::Static, &[], &p.c_header_cpp20),
            (binding.clone(), "c++17", Link::Shared, strict, &p.binding),
            (
                binding.clone(),
                "c++20",
                Link::Static,
                strict,
                &p.binding_cpp20,
            ),
            (
                binding,
                "c++17",
                Link::Shared,
                &no_exceptions,
                &p.binding_no_exceptions,
            ),
            (
                vec![tests.join("cpp/test_binding_stub.cpp")],
                "c++17",
                Link::None,
                strict,
                &p.binding_stub,
            ),
            (
                vec![tests.join("cpp/test_no_alloc.cpp")],
                "c++17",
                Link::Shared,
                strict,
                &p.no_alloc,
            ),
            (
                vec![examples.join("writer.c")],
                "c99",
                Link::Static,
                &[],
                &p.writer,
            ),
            (
                vec![examples.join("reader.c")],
                "c99",
                Link::Shared,
                &[],
                &p.reader,
            ),
            (
                vec![examples.join("example.cpp")],
                "c++17",
                Link::Shared,
                strict,
                &p.example,
            ),
        ];
        lib_dir(); // build the library once, before the compilers run in parallel
        std::thread::scope(|scope| {
            for (sources, std, link, flags, out) in &jobs {
                scope.spawn(move || compile(sources, std, *link, flags, out));
            }
        });
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

/// The C header on its own compiles and works as C++11 and C++20 (ADR 0028), independently of the C++17 binding.
#[test]
fn c_header_from_cpp11_and_cpp20() {
    for program in [&programs().c_header_cpp11, &programs().c_header_cpp20] {
        run_ok(&mut command(program));
    }
}

/// The C++ binding's whole surface against the real library, as C++17, C++20 and C++17 without exceptions.
#[test]
fn cpp_binding() {
    let p = programs();
    for program in [&p.binding, &p.binding_cpp20, &p.binding_no_exceptions] {
        let scratch = tempfile::tempdir().unwrap();
        run_ok(command(program).arg(scratch.path()));
    }
}

/// A throwing fill followed by a failed clean-up, every status and corruption reason, and a mismatched library,
/// against stubbed C functions.
#[test]
fn cpp_binding_against_stubs() {
    run_ok(&mut command(&programs().binding_stub));
}

/// The binding allocates nothing on the append and read paths (it counts `operator new`).
#[test]
fn cpp_binding_allocates_nothing_on_the_hot_path() {
    let scratch = tempfile::tempdir().unwrap();
    run_ok(command(&programs().no_alloc).arg(scratch.path()));
}

/// Debug builds of the binding abort when a record view's payload is read after the next call on its reader.
#[test]
fn cpp_binding_detects_a_stale_view_in_debug_builds() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = tempfile::tempdir().unwrap();
    let output = command(&programs().binding)
        .arg(scratch.path())
        .arg("stale-view")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("about to read a stale view"),
        "{stdout}{stderr}"
    );
    assert!(!stdout.contains("not detected"), "{stdout}{stderr}");
    assert!(
        stderr.contains("used after the next call on its Reader"),
        "{stderr}"
    );
    assert!(
        output.status.signal() == Some(6) || !output.status.success(),
        "{}",
        output.status
    );
}

/// Documents one platform's outcome, not a promise: an exception escaping a fill callback is unsupported, and Rust
/// leaves it unspecified whether a foreign exception reaching `catch_unwind` aborts the process or is caught as an
/// opaque error. On the platforms CI runs (Linux and macOS with the system C++ runtime) the process aborts; if a
/// toolchain update changes that, update the docs' description of the observed behaviour, not the contract. (The C++
/// binding never lets this happen: its trampoline catches everything.)
#[test]
fn cpp_exception_escaping_a_raw_fill_callback_aborts() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = tempfile::tempdir().unwrap();
    let output = command(&programs().binding)
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

fn read_fixture_in(program: &Path, dir: &Path) {
    let mut cmd = command(program);
    cmd.arg("read-fixture")
        .arg(dir)
        .arg(FIXTURE_RECORDS.to_string())
        .arg(FIXTURE_CHECKPOINT_AT.to_string());
    run_ok(&mut cmd);
}

#[test]
fn journal_written_through_c_reads_back_in_rust_python_and_cpp() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("from-c");
    run_ok(command(&programs().c_test).arg("write-fixture").arg(&dir));
    check_fixture_from_rust_and_python(&dir);
    read_fixture_in(&programs().c_test, &dir);
    read_fixture_in(&programs().binding, &dir);
}

#[test]
fn journal_written_through_cpp_reads_back_in_rust_python_and_c() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("from-cpp");
    run_ok(command(&programs().binding).arg("write-fixture").arg(&dir));
    check_fixture_from_rust_and_python(&dir);
    read_fixture_in(&programs().c_test, &dir);
    read_fixture_in(&programs().binding_no_exceptions, &dir);
}

#[test]
fn journal_written_by_rust_reads_back_in_c_and_cpp() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("from-rust");
    write_rust_fixture(&dir);
    check_fixture_from_rust_and_python(&dir);
    read_fixture_in(&programs().c_test, &dir);
    read_fixture_in(&programs().binding_cpp20, &dir);
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

    // The writer checkpointed at the end, so a durable-only replay sees everything.
    let durable = run_ok(
        command(&p.reader)
            .arg(&dir)
            .arg("replay")
            .env("TICKERTAPE_DURABLE_ONLY", "1"),
    );
    assert_eq!(
        String::from_utf8(durable.stdout).unwrap().lines().count(),
        30
    );
}

#[test]
fn cpp_example_records_follows_live_and_replays() {
    let scratch = tempfile::tempdir().unwrap();
    let dir = scratch.path().join("example");
    let p = programs();
    let follower = command(&p.example)
        .arg(&dir)
        .arg("follow")
        .arg("30")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    run_ok(
        command(&p.example)
            .arg(&dir)
            .arg("record")
            .arg("30")
            .arg("5"),
    );
    let followed = wait_with_deadline(follower, Duration::from_secs(60));
    let followed = String::from_utf8(followed.stdout).unwrap();
    let lines: Vec<&str> = followed.lines().collect();
    let expected: Vec<String> = (0..30)
        .map(|i| format!("{i} schema={} order {i}", 1 + i % 2))
        .collect();
    assert_eq!(lines, expected, "{followed}");

    let replay = run_ok(command(&p.example).arg(&dir).arg("replay").arg("10"));
    let replay = String::from_utf8(replay.stdout).unwrap();
    assert_eq!(replay.lines().collect::<Vec<_>>()[..], lines[10..]);
    // The C reader agrees.
    let c_replay = run_ok(command(&p.reader).arg(&dir).arg("replay"));
    assert_eq!(
        String::from_utf8(c_replay.stdout).unwrap().lines().count(),
        30
    );
    // It checkpointed at the end, so a durable-only replay sees everything.
    let durable = run_ok(
        command(&p.example)
            .arg(&dir)
            .arg("replay")
            .env("TICKERTAPE_DURABLE_ONLY", "1"),
    );
    assert_eq!(String::from_utf8(durable.stdout).unwrap(), followed);
}

// ---- The C++ binding's coverage of the C ABI (ADR 0036) ----

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn c_header() -> String {
    read_text(&manifest_dir().join("include/tickertape.h"))
}

fn cpp_header() -> String {
    read_text(&manifest_dir().join("include/tickertape.hpp"))
}

/// C or C++ source without its comments, and with string and character literals emptied, so that a name in a
/// comment or a message does not count as code.
fn strip_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '\'' {
            out.push(c);
            i += 1;
            while i < chars.len() && chars[i] != c {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            out.push(c);
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The names of the functions `tickertape.h` declares (one per declaration line inside `extern "C"`).
fn c_functions(header: &str) -> Vec<String> {
    header
        .lines()
        .filter(|line| {
            line.starts_with(|c: char| c.is_ascii_alphabetic())
                && !line.starts_with("typedef")
                && !line.starts_with("enum")
                && !line.starts_with("extern")
        })
        .filter_map(|line| {
            let before = &line[..line.find('(')?];
            let name = before
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .next()?;
            name.starts_with("tickertape_").then(|| name.to_owned())
        })
        .collect()
}

/// Every C function the C++ binding calls, with the C++ declaration that wraps it.
const CPP_WRAPPED: &[(&str, &str)] = &[
    (
        "tickertape_abi_version",
        "library_abi_version, library_compatible",
    ),
    (
        "tickertape_version_number",
        "library_version_number, library_compatible",
    ),
    ("tickertape_version", "library_version, check_library"),
    ("tickertape_realtime_ns", "realtime_ns"),
    ("tickertape_verify", "verify"),
    ("tickertape_list_segments", "list_segments"),
    ("tickertape_read_checkpoint", "read_checkpoint"),
    (
        "tickertape_retention_apply",
        "apply_retention, plan_retention",
    ),
    ("tickertape_writer_options_new", "WriterOptions()"),
    ("tickertape_writer_options_free", "~WriterOptions"),
    (
        "tickertape_writer_options_set_segment_size",
        "WriterOptions::segment_size",
    ),
    (
        "tickertape_writer_options_set_recovery_policy",
        "WriterOptions::recovery_policy",
    ),
    (
        "tickertape_writer_options_set_notify",
        "WriterOptions::notify",
    ),
    (
        "tickertape_writer_options_set_prefault",
        "WriterOptions::prefault",
    ),
    (
        "tickertape_writer_options_set_retention",
        "WriterOptions::retention, no_retention",
    ),
    ("tickertape_reader_options_new", "ReaderOptions()"),
    ("tickertape_reader_options_free", "~ReaderOptions"),
    (
        "tickertape_reader_options_set_durable_only",
        "ReaderOptions::durable_only",
    ),
    (
        "tickertape_reader_options_set_notify",
        "ReaderOptions::notify",
    ),
    (
        "tickertape_reader_options_set_backoff",
        "ReaderOptions::backoff",
    ),
    (
        "tickertape_reader_options_set_busy_spin",
        "ReaderOptions::busy_spin",
    ),
    ("tickertape_retention_new", "Retention()"),
    ("tickertape_retention_free", "~Retention"),
    (
        "tickertape_retention_set_max_segments",
        "Retention::max_segments",
    ),
    ("tickertape_retention_set_max_bytes", "Retention::max_bytes"),
    ("tickertape_retention_set_max_age_ns", "Retention::max_age"),
    (
        "tickertape_retention_set_delete",
        "Retention::delete_segments",
    ),
    ("tickertape_retention_set_move_to", "Retention::move_to"),
    ("tickertape_reader_open", "Reader::open"),
    (
        "tickertape_reader_open_mapped_unsafe",
        "Reader::open_mapped_unsafe",
    ),
    ("tickertape_reader_close", "~Reader, Reader::close"),
    ("tickertape_reader_seek", "Reader::seek"),
    ("tickertape_reader_next", "Reader::next"),
    ("tickertape_reader_next_copy", "Reader::next_copy"),
    (
        "tickertape_reader_read_batch",
        "Reader::read_batch, RecordBatch",
    ),
    ("tickertape_reader_position", "Reader::position"),
    ("tickertape_last_error_message", "Error::message"),
    ("tickertape_last_error_os_code", "Error::os_code"),
    (
        "tickertape_last_error_corrupt_reason",
        "Error::corrupt_reason",
    ),
    ("tickertape_status_name", "status_name, Error::name"),
    ("tickertape_corrupt_reason_name", "corrupt_reason_name"),
    ("tickertape_writer_open", "Writer::open"),
    ("tickertape_writer_close", "~Writer, Writer::close"),
    ("tickertape_writer_append", "Writer::append"),
    ("tickertape_writer_append_with", "Writer::append_with"),
    (
        "tickertape_writer_append_batch",
        "Writer::append_batch, NewRecord",
    ),
    ("tickertape_writer_checkpoint", "Writer::checkpoint"),
    ("tickertape_writer_next_seq", "Writer::next_seq"),
    (
        "tickertape_writer_last_retention_error",
        "Writer::last_retention_error",
    ),
];

/// C functions the C++ binding deliberately does not call; ADR 0036 lists each with the same reason.
const CPP_NOT_WRAPPED: &[(&str, &str)] = &[(
    "tickertape_last_error_code",
    "every call returns its status, and Error takes the code from that return value",
)];

/// Fails when a C function is added (or removed) without a decision about the C++ binding (ADR 0036).
#[test]
fn cpp_binding_decides_about_every_c_function() {
    let functions = c_functions(&c_header());
    assert!(functions.len() > 40, "parsed only {functions:?}");
    // Without comments: a function named in a comment is not called.
    let cpp = strip_comments(&cpp_header());
    let adr = read_text(&repo_root().join("docs/decisions/0036-cpp-binding.md"));
    for name in &functions {
        let wrapped = CPP_WRAPPED.iter().any(|(c, _)| c == name);
        let not_wrapped = CPP_NOT_WRAPPED.iter().any(|(c, _)| c == name);
        assert!(
            wrapped != not_wrapped,
            "{name}: add it to CPP_WRAPPED (and wrap it in tickertape.hpp) or to CPP_NOT_WRAPPED (and ADR 0036)"
        );
        if wrapped {
            assert!(
                cpp.contains(&format!("{name}(")),
                "{name} is listed as wrapped but tickertape.hpp never calls it"
            );
        } else {
            assert!(
                !cpp.contains(&format!("{name}(")),
                "{name} is listed as not wrapped but tickertape.hpp calls it"
            );
            assert!(
                adr.contains(name.as_str()),
                "ADR 0036 must say why {name} is not wrapped"
            );
        }
    }
    for (name, _) in CPP_WRAPPED.iter().chain(CPP_NOT_WRAPPED) {
        assert!(
            functions.iter().any(|f| f == name),
            "{name} is listed but tickertape.h does not declare it"
        );
    }
}

/// What the C++ binding does with a field of a C struct.
enum FieldUse {
    /// The binding reads or writes it: `.field` must appear in tickertape.hpp's code.
    Used(&'static str),
    /// Deliberately not used, for the reason given.
    Unused(&'static str),
}

/// Every field of every struct `tickertape.h` defines, with the binding's decision. A field added to a C struct
/// fails `cpp_binding_decides_about_every_c_struct_field` until it is listed here (and wrapped, or not, on purpose).
const C_STRUCT_FIELDS: &[(&str, &[(&str, FieldUse)])] = {
    use FieldUse::{Unused, Used};
    &[
        (
            "tickertape_verify_report",
            &[
                ("struct_size", Used("set to the struct's size by verify()")),
                ("segments", Used("VerifyReport::segments")),
                ("records", Used("VerifyReport::records")),
                (
                    "first_seq",
                    Used("VerifyReport::first_seq, when records > 0"),
                ),
                ("last_seq", Used("VerifyReport::last_seq, when records > 0")),
                ("problems", Used("VerifyReport::problems")),
                (
                    "first_problem_base_seq",
                    Used("VerifyReport::Problem::segment_base_seq"),
                ),
                (
                    "first_problem_offset",
                    Used("VerifyReport::Problem::offset"),
                ),
                (
                    "first_problem_reason",
                    Used("VerifyReport::Problem::reason"),
                ),
                ("reserved", Unused("always 0")),
            ],
        ),
        (
            "tickertape_segment_info",
            &[
                ("base_seq", Used("SegmentInfo::base_seq")),
                ("segment_size", Used("SegmentInfo::segment_size")),
                ("created_unix_ns", Used("SegmentInfo::created_unix_ns")),
                ("format_version", Used("SegmentInfo::format_version")),
                ("reserved", Unused("always 0")),
            ],
        ),
        (
            "tickertape_checkpoint",
            &[
                ("generation", Used("Checkpoint::generation")),
                ("next_seq", Used("Checkpoint::next_seq")),
                ("segment_base_seq", Used("Checkpoint::segment_base_seq")),
                ("offset", Used("Checkpoint::offset")),
            ],
        ),
        (
            "tickertape_retention_report",
            &[
                (
                    "struct_size",
                    Used("set to the struct's size by detail::retention"),
                ),
                (
                    "removed_segments",
                    Used("RetentionReport::removed_segments"),
                ),
                ("removed_bytes", Used("RetentionReport::removed_bytes")),
                (
                    "already_removed_segments",
                    Used("RetentionReport::already_removed_segments"),
                ),
            ],
        ),
        (
            "tickertape_record",
            &[
                ("seq", Used("RecordView::seq")),
                ("venue_ts_ns", Used("RecordView::venue_ts_ns")),
                ("local_ts_ns", Used("RecordView::local_ts_ns")),
                ("payload", Used("RecordView::data, as_string_view")),
                ("payload_len", Used("RecordView::size, Error::needed_size")),
                ("schema_id", Used("RecordView::schema_id")),
                ("reserved", Unused("always 0")),
            ],
        ),
        (
            "tickertape_new_record",
            &[
                ("payload", Used("NewRecord's constructor and data()")),
                ("payload_len", Used("NewRecord's constructor and size()")),
                (
                    "venue_ts_ns",
                    Used("NewRecord's constructor and venue_ts_ns()"),
                ),
                (
                    "local_ts_ns",
                    Used("NewRecord's constructor and local_ts_ns()"),
                ),
                ("schema_id", Used("NewRecord's constructor and schema_id()")),
                ("reserved", Unused("set to 0 by NewRecord's constructor")),
            ],
        ),
    ]
};

/// The fields of each struct that `tickertape.h` defines (`typedef struct name { ... } name;`), in order.
fn c_struct_fields(header: &str) -> Vec<(String, Vec<String>)> {
    let code = strip_comments(header);
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut structs = Vec::new();
    let mut rest = code.as_str();
    while let Some(at) = rest.find("typedef struct ") {
        let after = &rest[at + "typedef struct ".len()..];
        let name: String = after.chars().take_while(|&c| is_ident(c)).collect();
        let body = after[name.len()..].trim_start();
        rest = after;
        let Some(body) = body.strip_prefix('{') else {
            continue; // `typedef struct name name;`: an opaque handle
        };
        let body = &body[..body.find('}').unwrap()];
        let fields = body
            .split(';')
            .map(str::trim)
            .filter(|declaration| !declaration.is_empty())
            .map(|declaration| {
                declaration
                    .rsplit(|c: char| !is_ident(c))
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        structs.push((name, fields));
    }
    structs
}

/// Fails when a field is added to (or removed from) a C struct without a decision about the C++ binding.
#[test]
fn cpp_binding_decides_about_every_c_struct_field() {
    let structs = c_struct_fields(&c_header());
    assert!(structs.len() >= 6, "parsed only {structs:?}");
    let cpp = strip_comments(&cpp_header());
    for (name, fields) in &structs {
        let decided = C_STRUCT_FIELDS
            .iter()
            .find(|(s, _)| s == name)
            .unwrap_or_else(|| panic!("{name}: add its fields to C_STRUCT_FIELDS"))
            .1;
        let listed: Vec<&str> = decided.iter().map(|(field, _)| *field).collect();
        assert_eq!(
            fields, &listed,
            "{name}: the fields in tickertape.h and in C_STRUCT_FIELDS differ; decide what the C++ binding does \
             with each new field"
        );
        for (field, use_) in decided.iter() {
            match use_ {
                FieldUse::Used(how) => {
                    let member = format!(".{field}");
                    let used = cpp.match_indices(&member).any(|(at, _)| {
                        !cpp[at + member.len()..]
                            .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
                    });
                    assert!(
                        used,
                        "{name}.{field} is listed as used ({how}) but tickertape.hpp never uses it"
                    );
                }
                FieldUse::Unused(why) => {
                    assert!(!why.is_empty(), "{name}.{field}: say why it is unused")
                }
            }
        }
    }
    for (name, _) in C_STRUCT_FIELDS {
        assert!(
            structs.iter().any(|(s, _)| s == name),
            "{name} is listed but tickertape.h does not define it"
        );
    }
}

/// The enumerators of the C enum or C++ enum class that starts at `start`: the `TICKERTAPE_*` name on each line
/// up to the closing brace (left of `=` in C, right of it in C++).
fn enumerators(text: &str, start: &str) -> Vec<String> {
    let body = &text[text.find(start).unwrap_or_else(|| panic!("no {start}"))..];
    let body = &body[..body.find("};").unwrap()];
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            let at = line.find("TICKERTAPE_")?;
            let name: String = line[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            (line.contains('=') && !line.starts_with("//") && !line.starts_with('*'))
                .then_some(name)
        })
        .collect()
}

/// Every status code, corruption reason and recovery policy of the C ABI has a C++ enumerator.
#[test]
fn cpp_binding_maps_every_status_and_reason() {
    let c = c_header();
    let cpp = cpp_header();
    for (c_enum, cpp_enum, min) in [
        ("enum tickertape_status_code", "enum class Status", 20),
        (
            "enum tickertape_corrupt_reason",
            "enum class CorruptReason",
            15,
        ),
        (
            "enum tickertape_recovery_policy",
            "enum class RecoveryPolicy",
            2,
        ),
    ] {
        let mut from_c = enumerators(&c, c_enum);
        let mut from_cpp = enumerators(&cpp, cpp_enum);
        assert!(from_c.len() >= min, "{c_enum}: {from_c:?}");
        from_c.sort();
        from_cpp.sort();
        assert_eq!(
            from_c, from_cpp,
            "{cpp_enum} in tickertape.hpp must name every value of {c_enum}"
        );
    }
}

fn define(text: &str, name: &str) -> u32 {
    let prefix = format!("#define {name} ");
    let line = text
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("no {name}"));
    line[prefix.len()..].trim().parse().unwrap()
}

/// The C++ header carries the version of the release it belongs to (ADR 0036 -> Versioning).
#[test]
fn cpp_header_version_matches_the_crate() {
    let cpp = cpp_header();
    let number = |s: &str| s.parse::<u32>().unwrap();
    assert_eq!(
        define(&cpp, "TICKERTAPE_HPP_VERSION_MAJOR"),
        number(env!("CARGO_PKG_VERSION_MAJOR"))
    );
    assert_eq!(
        define(&cpp, "TICKERTAPE_HPP_VERSION_MINOR"),
        number(env!("CARGO_PKG_VERSION_MINOR"))
    );
    assert_eq!(
        define(&cpp, "TICKERTAPE_HPP_VERSION_PATCH"),
        number(env!("CARGO_PKG_VERSION_PATCH"))
    );
    assert_eq!(
        define(&cpp, "TICKERTAPE_HPP_ABI_VERSION"),
        define(&c_header(), "TICKERTAPE_ABI_VERSION")
    );
}

/// The C++ compilers for the language-level matrix: the one the `cc` crate picks, plus `g++` and `clang++` if they
/// are on the `PATH` and are different compilers (by `--version`). `TICKERTAPE_FFI_CXX_COMPILERS` (space-separated)
/// replaces the list.
fn cpp_compilers() -> Vec<PathBuf> {
    if let Ok(list) = std::env::var("TICKERTAPE_FFI_CXX_COMPILERS") {
        return list.split_whitespace().map(PathBuf::from).collect();
    }
    let mut candidates = vec![compiler(true).path().to_path_buf()];
    candidates.extend([PathBuf::from("g++"), PathBuf::from("clang++")]);
    let mut seen = Vec::new();
    let mut compilers = Vec::new();
    for candidate in candidates {
        let Ok(output) = Command::new(&candidate).arg("--version").output() else {
            continue;
        };
        let version = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned();
        if output.status.success() && !seen.contains(&version) {
            eprintln!(
                "C++ compiler for the matrix: {} ({version})",
                candidate.display()
            );
            seen.push(version);
            compilers.push(candidate);
        }
    }
    compilers
}

/// The header compiles without warnings as C++17, C++20 and C++23, with and without exceptions, with every C++
/// compiler found (GCC and Clang on Linux CI), under -Wall -Wextra -Wpedantic -Wconversion -Wshadow.
#[test]
fn cpp_header_compiles_at_every_language_level() {
    let scratch = tempfile::tempdir().unwrap();
    let source = manifest_dir().join("tests/cpp/compile_all.cpp");
    let compilers = cpp_compilers();
    assert!(!compilers.is_empty(), "no C++ compiler found");
    for (i, compiler) in compilers.iter().enumerate() {
        for std in ["c++17", "c++20", "c++23"] {
            for exceptions in ["-fexceptions", "-fno-exceptions"] {
                let mut cmd = Command::new(compiler);
                cmd.arg(format!("-std={std}"))
                    .arg(exceptions)
                    .args(["-O2", "-Wall", "-Wextra", "-Werror"])
                    .args(CPP_BINDING_WARNINGS)
                    .arg("-I")
                    .arg(manifest_dir().join("include"))
                    .arg("-c")
                    .arg(&source)
                    .arg("-o")
                    .arg(
                        scratch
                            .path()
                            .join(format!("compile_all_{i}_{std}{exceptions}.o")),
                    );
                let output = cmd.output().expect("run the C++ compiler");
                assert!(
                    output.status.success(),
                    "{cmd:?} failed\n{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}
