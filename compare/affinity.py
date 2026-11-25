"""Thread pinning for both drivers: which CPUs to use, placing the runner, and checking what was in effect.

The runner moves itself to the helper CPUs after the build, so every child process and every thread a tool starts
inherits them. Each measured thread (writer, reader) then pins itself to its own CPU, and reports its affinity from
/proc at the end of the run; the Keeper moves threads that start on a measured CPU later. See README.md, Thread pinning.
"""
import os
from pathlib import Path
import platform
import shutil
import threading

SYS_CPU = Path("/sys/devices/system/cpu")
JAVA_TOOLS = ("chronicle", "aeron")


def parse_cpus(text):
    """The kernel's list format: 0-3,8."""
    cpus = set()
    for part in text.strip().split(","):
        if part:
            first, _, last = part.partition("-")
            first, last = int(first), int(last or first)
            if first > last:
                raise ValueError(f"invalid CPU range {part}")
            cpus.update(range(first, last + 1))
    return cpus


def cpu_list(cpus):
    return ",".join(str(c) for c in cpus)


def add_options(parser):
    group = parser.add_argument_group("thread pinning (Linux; see compare/README.md)")
    group.add_argument("--writer-cpu", type=int, help="CPU of the writer thread (default: chosen from the topology)")
    group.add_argument("--reader-cpu", type=int, help="CPU of the reader thread (default: chosen from the topology)")
    group.add_argument("--helper-cpus", help="CPUs of every other thread, for example 0-3,8 (default: every allowed, "
                                             "non-isolated CPU outside the writer's and the reader's physical cores)")
    group.add_argument("--no-pin", action="store_true", help="pin nothing, as before this option existed")


def _read(path, default=None):
    try:
        return path.read_text().strip()
    except OSError:
        return default


class Topology:
    def __init__(self, root=SYS_CPU, allowed=None):
        self.root = Path(root)
        self.online = parse_cpus(_read(self.root / "online"))
        self.isolated = parse_cpus(_read(self.root / "isolated", "")) & self.online
        self.allowed = set(os.sched_getaffinity(0) if allowed is None else allowed) & self.online

    def siblings(self, cpu):
        """The CPUs of this CPU's physical core (hyperthreads), itself included."""
        return parse_cpus(_read(self.root / f"cpu{cpu}/topology/thread_siblings_list", str(cpu))) | {cpu}

    def domain(self, cpu):
        """What a writer and a reader should share: the package and, where the kernel reports one, the L3 cache."""
        for cache in sorted((self.root / f"cpu{cpu}/cache").glob("index*")):
            if _read(cache / "level") == "3":
                return _read(self.root / f"cpu{cpu}/topology/physical_package_id"), _read(cache / "shared_cpu_list")
        return _read(self.root / f"cpu{cpu}/topology/physical_package_id"), None


def _choose(topology, count, taken, near):
    """`count` CPUs on different physical cores without a CPU from `taken`, in one domain if possible, and not on
    CPU 0's core (the usual target of interrupts) if possible."""
    cores = {}
    for cpu in sorted(topology.allowed - topology.isolated):
        core = topology.siblings(cpu)
        if not core & taken:
            cores.setdefault(min(core), cpu)
    cpus = sorted(cores.values())
    core_zero = topology.siblings(0)
    domains = {}
    for cpu in cpus:
        domains.setdefault(topology.domain(cpu), []).append(cpu)
    near = [topology.domain(cpu) for cpu in near]
    # A named CPU's domain first, then domains in CPU order; within one, CPU 0's core only if the others do not do.
    ordered = sorted(domains, key=lambda d: (d not in near, domains[d][0]))
    candidates = [[c for c in domains[d] if c not in core_zero] for d in ordered] + [domains[d] for d in ordered]
    if len(cpus) >= count:
        candidates.append([c for c in cpus if c not in core_zero])
        candidates.append(cpus)
    for group in candidates:
        if len(group) >= count:
            return group[:count]
    raise ValueError(f"automatic pinning needs one physical core each for the writer, the reader and the helper "
                     f"threads; CPUs {cpu_list(sorted(topology.allowed)) or 'none'} do not have them. Name CPUs with "
                     f"--writer-cpu, --reader-cpu and --helper-cpus, or pass --no-pin")


