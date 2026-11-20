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
        run_library_build(library_build("ffi-c-api"), "ffi-c-api")
    })
}

/// The directory with a `libtickertape.a` built with `panic = "abort"`, as [`lib_dir`] builds its libraries but into
/// `<target-dir>/ffi-c-api-panic-abort`, with the private compiler cfg `tickertape_test_panic_abort` (which lets the
/// crate build that way; `src/lib.rs`) added to that build's rustflags only. Nothing in such a library can unwind: a
/// panic, or a `resume_unwind`, ends the process.
fn panic_abort_lib_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let profile = cargo_profile();
        let mut cmd = library_build("ffi-c-api-panic-abort");
        cmd.args(["--config", &format!("profile.{profile}.panic=\"abort\"")]);
        // Cargo takes the rustflags from the first of CARGO_ENCODED_RUSTFLAGS and RUSTFLAGS that is set (and ignores
        // `build.rustflags` then), so the cfg is added to whichever the outer environment uses.
        let mut flags: Vec<String> = match std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            Ok(encoded) => encoded
                .split('\x1f')
                .filter(|f| !f.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(_) => std::env::var("RUSTFLAGS")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        };
        flags.extend(["--cfg".to_owned(), "tickertape_test_panic_abort".to_owned()]);
        cmd.env_remove("RUSTFLAGS")
            .env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"));
        run_library_build(cmd, "ffi-c-api-panic-abort")
    })
}

/// The Cargo profile of the outer build (`dev` for the `debug` directory).
fn cargo_profile() -> &'static str {
    match outer_layout().profile_dir.as_str() {
        "debug" => "dev",
        name => name,
    }
}

/// The command `cargo build -p tickertape-ffi --locked` with the outer build's profile and target into
/// `<target-dir>/<name>`, for [`run_library_build`].
fn library_build(name: &str) -> Command {
    let layout = outer_layout();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args([
        "build",
        "-p",
        "tickertape-ffi",
        "--locked",
        "--profile",
        cargo_profile(),
    ])
    .arg("--manifest-path")
    .arg(repo_root().join("Cargo.toml"))
    .arg("--target-dir")
    .arg(layout.target_dir.join(name));
    if let Some(triple) = &layout.triple {
        cmd.args(["--target", triple]);
    }
    cmd
}

