"""Settings of the JVM tools (issue #56): warm-up, Chronicle configuration, JVM logs and experimental JVM flags.

Everything here is on by default except the experiments, can be switched off one by one, and is recorded in every
result of a JVM tool under "java", so runs with the old defaults stay reproducible.
"""

JAVA_TOOLS = ("chronicle", "aeron")
# Enough for the index array boundary at record 1,048,576 (Chronicle: 4096 x 256 records) with a margin for the
# recompilation after it, and for at least two 64 MiB chunks at 64 B (84 B per record on disk).
LONG_WARMUP_RECORDS = 1_200_000
# Chronicle's README on blockSize: "in production you may want this to be high to reduce jitter from creating new
# chunks", but also "You should avoid changing `blockSize` unnecessarily". 1G is its system.properties example, and one
# of Chronicle's benchmarks (InternalBenchmarkMain) sets 1 << 30; others use other sizes (compare/README.md).
CHRONICLE_BLOCK_SIZE = 1 << 30
CHRONICLE_DEFAULT_BLOCK_SIZE = 64 << 20
# Last wins for -Xmx; the suite's GC has to be deselected before another one can be selected.
EXPERIMENTS = {
    "always-pre-touch": ["-XX:+AlwaysPreTouch"],
    "epsilon": ["-XX:-UseParallelGC", "-XX:+UnlockExperimentalVMOptions", "-XX:+UseEpsilonGC", "-Xmx4g"],
    "no-guaranteed-safepoint": ["-XX:+UnlockDiagnosticVMOptions", "-XX:GuaranteedSafepointInterval=0"],
}
DEFAULTS = {"long_warmup": True, "micro_touch": True, "chronicle_block_size": CHRONICLE_BLOCK_SIZE,
            "aeron_sparse_terms": False, "aeron_pre_touch": True, "jvm_logs": True, "experiments": []}
# For the runner's timeouts: each Java process of a run warms up, the long warm-up of W3 writes about 4 GB per process
# at 1,024 B, and a slow disk can take minutes for that.
WARMUP_ALLOWANCE_S = 600


def add_arguments(parser):
    group = parser.add_argument_group("JVM tools (defaults on; see compare/README.md)")
    group.add_argument("--no-long-warmup", action="store_true",
                       help="the warm-up of earlier results: 100,000 records for W1/W3, none for W2 and sustained runs")
    group.add_argument("--no-micro-touch", action="store_true", help="no appender.microTouch() in Chronicle's busy-wait")
    group.add_argument("--chronicle-block-size", default=str(CHRONICLE_BLOCK_SIZE), metavar="BYTES",
                       help="Chronicle blockSize in bytes (default 1073741824), or 'default' for Chronicle's own 64 MiB")
    group.add_argument("--aeron-sparse-terms", action="store_true",
                       help="Aeron's default sparse term buffers (default: not sparse, as LowLatencyMediaDriver)")
    group.add_argument("--no-aeron-pre-touch", action="store_true",
                       help="no preTouchMappedMemory in Aeron's clients")
    group.add_argument("--no-jvm-logs", action="store_true", help="no JVM logs and late-send events beside the results")
    group.add_argument("--java-experiment", action="append", default=[], choices=sorted(EXPERIMENTS),
                       help="opt-in JVM flags, repeatable")


def settings(args):
    block = args.chronicle_block_size
    if block != "default":
        block = int(block)
        if block < 1 << 16:
            raise ValueError("--chronicle-block-size must be 'default' or at least 65536")
    return {"long_warmup": not args.no_long_warmup, "micro_touch": not args.no_micro_touch,
            "chronicle_block_size": block, "aeron_sparse_terms": args.aeron_sparse_terms,
            "aeron_pre_touch": not args.no_aeron_pre_touch, "jvm_logs": not args.no_jvm_logs,
            "experiments": sorted(set(args.java_experiment))}


def jvm_flags(java, log):
    """Flags for run.sh --jvm=...; `log` is the log file or None. -Xlog:async hands the lines to a writer thread, so
    the VM and compiler threads that log never wait for the file; utctime and timenanos are the wall clock and
    System.nanoTime()."""
    flags = [flag for name in java["experiments"] for flag in EXPERIMENTS[name]]
    if java["jvm_logs"] and log is not None:
        flags += ["-Xlog:async", f"-Xlog:gc*,safepoint,handshake*,jit+compilation=debug,deoptimization=debug:file=\"{log}\""
                                 ":utctime,timenanos,tags:filecount=0"]
    return flags