def resolve(args, system=None, topology=None):
    """The pinning plan from the options: JSON-safe, recorded in every result and in matrix.json."""
    system = system or platform.system()
    named = [o for o in ("writer_cpu", "reader_cpu", "helper_cpus") if getattr(args, o, None) is not None]
    if getattr(args, "no_pin", False):
        if named:
            raise ValueError("--no-pin excludes --writer-cpu, --reader-cpu and --helper-cpus")
        return {"mode": "none", "summary": "affinity: none (--no-pin)"}
    if system != "Linux":
        name = "macOS" if system == "Darwin" else system
        if named:
            raise ValueError(f"affinity is unsupported on {name}")
        return {"mode": "unsupported", "summary": f"affinity: unsupported on {name}"}
    topology = topology or Topology()
    measured = {"writer_cpu": args.writer_cpu, "reader_cpu": args.reader_cpu}
    helpers = None if args.helper_cpus is None else parse_cpus(args.helper_cpus)
    explicit = {c for c in measured.values() if c is not None}
    offline = (explicit | (helpers or set())) - topology.online
    if offline:
        raise ValueError(f"CPUs {cpu_list(sorted(offline))} do not exist or are offline")
    missing = [role for role, cpu in measured.items() if cpu is None]
    if missing:
        taken = set().union(*[topology.siblings(c) for c in explicit | (helpers or set())])
        for role, cpu in zip(missing, _choose(topology, len(missing), taken, sorted(explicit))):
            measured[role] = cpu
    if measured["writer_cpu"] == measured["reader_cpu"]:
        raise ValueError("the writer and the reader need different CPUs")
    cores = {c: topology.siblings(c) for c in measured.values()}
    if helpers is None:
        helpers = topology.allowed - topology.isolated - set().union(*cores.values())
        if not helpers:
            raise ValueError("no CPU is left for the helper threads outside the writer's and the reader's physical "
                             "cores; name some with --helper-cpus, or pass --no-pin")
    if not helpers or helpers & set(cores):
        raise ValueError("--helper-cpus must name CPUs other than the writer's and the reader's")
    shared = {c for c, core in cores.items() if core & ((helpers | set(cores)) - {c})}
    return {"mode": "pinned", **measured, "helper_cpus": sorted(helpers),
            "named_by_caller": named, "isolated_cpus": sorted(topology.isolated),
            # Only explicit choices can do this: a hyperthread of a measured core runs the other measured thread or
            # helper threads.
            "measured_cpus_sharing_a_physical_core": sorted(shared),
            "summary": f"affinity: writer CPU {measured['writer_cpu']}, reader CPU {measured['reader_cpu']}, "
                       f"helper CPUs {cpu_list(sorted(helpers))}"}


def check(plan, tools):
    """Fails before the build if the plan cannot work here (a cpuset without these CPUs, no taskset for Java)."""
    if plan["mode"] != "pinned":
        return
    if any(t in JAVA_TOOLS for t in tools) and not shutil.which("taskset"):
        raise ValueError("pinning the Java adapters needs taskset (util-linux), or pass --no-pin")
    before = os.sched_getaffinity(0)
    try:
        for cpus in ([plan["writer_cpu"]], [plan["reader_cpu"]], plan["helper_cpus"]):
            try:
                os.sched_setaffinity(0, cpus)
            except OSError as error:
                raise ValueError(f"cannot run on CPUs {cpu_list(cpus)}: {error}") from error
            if os.sched_getaffinity(0) != set(cpus):
                raise ValueError(f"cannot run on all of CPUs {cpu_list(cpus)} (cgroup cpuset?)")
    finally:
        os.sched_setaffinity(0, before)


def place_runner(plan):
    """Moves the runner, and with it every process started from here on, to the helper CPUs."""
    if plan["mode"] == "pinned":
        os.sched_setaffinity(0, plan["helper_cpus"])