/// Runs a [`library_build`] command for `<target-dir>/<name>` and returns the directory with the libraries.
fn run_library_build(mut cmd: Command, name: &str) -> PathBuf {
    let layout = outer_layout();
    let mut dir = layout.target_dir.join(name);
    if let Some(triple) = &layout.triple {
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

/// The sanitizer flags for compiling and linking under `TICKERTAPE_FFI_SANITIZE=1`.
fn sanitizer_flags() -> &'static [&'static str] {
    if env_flag("TICKERTAPE_FFI_SANITIZE") {
        &[
            "-fsanitize=address,undefined",
            "-fno-sanitize-recover=all",
            "-fno-omit-frame-pointer",
        ]
    } else {
        &[]
    }
}

/// A compiler command for language standard `std` (`c99`, `c++17`, ...) with warnings as errors, the sanitizers if
/// requested, the include directory and the extra `flags`.
fn compiler_command(std: &str, flags: &[&str]) -> Command {
    let mut cmd = compiler(std.starts_with("c++")).to_command();
    cmd.arg(format!("-std={std}"));
    cmd.args(["-Wall", "-Wextra", "-Werror", "-pedantic"]);
    cmd.args(flags);
    cmd.args(sanitizer_flags());
    cmd.arg("-I").arg(manifest_dir().join("include"));
    cmd
}

/// Adds the arguments that link the tickertape library as `link` says.
fn link_library(cmd: &mut Command, link: Link) {
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
}

/// Runs a compiler (or linker) command and fails the test with its output if it fails.
fn run_compiler(cmd: &mut Command) {
    let output = cmd.output().expect("run the compiler");
    assert!(
        output.status.success(),
        "{cmd:?} failed\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Compiles `sources` as language standard `std` with the extra `flags` into the executable `out`.
fn compile(sources: &[PathBuf], std: &str, link: Link, flags: &[&str], out: &Path) {
    let mut cmd = compiler_command(std, flags);
    cmd.args(sources);
    link_library(&mut cmd, link);
    cmd.arg("-o").arg(out);
    run_compiler(&mut cmd);
}

/// Runs `commands` a few at a time (as many as there are CPUs) and returns their outputs in order.
fn run_parallel(commands: Vec<Command>) -> Vec<(Command, Output)> {
    let parallel = std::thread::available_parallelism().map_or(4, |n| n.get());
    let queue = Mutex::new(commands.into_iter().enumerate());
    let done = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..parallel {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().unwrap().next();
                    let Some((index, mut cmd)) = next else { break };
                    let output = cmd.output().expect("run the compiler");
                    done.lock().unwrap().push((index, cmd, output));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(index, _, _)| *index);
    done.into_iter()
        .map(|(_, cmd, output)| (cmd, output))
        .collect()
}

/// Runs `commands` a few at a time and fails with the output of every one that failed.
fn run_all_ok(commands: Vec<Command>) {
    let failures: Vec<String> = run_parallel(commands)
        .into_iter()
        .filter(|(_, output)| !output.status.success())
        .map(|(cmd, output)| {
            format!(
                "{cmd:?} failed\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
    command_with_valgrind_args(program, &[])
}

/// Valgrind's option for programs that replace `operator new` themselves (the allocation-counting and
/// allocation-failure tests): by default Valgrind also replaces the program's own `operator new` with its
/// allocator, so the program's counter never moves and its injected failures never happen. With this option the
/// program's operators are used, and Valgrind still checks the memory they get from `malloc`.
const VALGRIND_USER_ALLOCATOR: &[&str] = &["--soname-synonyms=somalloc=nouserintercepts"];

/// [`command`], with `valgrind_args` added when it runs under Valgrind.
fn command_with_valgrind_args(program: &Path, valgrind_args: &[&str]) -> Command {
    let mut cmd = if env_flag("TICKERTAPE_FFI_VALGRIND") {
        let mut c = Command::new("valgrind");
        c.args([
            "--quiet",
            "--error-exitcode=99",
            "--leak-check=full",
            "--errors-for-leak-kinds=definite",
        ]);
        c.args(valgrind_args);
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

/// An aborted fill (a callback that returns non-zero) needs no unwinding inside the library: the whole basic C test,
/// including its aborted fill (`TICKERTAPE_E_ABORTED`), passes against a static library built with
/// `panic = "abort"`, in which any unwind ends the process. Before `tickertape_writer_append_with` used the core's
/// `try_append_with`, the library aborted that append by unwinding, and this program ended at the aborted fill.
#[test]
fn aborted_fill_needs_no_unwinding() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("test_api_panic_abort");
    let mut cmd = compiler_command("c99", &[]);
    cmd.arg(manifest_dir().join("tests/c/test_api.c"));
    cmd.arg(panic_abort_lib_dir().join("libtickertape.a"));
    cmd.args(static_libs());
    cmd.arg("-o").arg(&program);
    run_compiler(&mut cmd);
    let scratch = tempfile::tempdir().unwrap();
    run_ok(command(&program).arg("basic").arg(scratch.path()));
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

/// A throwing fill followed by a failed clean-up, every status and corruption reason, a mismatched library (also
/// for the option builders), and an allocation failure after a clean abort, against stubbed C functions.
#[test]
fn cpp_binding_against_stubs() {
    run_ok(&mut command_with_valgrind_args(
        &programs().binding_stub,
        VALGRIND_USER_ALLOCATOR,
    ));
}

/// The binding allocates nothing on the append and read paths (it counts `operator new`, after a positive control
/// that a deliberate allocation is counted, also under Valgrind).
#[test]
fn cpp_binding_allocates_nothing_on_the_hot_path() {
    let scratch = tempfile::tempdir().unwrap();
    run_ok(
        command_with_valgrind_args(&programs().no_alloc, VALGRIND_USER_ALLOCATOR)
            .arg(scratch.path()),
    );
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

/// C++20: `as_span` of a stale view whose record is empty is caught too (it used to return before the check).
#[test]
fn cpp_binding_detects_a_stale_empty_view_through_as_span() {
    use std::os::unix::process::ExitStatusExt;
    let scratch = tempfile::tempdir().unwrap();
    let output = command(&programs().binding_cpp20)
        .arg(scratch.path())
        .arg("stale-empty-span")
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

/// Whether `code` contains `word` as a whole identifier.
fn contains_word(code: &str, word: &str) -> bool {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    code.match_indices(word).any(|(at, _)| {
        !code[..at].ends_with(is_ident) && !code[at + word.len()..].starts_with(is_ident)
    })
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
                ("payload", Used("new_record(); NewRecord is the C struct")),
                ("payload_len", Used("new_record()")),
                ("venue_ts_ns", Used("new_record()")),
                ("local_ts_ns", Used("new_record()")),
                ("schema_id", Used("new_record()")),
                (
                    "reserved",
                    Unused("set to 0 by new_record()'s value-initialisation"),
                ),
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

/// The header compiles without warnings as C++17, C++20 and C++23, with and without exceptions, with and without
/// `NDEBUG` (which turns the debug checks off), with every C++ compiler found (GCC and Clang on Linux CI), under
/// -Wall -Wextra -Wpedantic -Wconversion -Wshadow -Werror. `layout.cpp` pins every public class's size and
/// alignment in each of these configurations.
#[test]
fn cpp_header_compiles_at_every_language_level() {
    let scratch = tempfile::tempdir().unwrap();
    let tests = manifest_dir().join("tests/cpp");
    let sources = [tests.join("compile_all.cpp"), tests.join("layout.cpp")];
    let compilers = cpp_compilers();
    assert!(!compilers.is_empty(), "no C++ compiler found");
    let mut jobs = Vec::new();
    for (i, compiler) in compilers.iter().enumerate() {
        for std in ["c++17", "c++20", "c++23"] {
            for exceptions in ["-fexceptions", "-fno-exceptions"] {
                for debug in ["-UNDEBUG", "-DNDEBUG"] {
                    for source in &sources {
                        let stem = source.file_stem().unwrap().to_string_lossy();
                        let mut cmd = Command::new(compiler);
                        cmd.arg(format!("-std={std}"))
                            .args([exceptions, debug])
                            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
                            .args(CPP_BINDING_WARNINGS)
                            .arg("-I")
                            .arg(manifest_dir().join("include"))
                            .arg("-c")
                            .arg(source)
                            .arg("-o")
                            .arg(
                                scratch
                                    .path()
                                    .join(format!("{stem}_{i}_{std}{exceptions}{debug}.o")),
                            );
                        jobs.push(cmd);
                    }
                }
            }
        }
    }
    run_all_ok(jobs);
}

/// The lines of `tickertape.hpp` after preprocessing, as (line number in the header, text), when `compiler`
/// preprocesses a file that includes it with `flags`; blank lines are left out.
fn preprocessed_header_lines(
    compiler: &Path,
    flags: &[&str],
    scratch: &Path,
) -> Vec<(u32, String)> {
    let source = scratch.join("include_tickertape_hpp.cpp");
    std::fs::write(&source, "#include \"tickertape.hpp\"\n").unwrap();
    let mut cmd = Command::new(compiler);
    cmd.arg("-E")
        .args(flags)
        .arg("-I")
        .arg(manifest_dir().join("include"))
        .arg(&source);
    let output = cmd.output().expect("run the preprocessor");
    assert!(
        output.status.success(),
        "{cmd:?} failed\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut lines = Vec::new();
    let mut in_header = false;
    let mut line_no = 0u32;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // A line marker: `# <line> "<file>" <flags>` (GCC, Clang) or `#line <line> "<file>"`.
        let marker = line
            .strip_prefix("# ")
            .or_else(|| line.strip_prefix("#line "))
            .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        if let Some(rest) = marker {
            let mut parts = rest.splitn(2, ' ');
            line_no = parts.next().unwrap().parse().unwrap();
            let file = parts.next().unwrap_or_default();
            in_header = file
                .split('"')
                .nth(1)
                .is_some_and(|f| f.ends_with("/tickertape.hpp"));
            continue;
        }
        if in_header && !line.trim().is_empty() {
            lines.push((line_no, line.trim().to_owned()));
        }
        line_no += 1;
    }
    assert!(
        lines.len() > 1000,
        "found only {} lines of tickertape.hpp in {cmd:?}",
        lines.len()
    );
    lines
}

/// Whether the brackets in `text` balance (a crude test that it holds whole declarations).
fn brackets_balance(text: &str) -> bool {
    let mut stack = Vec::new();
    for c in text.chars() {
        match c {
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(open) {
                    return false;
                }
            }
            _ => {}
        }
    }
    stack.is_empty()
}

/// Whether every scope still open at the end of `text` (preprocessed C++) is a namespace or an `extern "C"` block,
/// so that a declaration placed there is a declaration of its own and not a member added to a class or a statement
/// added to a function. String and character literals are skipped; a `'` after a letter or digit is a digit
/// separator.
fn at_namespace_scope(text: &str) -> bool {
    let mut open: Vec<bool> = Vec::new();
    // The tokens since the last `;`, `{` or `}`: what the next `{` belongs to.
    let mut head = String::new();
    let mut previous = ' ';
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' if c == '"' || !previous.is_ascii_alphanumeric() => {
                head.push(c);
                while let Some(d) = chars.next() {
                    if d == '\\' {
                        chars.next();
                    } else if d == c {
                        break;
                    } else {
                        head.push(d);
                    }
                }
                head.push(c);
            }
            '{' => {
                let h = head.trim_start();
                // `extern "C" {` is a linkage block; `extern "C" int f() {` is a function body. A namespace head may
                // go on with a name or with attributes (`namespace [[deprecated]] n {`). The check is for this
                // header: a spelling it does not recognise is reported, which is the safe direction.
                let namespace = h
                    .strip_prefix("inline ")
                    .unwrap_or(h)
                    .trim_start()
                    .strip_prefix("namespace")
                    .is_some_and(|rest| {
                        !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                    });
                open.push(namespace || h.trim_end() == "extern \"C\"");
                head.clear();
            }
            '}' => {
                open.pop();
                head.clear();
            }
            ';' => head.clear(),
            _ => head.push(c),
        }
        previous = c;
    }
    open.iter().all(|namespace| *namespace)
}

/// Compares the preprocessed header at a lower and a higher language level: every line of the lower one must be in
/// the higher one unchanged, and the lines only the higher one has must come in runs of whole declarations (balanced
/// brackets, ending in `;` or `}`, after a line that ends a declaration or opens a scope) at namespace scope (a
/// member added to a class changes that class's definition).
fn standard_dependent_definitions(
    lower: &[(u32, String)],
    higher: &[(u32, String)],
) -> Vec<String> {
    let mut lower_lines: std::collections::BTreeMap<u32, String> = Default::default();
    for (n, text) in lower {
        lower_lines
            .entry(*n)
            .and_modify(|t| {
                t.push(' ');
                t.push_str(text);
            })
            .or_insert_with(|| text.clone());
    }
    let mut higher_lines: Vec<(u32, String)> = Vec::new();
    for (n, text) in higher {
        match higher_lines.last_mut() {
            Some((last, t)) if last == n => {
                t.push(' ');
                t.push_str(text);
            }
            _ => higher_lines.push((*n, text.clone())),
        }
    }
    let mut problems = Vec::new();
    for (n, text) in &lower_lines {
        match higher_lines.iter().find(|(m, _)| m == n) {
            Some((_, t)) if t == text => {}
            Some((_, t)) => problems.push(format!("line {n} changes: `{text}` becomes `{t}`")),
            None => problems.push(format!("line {n} disappears: `{text}`")),
        }
    }
    let mut i = 0;
    while i < higher_lines.len() {
        if lower_lines.contains_key(&higher_lines[i].0) {
            i += 1;
            continue;
        }
        let start = i;
        while i < higher_lines.len() && !lower_lines.contains_key(&higher_lines[i].0) {
            i += 1;
        }
        let added: Vec<&str> = higher_lines[start..i]
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        let added = added.join(" ");
        let after_a_declaration =
            start == 0 || higher_lines[start - 1].1.ends_with([';', '{', '}', ':']);
        if !(brackets_balance(&added) && added.ends_with([';', '}']) && after_a_declaration) {
            problems.push(format!(
                "lines {}..={} are added inside a declaration, not as whole declarations: `{added}`",
                higher_lines[start].0,
                higher_lines[i - 1].0
            ));
            continue;
        }
        let before: Vec<&str> = higher_lines[..start]
            .iter()
            .map(|(_, t)| t.as_str())
            .collect();
        if !at_namespace_scope(&before.join(" ")) {
            problems.push(format!(
                "lines {}..={} are added inside a class or a function, not at namespace scope: `{added}`",
                higher_lines[start].0,
                higher_lines[i - 1].0
            ));
        }
    }
    problems
}

/// The comparison itself: what it must accept and what it must report.
#[test]
fn language_level_comparison_reports_changed_definitions() {
    let lines = |text: &[(u32, &str)]| -> Vec<(u32, String)> {
        text.iter().map(|(n, t)| (*n, (*t).to_owned())).collect()
    };
    let lower = lines(&[
        (1, "extern \"C\" {"),
        (2, "struct c_struct { int a; };"),
        (3, "}"),
        (4, "namespace tickertape { inline namespace v1 {"),
        (5, "class Error {"),
        (6, "const char *open() const { return \"{\"; }"),
        (7, "char brace() const { return '{'; }"),
        (8, "long big() const { return 1'000'000; }"),
        (10, "};"),
        (11, "inline bool ok(int n) {"),
        (12, "return n > 0;"),
        (14, "}"),
        (17, "} }"),
    ]);
    assert_eq!(
        standard_dependent_definitions(&lower, &lower),
        Vec::<String>::new()
    );
    // Whole declarations added at namespace scope: accepted.
    let mut added = lower.clone();
    added.insert(12, (15, "template <class T>".to_owned()));
    added.insert(
        13,
        (16, "inline bool spanned(T t) { return ok(t); }".to_owned()),
    );
    assert_eq!(
        standard_dependent_definitions(&lower, &added),
        Vec::<String>::new()
    );
    // A member added to a class: a whole declaration, but it changes the class.
    let mut member = lower.clone();
    member.insert(8, (9, "void only_cxx20() {}".to_owned()));
    let problems = standard_dependent_definitions(&lower, &member);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("not at namespace scope"),
        "{problems:?}"
    );
    // A statement added to a function.
    let mut statement = lower.clone();
    statement.insert(11, (13, "(void)n;".to_owned()));
    let problems = standard_dependent_definitions(&lower, &statement);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("not at namespace scope"),
        "{problems:?}"
    );
    // A declaration added inside a namespace whose head has attributes or no name.
    for head in [
        "namespace[[deprecated]] old {",
        "namespace {",
        "inline namespace v2 {",
    ] {
        let ns = lines(&[(1, head), (2, "int a;"), (4, "}")]);
        let mut more = ns.clone();
        more.insert(2, (3, "int b;".to_owned()));
        assert_eq!(
            standard_dependent_definitions(&ns, &more),
            Vec::<String>::new(),
            "{head}"
        );
    }
    // A name that merely starts with `namespace` is not a namespace head.
    let class = lines(&[(1, "struct namespace_like {"), (2, "int a;"), (4, "};")]);
    let mut more = class.clone();
    more.insert(2, (3, "int b;".to_owned()));
    assert_eq!(standard_dependent_definitions(&class, &more).len(), 1);
    // A statement added to a function with C linkage, which is not a linkage block.
    let c_function = lines(&[
        (1, "extern \"C\" inline int f(int n) {"),
        (3, "return n;"),
        (4, "}"),
    ]);
    let mut c_statement = c_function.clone();
    c_statement.insert(1, (2, "n = 0;".to_owned()));
    let problems = standard_dependent_definitions(&c_function, &c_statement);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].contains("not at namespace scope"),
        "{problems:?}"
    );
    // A changed line and a line that disappears.
    let mut changed = lower.clone();
    changed[10].1 = "return n >= 0;".to_owned();
    changed.remove(7);
    let problems = standard_dependent_definitions(&lower, &changed);
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(
        problems.iter().any(|p| p.contains("line 12 changes")),
        "{problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("line 8 disappears")),
        "{problems:?}"
    );
}

/// Nothing the header defines depends on the language level (ADR 0036 -> Language level): a block conditional on
/// the standard or on a library feature-test macro may only add whole declarations (`as_span`, the `char8_t` and
/// `std::span` specialisations). Otherwise a program linking a C++17 unit and a C++20 unit would contain two
/// different definitions of one inline function, class or variable (a one-definition-rule violation). Checked by
/// preprocessing the header as C++17, C++20 and C++23, with and without exceptions, with every C++ compiler found.
#[test]
fn cpp_header_definitions_do_not_depend_on_the_language_level() {
    let scratch = tempfile::tempdir().unwrap();
    let compilers = cpp_compilers();
    assert!(!compilers.is_empty(), "no C++ compiler found");
    let mut problems = Vec::new();
    for compiler in &compilers {
        for exceptions in ["-fexceptions", "-fno-exceptions"] {
            let at = |std: &str| {
                preprocessed_header_lines(
                    compiler,
                    &[&format!("-std={std}"), exceptions],
                    scratch.path(),
                )
            };
            let (cpp17, cpp20, cpp23) = (at("c++17"), at("c++20"), at("c++23"));
            for (lower, higher, what) in [
                (&cpp17, &cpp20, "C++17 -> C++20"),
                (&cpp20, &cpp23, "C++20 -> C++23"),
            ] {
                for problem in standard_dependent_definitions(lower, higher) {
                    problems.push(format!(
                        "{} {exceptions} {what}: {problem}",
                        compiler.display()
                    ));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "tickertape.hpp has definitions that differ between language levels:\n{}",
        problems.join("\n")
    );
}

/// Every public class and enumeration of the binding has its size and alignment pinned in `tests/cpp/layout.cpp`
/// (ADR 0036 -> Versioning: no data member is added within a major version).
#[test]
fn cpp_layout_test_pins_every_public_class() {
    // Classes that are not part of the public surface: the `detail` namespace and Writer's private frame type.
    const NOT_PUBLIC: &[&str] = &[
        "is_byte",
        "byte_range",
        "writable_byte_range",
        "Array",
        "PathArg",
        "ResultAccess",
        "FillFrame",
        "CloseOnExit",
        "FillContext",
    ];
    let header = strip_comments(&cpp_header());
    let layout = read_text(&manifest_dir().join("tests/cpp/layout.cpp"));
    let pinned: Vec<&str> = layout
        .lines()
        .filter_map(|line| line.strip_prefix("TT_PIN(")?.split(',').next())
        .collect();
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut classes = Vec::new();
    for line in header.lines() {
        let line = line.trim_start();
        let Some(rest) = ["enum class ", "class ", "struct "]
            .iter()
            .find_map(|kind| line.strip_prefix(kind))
        else {
            continue;
        };
        let rest = rest.trim_start_matches("[[nodiscard]]").trim_start();
        let name: String = rest.chars().take_while(|&c| is_ident(c)).collect();
        let after = rest[name.len()..].trim_start();
        // A definition, not a forward declaration or an explicit specialisation of a detail trait.
        if name.is_empty()
            || after.starts_with(';')
            || after.starts_with('<')
            || NOT_PUBLIC.contains(&name.as_str())
        {
            continue;
        }
        classes.push(name);
    }
    assert!(classes.len() >= 20, "parsed only {classes:?}");
    for name in &classes {
        let is_pinned = pinned.iter().any(|ty| {
            ty.split("::")
                .any(|part| part == name || part.starts_with(&format!("{name}<")))
        });
        assert!(
            is_pinned,
            "{name} is a public class of tickertape.hpp without a TT_PIN line in tests/cpp/layout.cpp"
        );
    }
}

// ---- Mixing exception and debug-check modes in one program (ADR 0036 -> Mixing modes) ----

/// The attribute that gives a function a symbol per debug-check mode. (The exception mode is in the inline
/// namespace's name, which covers every declaration: `tickertape::v1` or `tickertape::v1_noexcept`.)
const MODE_TAG: &str = "TICKERTAPE_HPP_MODE_DEPENDENT";

/// The macros whose value can make an inline function's behaviour depend on the debug-check mode.
const MODE_MACROS: &[&str] = &["TICKERTAPE_DEBUG_CHECKS", "NDEBUG"];

/// Calls from functions without the tag to a function that has the name of a tagged one but is a different
/// function: (caller, callee, what the callee really is).
const NOT_MODE_DEPENDENT_CALLS: &[(&str, &str, &str)] = &[(
    "check_options",
    "error",
    "the option builders' error(), which returns the recorded std::optional<Error>",
)];

/// Tagged functions that call a tagged function only through an operator, which the source check cannot see:
/// (name, what it calls).
const TAGGED_THROUGH_OPERATORS: &[(&str, &str)] = &[(
    "operator*",
    "RecordBatch::iterator's operator* calls RecordBatch::operator[]",
)];

/// A function defined in C++ source without comments: everything before its body (attributes, return type, name,
/// parameters, qualifiers, member initialisers), its name, and its body.
struct CppFunction {
    head: String,
    name: String,
    body: String,
}

/// The function definitions in `code` (from `strip_comments`), including member functions defined in their class.
/// A crude parser for this header's style, not for C++ in general: a `{` opens a function body when the declaration
/// before it has a parameter list and is not a class, struct, union, enum or namespace.
fn cpp_functions(code: &str) -> Vec<CppFunction> {
    let chars: Vec<char> = code.chars().collect();
    let mut functions = Vec::new();
    let mut head_start = 0;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            ';' | '}' => head_start = i + 1,
            '{' => {
                let head = declaration_head(&chars[head_start..i].iter().collect::<String>());
                let close = matching_brace(&chars, i);
                if member_brace_initializer(&head) {
                    i = close; // `member_{...}` in a constructor's initialiser list: the declaration continues
                } else if let Some(name) = function_name(&head) {
                    let body = chars[i + 1..close].iter().collect();
                    functions.push(CppFunction { head, name, body });
                    i = close;
                    head_start = close + 1;
                } else {
                    head_start = i + 1; // a class, namespace, enum or braced initialiser: look inside
                }
            }
            _ => {}
        }
        i += 1;
    }
    functions
}

/// The index of the `}` that closes the `{` at `open`.
fn matching_brace(chars: &[char], open: usize) -> usize {
    let mut depth = 0;
    for (k, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return k;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in tickertape.hpp");
}

/// A declaration's text without preprocessor directives (and their continuation lines) and access specifiers, on
/// one line.
fn declaration_head(text: &str) -> String {
    let mut kept = Vec::new();
    let mut continued = false;
    for line in text.lines() {
        let directive = continued || line.trim_start().starts_with('#');
        continued = directive && line.trim_end().ends_with('\\');
        if !directive {
            kept.push(line.trim());
        }
    }
    let mut head = kept.join(" ").trim().to_owned();
    while let Some(rest) = ["public:", "private:", "protected:"]
        .iter()
        .find_map(|access| head.strip_prefix(access))
    {
        head = rest.trim().to_owned();
    }
    head
}

/// `head` without leading `template <...>` clauses and `[[...]]` attributes.
fn strip_template_and_attributes(head: &str) -> &str {
    let mut rest = head.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("template") {
            let after = after.trim_start();
            if after.starts_with('<') {
                let mut depth = 0;
                let mut end = after.len();
                for (k, c) in after.char_indices() {
                    match c {
                        '<' => depth += 1,
                        '>' => {
                            depth -= 1;
                            if depth == 0 {
                                end = k + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                rest = after[end..].trim_start();
                continue;
            }
        }
        if rest.starts_with("[[") {
            if let Some(end) = rest.find("]]") {
                rest = rest[end + 2..].trim_start();
                continue;
            }
        }
        return rest;
    }
}

/// Whether a constructor's member initialiser list (`) : member_(...)` or `) noexcept : ...`) is in `head`.
fn has_member_initializers(head: &str) -> bool {
    let bytes = head.as_bytes();
    (0..bytes.len()).any(|k| {
        bytes[k] == b':' && bytes.get(k + 1) != Some(&b':') && (k == 0 || bytes[k - 1] != b':') && {
            let before = head[..k].trim_end();
            before.ends_with(')') || before.ends_with("noexcept")
        }
    })
}

/// Whether the `{` after `head` starts a braced member initialiser (`: rec_{...}`) rather than the body.
fn member_brace_initializer(head: &str) -> bool {
    let last = head.trim_end().chars().last();
    last.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '>')
        && has_member_initializers(head)
}

/// The name of the function `head` declares (`value`, `operator*`, `~Writer`), or None if it is not a function.
fn function_name(head: &str) -> Option<String> {
    let rest = strip_template_and_attributes(head);
    let first_word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if ["class", "struct", "union", "enum", "namespace"].contains(&first_word.as_str()) {
        return None;
    }
    let before = rest[..rest.find('(')?].trim_end();
    if before.ends_with("operator") {
        return Some("operator()".to_owned());
    }
    if let Some(at) = before.rfind("operator") {
        if !before[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
            return Some(
                before[at..]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
    let name = before
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '~'))
        .next()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Whether `body` calls a function named `name` (`name(` as a whole identifier, not `std::name(`).
fn calls(body: &str, name: &str) -> bool {
    let call = format!("{name}(");
    body.match_indices(&call).any(|(at, _)| {
        let before = &body[..at];
        !before.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && !before.ends_with("std::")
    })
}

/// Every inline function whose behaviour depends on the debug-check mode has a symbol per mode: it is declared
/// TICKERTAPE_HPP_MODE_DEPENDENT if its body mentions a debug-check macro, or calls a function that is (by name;
/// calls through operators, such as `*result`, are not seen, and the operators are tagged by hand). Without it, the
/// linker keeps one translation unit's copy for the whole program. Conversely, a tagged function must have a reason
/// (so that no exception-mode tag is left behind: the inline namespace carries that mode), and the whole binding is
/// inside the mode-specific inline namespace.
#[test]
fn cpp_mode_dependent_functions_have_a_symbol_per_mode() {
    let header = cpp_header();
    assert!(
        header.contains(&format!(
            "#define {MODE_TAG} \\\n    __attribute__((abi_tag(TICKERTAPE_HPP_DEBUG_CHECKS_TAG_)))"
        )),
        "{MODE_TAG} must expand to abi_tag with the debug-check tag only on GCC and Clang"
    );
    let code = strip_comments(&header);
    assert_eq!(
        code.matches("namespace tickertape").count(),
        1,
        "the binding opens namespace tickertape once"
    );
    let opened = code.find("namespace tickertape {").unwrap();
    assert!(
        code[opened..]
            .trim_start_matches("namespace tickertape {")
            .trim_start()
            .starts_with("inline namespace TICKERTAPE_HPP_INLINE_NAMESPACE_ {"),
        "everything in namespace tickertape must be inside the mode-specific inline namespace"
    );
    assert!(
        code.contains("#define TICKERTAPE_HPP_INLINE_NAMESPACE_ v1\n")
            && code.contains("#define TICKERTAPE_HPP_INLINE_NAMESPACE_ v1_noexcept\n"),
        "the inline namespace is v1 with exceptions and v1_noexcept without"
    );
    let functions = cpp_functions(&strip_comments(&header));
    assert!(
        functions.len() > 100,
        "parsed only {} functions",
        functions.len()
    );
    let mut tagged: Vec<&str> = functions
        .iter()
        .filter(|f| f.head.contains(MODE_TAG))
        .map(|f| f.name.as_str())
        .collect();
    tagged.sort_unstable();
    tagged.dedup();
    for expected in ["debug_check", "error", "check_live", "data", "operator[]"] {
        assert!(
            tagged.contains(&expected),
            "{expected} is not tagged; parsed {tagged:?}"
        );
    }
    let mut problems = Vec::new();
    for f in functions.iter().filter(|f| f.head.contains(MODE_TAG)) {
        let mentions_macro = MODE_MACROS
            .iter()
            .any(|m| contains_word(&f.head, m) || contains_word(&f.body, m));
        let calls_tagged = tagged
            .iter()
            .filter(|name| !name.starts_with("operator"))
            .any(|callee| calls(&f.body, callee));
        let by_hand = TAGGED_THROUGH_OPERATORS
            .iter()
            .any(|(name, _)| *name == f.name);
        if !mentions_macro && !calls_tagged && !by_hand {
            problems.push(format!(
                "{}: is {MODE_TAG} but does not depend on the debug-check mode (the exception mode is the inline \
                 namespace's job)",
                f.name
            ));
        }
    }
    for f in functions.iter().filter(|f| !f.head.contains(MODE_TAG)) {
        for macro_name in MODE_MACROS {
            if contains_word(&f.head, macro_name) || contains_word(&f.body, macro_name) {
                problems.push(format!(
                    "{}: mentions {macro_name} but is not {MODE_TAG}",
                    f.name
                ));
            }
        }
        for callee in tagged.iter().filter(|name| !name.starts_with("operator")) {
            let allowed = NOT_MODE_DEPENDENT_CALLS
                .iter()
                .any(|(caller, c, _)| *caller == f.name && c == callee);
            if calls(&f.body, callee) && !allowed {
                problems.push(format!(
                    "{}: calls {callee}(), which is {MODE_TAG}, but is not",
                    f.name
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "declare these functions {MODE_TAG} (ADR 0036 -> Mixing modes):\n{}",
        problems.join("\n")
    );
    for (caller, callee, _) in NOT_MODE_DEPENDENT_CALLS {
        assert!(
            functions
                .iter()
                .any(|f| f.name == *caller && !f.head.contains(MODE_TAG) && calls(&f.body, callee)),
            "NOT_MODE_DEPENDENT_CALLS: {caller} no longer calls {callee}(); remove the entry"
        );
    }
}

/// Translation units built with and without exceptions, and with and without debug checks, keep their own behaviour
/// when linked into one program, in either link order, at -O0 and -O2, with every C++ compiler of the matrix, and the
/// two exception-mode objects define no common symbol that mentions tickertape (ADR 0036 -> Mixing modes). Without
/// the mode-specific inline namespace (exceptions) or the abi_tag (debug checks), the copy of `result<T>::value()`,
/// `append_with`, `list_segments` or `RecordBatch::operator[]` from the first object on the link line serves both
/// units: `value()` aborts instead of throwing, a fill's exception escapes, an allocation failure inside
/// `list_segments` terminates the process (Clang) or skips its clean-up (GCC), or a debug check disappears. The same
/// happens through a standard-library instantiation over a C struct (round 2's `std::vector<tickertape_segment_info>`),
/// which the shared-symbol check catches.
#[test]
fn cpp_binding_links_units_built_in_different_modes() {
    let scratch = tempfile::tempdir().unwrap();
    let out = scratch.path();
    let tests = manifest_dir().join("tests/cpp");
    let unit = tests.join("mixed_modes_unit.cpp");
    let main = tests.join("mixed_modes_main.cpp");
    lib_dir(); // build the library before the compilers run in parallel
    let objects: &[(&str, &Path, &[&str])] = &[
        (
            "noexc",
            &unit,
            &["-fno-exceptions", "-DTICKERTAPE_DEBUG_CHECKS=0"],
        ),
        ("exc", &unit, &["-DTICKERTAPE_DEBUG_CHECKS=0"]),
        ("dbg", &unit, &["-DTICKERTAPE_DEBUG_CHECKS=1"]),
        ("main_exceptions", &main, &["-DTT_MIXED_PAIR_EXCEPTIONS"]),
        (
            "main_debug_checks",
            &main,
            &["-DTT_MIXED_PAIR_DEBUG_CHECKS"],
        ),
    ];
    // (driver, first unit, second unit): each pair in both orders.
    let programs = [
        ("main_exceptions", "noexc", "exc"),
        ("main_exceptions", "exc", "noexc"),
        ("main_debug_checks", "dbg", "exc"),
        ("main_debug_checks", "exc", "dbg"),
    ];
    let compilers = cpp_compilers();
    assert!(!compilers.is_empty(), "no C++ compiler found");
    for (ci, cxx) in compilers.iter().enumerate() {
        // The sanitizers only with the compiler the other tests use (Homebrew GCC on macOS has none).
        let sanitize: &[&str] = if cxx.as_path() == compiler(true).path() {
            sanitizer_flags()
        } else {
            &[]
        };
        for opt in ["-O0", "-O2"] {
            let object = |name: &str| out.join(format!("{name}_{ci}{opt}.o"));
            let compiles = objects
                .iter()
                .map(|(name, source, flags)| {
                    let mut cmd = Command::new(cxx);
                    cmd.args(["-std=c++17", "-Wall", "-Wextra", "-Werror"])
                        .args(CPP_BINDING_WARNINGS)
                        .args(*flags)
                        .arg(opt)
                        .args(sanitize)
                        .arg("-I")
                        .arg(manifest_dir().join("include"))
                        .arg("-c")
                        .arg(source)
                        .arg("-o")
                        .arg(object(name));
                    cmd
                })
                .collect();
            run_all_ok(compiles);
            check_no_shared_binding_symbols(&object("exc"), &object("noexc"));
            for (driver, first, second) in programs {
                let program = out.join(format!("{driver}_{first}_{second}_{ci}{opt}"));
                let mut link = Command::new(cxx);
                link.args(sanitize)
                    .arg(object(driver))
                    .arg(object(first))
                    .arg(object(second));
                link_library(&mut link, Link::Shared);
                run_compiler(link.arg("-o").arg(&program));
                let run = |args: &[&str]| {
                    let output = command_with_valgrind_args(&program, VALGRIND_USER_ALLOCATOR)
                        .args(args)
                        .output()
                        .unwrap();
                    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    (output.status.success(), stdout, stderr)
                };
                let context = format!(
                    "{} ({}; objects in this order: {first}, {second})",
                    program.display(),
                    cxx.display()
                );
                if driver == "main_exceptions" {
                    let (ok, stdout, stderr) = run(&["exc-value"]);
                    assert!(
                        ok && stdout.contains("threw"),
                        "{context}: exc-value\n{stdout}{stderr}"
                    );
                    let (ok, stdout, stderr) = run(&["noexc-value"]);
                    assert!(
                        !ok && stderr.contains("result<T>::value() on an error")
                            && !stdout.contains("returned"),
                        "{context}: noexc-value must abort\n{stdout}{stderr}"
                    );
                    let journal = out.join(format!("exc_{first}_{ci}{opt}"));
                    let (ok, stdout, stderr) = run(&["exc-fill", journal.to_str().unwrap()]);
                    assert!(
                        ok && stdout.contains("rethrown"),
                        "{context}: exc-fill\n{stdout}{stderr}"
                    );
                    let journal = out.join(format!("noexc_{first}_{ci}{opt}"));
                    let (ok, stdout, stderr) = run(&["noexc-fill", journal.to_str().unwrap()]);
                    assert!(
                        ok && stdout.contains("committed"),
                        "{context}: noexc-fill\n{stdout}{stderr}"
                    );
                    // std::bad_alloc from the 1st, 2nd, ... allocation inside list_segments (an untagged function that
                    // the no-exceptions unit also calls) reaches the exceptions unit's catch every time, until a call
                    // succeeds. list_segments allocates at least twice (its C-struct buffer, then the result's vector).
                    let journal = out.join(format!("alloc_{first}_{ci}{opt}"));
                    let (ok, stdout, stderr) = run(&["exc-alloc", journal.to_str().unwrap()]);
                    let caught = stdout
                        .trim()
                        .strip_prefix("caught ")
                        .and_then(|n| n.parse::<i32>().ok());
                    assert!(
                        ok && caught.is_some_and(|n| n >= 2),
                        "{context}: exc-alloc must catch a failure at each allocation of list_segments\n\
                         {stdout}{stderr}"
                    );
                } else {
                    let (ok, stdout, stderr) = run(&["dbg-index"]);
                    assert!(
                        !ok && stderr.contains("RecordBatch index out of range")
                            && !stdout.contains("returned"),
                        "{context}: dbg-index must abort\n{stdout}{stderr}"
                    );
                    let (ok, stdout, stderr) = run(&["nodbg-index"]);
                    assert!(
                        ok && stdout.contains("unchecked 0"),
                        "{context}: nodbg-index\n{stdout}{stderr}"
                    );
                }
            }
        }
    }
}

/// The defined external symbols of an object file, as `nm` prints them, or `None` if `nm` cannot be run.
fn defined_symbols(object: &Path) -> Option<Vec<String>> {
    let output = Command::new("nm").arg("-g").arg(object).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let symbols = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            // "<address> <type> <name>" for a defined symbol, whatever its type letter (GNU nm prints `u` for a
            // defined unique global and `W`/`V` for defined weak ones); "<type> <name>" (U, w, v), with no address,
            // for an undefined one.
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                [_, kind, name] if *kind != "U" => Some((*name).to_owned()),
                _ => None,
            }
        })
        .collect();
    Some(symbols)
}

/// No symbol that mentions `tickertape` is defined in both the exceptions object and the no-exceptions object (ADR
/// 0036 -> Mixing modes). Such a symbol is a weak definition the linker keeps once for the whole program, so one
/// mode's copy would run in the other: for example `std::vector<tickertape_segment_info>::resize`, a standard-library
/// instantiation over a C struct, which is outside the binding's namespace. Skipped with a message without `nm`.
fn check_no_shared_binding_symbols(exceptions: &Path, no_exceptions: &Path) {
    let (Some(exc), Some(noexc)) = (defined_symbols(exceptions), defined_symbols(no_exceptions))
    else {
        eprintln!(
            "skipping the shared-symbol check of the mixed-mode objects: nm is not available"
        );
        return;
    };
    assert!(
        exc.iter().any(|s| s.contains("tickertape")),
        "nm found no tickertape symbol in {}: the check would prove nothing",
        exceptions.display()
    );
    let shared: Vec<&String> = exc
        .iter()
        .filter(|s| s.contains("tickertape") && noexc.contains(s))
        .collect();
    assert!(
        shared.is_empty(),
        "symbols defined by both the exceptions and the no-exceptions object ({} and {}); the linker keeps one \
         copy for both modes:\n{}",
        exceptions.display(),
        no_exceptions.display(),
        shared
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A binding type cannot be passed between units built in different exception modes: the same declaration names a
/// function taking `tickertape::v1::Writer` in one unit and `tickertape::v1_noexcept::Writer` in the other, so the
/// link fails (ADR 0036 -> Mixing modes). Built in the same mode, the same two units link (the control).
#[test]
fn cpp_binding_types_do_not_cross_exception_modes() {
    let scratch = tempfile::tempdir().unwrap();
    let out = scratch.path();
    let source = manifest_dir().join("tests/cpp/mixed_modes_boundary.cpp");
    lib_dir();
    let objects: &[(&str, &[&str])] = &[
        ("define_noexc", &["-fno-exceptions", "-DTT_BOUNDARY_DEFINE"]),
        ("define_exc", &["-DTT_BOUNDARY_DEFINE"]),
        ("call_exc", &[]),
    ];
    let compiles = objects
        .iter()
        .map(|(name, flags)| {
            let mut cmd = compiler_command("c++17", &[CPP_BINDING_WARNINGS, flags].concat());
            cmd.arg("-c")
                .arg(&source)
                .arg("-o")
                .arg(out.join(format!("{name}.o")));
            cmd
        })
        .collect();
    run_all_ok(compiles);
    let link = |definition: &str| {
        let mut cmd = compiler(true).to_command();
        cmd.args(sanitizer_flags())
            .arg(out.join("call_exc.o"))
            .arg(out.join(format!("{definition}.o")));
        link_library(&mut cmd, Link::Shared);
        cmd.arg("-o")
            .arg(out.join(format!("boundary_{definition}")));
        cmd.output().expect("run the linker")
    };
    let control = link("define_exc");
    assert!(
        control.status.success(),
        "the same mode must link\n{}",
        String::from_utf8_lossy(&control.stderr)
    );
    let crossed = link("define_noexc");
    let stderr = String::from_utf8_lossy(&crossed.stderr);
    assert!(
        !crossed.status.success() && stderr.contains("tt_boundary_is_open"),
        "passing a tickertape::Writer between exception modes must fail to link\n{stderr}"
    );
}

// ---- Code the binding refuses at compile time (ADR 0036) ----

/// Programs that must not compile, and their neighbours that must, as C++17 and C++20 (the C++20-only statements
/// at C++20): `new_record` refuses an rvalue range that owns its bytes and accepts lvalues and rvalue views (types
/// for which `tickertape::enable_borrowed_bytes` is true: `std::basic_string_view`, `std::span`, and a user's view
/// type that specialises it, at every language level; `std::ranges::enable_borrowed_range` is not consulted);
/// `result<T>` refuses a T whose move can throw; a const rvalue `result<Writer>` does not hand out its Writer;
/// `tickertape::PathArg` cannot be named, while temporaries passed as path arguments compile.
#[test]
fn cpp_binding_refuses_unsafe_code_at_compile_time() {
    const PRELUDE: &str = r#"#include "tickertape.hpp"
#include <array>
#include <filesystem>
#if __cplusplus >= 202002L
#include <ranges>
#include <span>
#endif
#include <string>
#include <string_view>
#include <utility>
#include <vector>
namespace tt = tickertape;
const std::string const_string();
// A user's own view type: it does not own its bytes. It opts into tickertape::enable_borrowed_bytes below.
struct MyView {
    const char *p;
    std::size_t n;
    const char *data() const { return p; }
    std::size_t size() const { return n; }
    const char *begin() const { return p; }
    const char *end() const { return p + n; }
};
template <>
inline constexpr bool tt::enable_borrowed_bytes<MyView> = true;
#if __cplusplus >= 202002L
// A view that opts into std::ranges::enable_borrowed_range only, which the binding does not consult.
struct StdBorrowed {
    const char *p;
    std::size_t n;
    const char *data() const { return p; }
    std::size_t size() const { return n; }
    const char *begin() const { return p; }
    const char *end() const { return p + n; }
};
template <>
inline constexpr bool std::ranges::enable_borrowed_range<StdBorrowed> = true;
#endif
// The same without opting in.
struct NotBorrowed {
    const char *p;
    std::size_t n;
    const char *data() const { return p; }
    std::size_t size() const { return n; }
};
struct ThrowingMove {
    ThrowingMove() = default;
    ThrowingMove(ThrowingMove &&) noexcept(false) {}
};
void use(std::vector<tt::NewRecord> &batch, std::string &owned, const std::string &cowned,
         std::vector<std::uint8_t> &bytes, std::array<char, 1> &fixed, const tt::result<tt::Writer> &opened);
void use(std::vector<tt::NewRecord> &batch, std::string &owned, const std::string &cowned,
         std::vector<std::uint8_t> &bytes, std::array<char, 1> &fixed, const tt::result<tt::Writer> &opened) {
    (void)batch; (void)owned; (void)cowned; (void)bytes; (void)fixed; (void)opened;
"#;
    // (statement, a word the compiler's error must contain); each must fail to compile.
    const REFUSED: &[(&str, &str)] = &[
        (
            r#"batch.push_back(tt::new_record(1, 0, 0, std::to_string(1) + "..."));"#,
            "deleted",
        ),
        (
            r#"tt::NewRecord r = tt::new_record(1, 0, 0, std::string("x")); (void)r;"#,
            "deleted",
        ),
        (
            "tt::NewRecord r = tt::new_record(1, 0, 0, std::vector<char>{'a'}); (void)r;",
            "deleted",
        ),
        (
            "tt::NewRecord r = tt::new_record(1, 0, 0, std::array<char, 1>{{'a'}}); (void)r;",
            "deleted",
        ),
        (
            "tt::NewRecord r = tt::new_record(1, 0, 0, std::move(owned)); (void)r;",
            "deleted",
        ),
        (
            "tt::NewRecord r = tt::new_record(1, 0, 0, const_string()); (void)r;",
            "deleted",
        ),
        (
            "batch.push_back(tt::new_record(1, 0, 0, std::vector<std::uint8_t>(4)));",
            "deleted",
        ),
        (
            "batch.push_back(tt::new_record(1, 0, 0, NotBorrowed{\"x\", 1}));",
            "deleted",
        ),
        (
            "batch.push_back(tt::new_record(1, 0, 0, StdBorrowed{\"x\", 1}));",
            "deleted",
        ),
        (
            "batch.push_back(tt::new_record(1, 0, 0, std::ranges::subrange(owned.data(), owned.data() + 1)));",
            "deleted",
        ),
        (
            "tt::result<ThrowingMove> r = ThrowingMove(); (void)r;",
            "nothrow move",
        ),
        (
            "tt::Writer w = std::move(opened).value(); (void)w;",
            "deleted",
        ),
        ("tt::Writer w = *std::move(opened); (void)w;", "deleted"),
        (
            r#"tt::PathArg p = std::filesystem::path("root") / "journal"; (void)p;"#,
            "PathArg",
        ),
        (
            r#"const tt::PathArg &p = std::string("root"); (void)p;"#,
            "PathArg",
        ),
    ];
    const ACCEPTED: &[&str] = &[
        "batch.push_back(tt::new_record(1, 0, 0, owned));",
        "batch.push_back(tt::new_record(1, 0, 0, cowned));",
        "batch.push_back(tt::new_record(1, 0, 0, bytes));",
        "batch.push_back(tt::new_record(1, 0, 0, fixed));",
        "batch.push_back(tt::new_record(1, 0, 0, std::string_view(owned)));",
        r#"batch.push_back(tt::new_record(1, 0, 0, std::string_view("literal")));"#,
        "batch.push_back(tt::new_record(1, 0, 0, std::span<const std::uint8_t>(bytes)));",
        "batch.push_back(tt::new_record(1, 0, 0, std::span<const char, 1>(fixed)));",
        "batch.push_back(tt::new_record(1, 0, 0, std::as_bytes(std::span<const char>(owned))));",
        "batch.push_back(tt::new_record(1, 0, 0, MyView{owned.data(), owned.size()}));",
        "batch.push_back(tt::new_record(1, 0, 0, owned.data(), owned.size()));",
        "tt::result<int> r = 1; (void)r;",
        "const tt::Writer &w = opened.value(); (void)w;",
        r#"(void)tt::verify(std::filesystem::path("root") / "journal");"#,
        r#"(void)tt::Writer::open(std::string("root") + "/journal");"#,
        r#"tt::Retention r; r.move_to(std::filesystem::path("root") / "archive");"#,
    ];
    // Statements that name C++20 library types are compiled as C++20 only.
    let needs_cpp20 = |statement: &str| {
        ["std::span", "std::ranges", "std::as_bytes", "StdBorrowed"]
            .iter()
            .any(|name| statement.contains(name))
    };
    let scratch = tempfile::tempdir().unwrap();
    let compilers = cpp_compilers();
    assert!(!compilers.is_empty(), "no C++ compiler found");
    let mut jobs = Vec::new();
    let mut cases = Vec::new();
    for (i, (accept, statement, needle)) in REFUSED
        .iter()
        .map(|(s, n)| (false, *s, *n))
        .chain(ACCEPTED.iter().map(|s| (true, *s, "")))
        .enumerate()
    {
        let source = scratch.path().join(format!("case_{i}.cpp"));
        std::fs::write(&source, format!("{PRELUDE}    {statement}\n}}\n")).unwrap();
        let standards: &[&str] = if needs_cpp20(statement) {
            &["c++20"]
        } else {
            &["c++17", "c++20"]
        };
        for compiler in &compilers {
            for std in standards {
                let mut cmd = Command::new(compiler);
                cmd.arg(format!("-std={std}"))
                    .args(["-fsyntax-only", "-Wall", "-Wextra", "-Werror"])
                    .args(CPP_BINDING_WARNINGS)
                    .arg("-I")
                    .arg(manifest_dir().join("include"))
                    .arg(&source);
                jobs.push(cmd);
                cases.push((accept, statement, needle));
            }
        }
    }
    for ((accept, statement, needle), (cmd, output)) in cases.into_iter().zip(run_parallel(jobs)) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if accept {
            assert!(
                output.status.success(),
                "must compile: {statement}\n{cmd:?}\n{stderr}"
            );
        } else {
            assert!(
                !output.status.success() && stderr.contains(needle),
                "must be refused (with \"{needle}\" in the error): {statement}\n{cmd:?}\n{stderr}"
            );
        }
    }
}