def warmup_records(java, quick, sustained):
    if java["long_warmup"]:
        return 4_000 if quick else LONG_WARMUP_RECORDS
    return 1000 if quick or sustained else 100_000


def warms_up_before_open(tool, java):
    """True if the writer waits for the reader process's warm-up before it opens the measured endpoint (Harness.java),
    so the runner has to start the reader without waiting for the writer."""
    return tool in JAVA_TOOLS and java["long_warmup"]


def adapter_arguments(tool, java, quick, sustained, events):
    args = ["--jit-warmup", str(warmup_records(java, quick, sustained)),
            "--warmup-mode", "long" if java["long_warmup"] else "short"]
    if tool == "chronicle":
        args += ["--micro-touch", "on" if java["micro_touch"] else "off",
                 "--block-size", str(java["chronicle_block_size"])]
    if tool == "aeron":
        args += ["--sparse-terms", "on" if java["aeron_sparse_terms"] else "off",
                 "--pre-touch", "on" if java["aeron_pre_touch"] else "off"]
    if java["jvm_logs"] and events is not None:
        args += ["--events", str(events)]
    return args


def log_paths(out, name):
    """The JVM log and the events file of one child process, beside the results."""
    directory = out / "jvm-logs"
    directory.mkdir(exist_ok=True)
    return directory / f"{name}.log", directory / f"{name}.events.json"


def wrap(cmd, tool, java, quick, sustained, out, name):
    """Adds the JVM flags and adapter options to a child command [run.sh, role, ...] of a JVM tool. Without `out` (a
    command that is only looked at) it has no log files."""
    log, events = log_paths(out, name) if java["jvm_logs"] and out is not None else (None, None)
    return [cmd[0], *(f"--jvm={flag}" for flag in jvm_flags(java, log)), *cmd[1:],
            *adapter_arguments(tool, java, quick, sustained, events)]


def collect(tool, java, children, quick, sustained, live):
    """Moves the "java" block out of each child's result and joins them with the requested settings, or fails if a
    child ran with others. `children` maps the role to the child's result (modified)."""
    if tool not in JAVA_TOOLS:
        return None
    block = {"settings": java}
    for role, result in children.items():
        block[role] = result.pop("java")
        for name, want, got in differences(tool, java, role, block[role], quick, sustained, live):
            raise RuntimeError(f"{tool} {role} ran with {name} {got!r} instead of {want!r}")
    return block


def differences(tool, java, role, child, quick, sustained, live):
    """(name, requested, reported) for every setting of a child process that is not the requested one."""
    warmup, reported = child["warmup"], child["tool"]
    records = warmup_records(java, quick, sustained) if java["long_warmup"] or not live else 0
    checks = [("warm-up", "long" if java["long_warmup"] else "short", warmup["mode"]),
              ("warm-up records", records, warmup["records"])]
    if tool == "chronicle":
        block = java["chronicle_block_size"]
        checks.append(("blockSize", CHRONICLE_DEFAULT_BLOCK_SIZE if block == "default" else block,
                       reported["block_size"]))
        if role == "writer":
            checks.append(("microTouch", java["micro_touch"], reported["micro_touch"]))
    else:
        checks.append(("preTouchMappedMemory", java["aeron_pre_touch"], reported["pre_touch"]))
        if role == "writer":
            checks.append(("termBufferSparseFile", java["aeron_sparse_terms"], reported["sparse_terms"]))
    arguments = child["jvm_arguments"]
    for name, flags in EXPERIMENTS.items():
        present = all(flag in arguments for flag in flags)
        checks.append((f"experiment {name}", name in java["experiments"], present))
    checks.append(("JVM log", java["jvm_logs"], any(a.startswith("-Xlog:gc*") for a in arguments)))
    checks.append(("-Xlog:async", java["jvm_logs"], "-Xlog:async" in arguments))
    return [c for c in checks if c[1] != c[2]]