class Keeper:
    """Keeps the measured CPUs to the measured threads while a measurement runs.

    A thread starts on the CPUs of the thread that created it, so one that a tool starts from its measured thread after
    that was pinned starts on the measured CPU (yaque's reader: a file watcher per segment). Every INTERVAL_S this
    moves such threads of the watched processes to the helper CPUs, and counts them for the result."""
    INTERVAL_S = 0.01

    def __init__(self, plan, control):
        self.plan, self.control = plan, Path(control)
        self.processes, self.measured, self.moved = [], {}, {}
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True) if plan["mode"] == "pinned" else None

    def __enter__(self):
        if self.thread:
            self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        if self.thread:
            self.thread.join()

    def watch(self, pid, role):
        self.processes.append((pid, role))

    def _run(self):
        while not self.stop.wait(self.INTERVAL_S):
            for pid, role in list(self.processes):
                try:
                    self.sweep(pid, role)
                except OSError:
                    self.processes.remove((pid, role))  # It has ended.

    def sweep(self, pid, role):
        tasks = Path(f"/proc/{pid}/task")
        if pid not in self.measured:
            tid = int(_read(self.control / f"{role}.tid", "0"))
            # Not pinned yet, or the id an earlier process of this role left: there is nothing to move before that.
            if not tid or not (tasks / str(tid)).exists():
                return
            self.measured[pid] = tid
        cpus = {self.plan["writer_cpu"], self.plan["reader_cpu"]}
        for task in os.listdir(tasks):
            tid = int(task)
            if tid == self.measured[pid]:
                continue
            try:
                if cpus & os.sched_getaffinity(tid):
                    os.sched_setaffinity(tid, self.plan["helper_cpus"])
                    key = role, _read(tasks / task / "comm", "?")
                    self.moved[key] = self.moved.get(key, 0) + 1
            except OSError:
                pass  # This thread has ended.


def adapter_options(plan, tool, role):
    """Options for one adapter process. Java needs no --helper-cpus: the JVM's threads inherit the runner's CPUs."""
    if plan is None or plan["mode"] != "pinned":
        return []
    options = ["--cpu", str(plan[f"{role}_cpu"])]
    return options if tool in JAVA_TOOLS else options + ["--helper-cpus", cpu_list(plan["helper_cpus"])]


def latency_options(plan):
    """The same for tt-latency, which starts both roles itself."""
    if plan is None or plan["mode"] != "pinned":
        return []
    return ["--writer-cpu", str(plan["writer_cpu"]), "--reader-cpu", str(plan["reader_cpu"]),
            "--helper-cpus", cpu_list(plan["helper_cpus"])]


def record(plan, writer, reader, keeper=None):
    """The result's affinity section: the plan and what the processes read back. Takes `affinity` out of the raw
    child results. Fails if the plan is pinned and a measured thread or a helper thread was somewhere else."""
    section = dict(plan)
    if keeper is not None and plan["mode"] == "pinned":
        section["moved_threads"] = [{"role": role, "name": name, "count": count}
                                    for (role, name), count in sorted(keeper.moved.items())]
    for role, result in (("writer", writer), ("reader", reader)):
        if result is not None:
            section[role] = result.pop("affinity")
    validate(section)
    return section


def validate(section):
    if section["mode"] != "pinned":
        return
    measured = {section["writer_cpu"], section["reader_cpu"]}
    assert "writer" in section or "reader" in section, "no affinity read back"
    for role in ("writer", "reader"):
        actual = section.get(role)
        if actual is None:
            continue
        cpu = section[f"{role}_cpu"]
        assert actual.get("cpus_allowed") == [cpu] and actual["last_cpu"] == cpu, \
            f"the {role} was not pinned to CPU {cpu}: {actual}"
        for thread in actual["other_threads"]:
            assert not measured & set(thread["cpus_allowed"]), \
                f"a thread of the {role} process can run on a measured CPU: {thread}"
