#!/usr/bin/env bash
set -euo pipefail
umask 077

exec python3 -c 'import fcntl
import json
import os
from pathlib import Path
import resource
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

os.umask(0o077)

class Failure(Exception):
    def __init__(self, message, code=1):
        self.message, self.code = message, code

class Interrupted(BaseException):
    def __init__(self, number): self.number = number

def inherited(name, lock=False):
    raw = os.environ.get(name)
    if raw is None: return None
    try:
        if not raw or len(raw) > 10 or not raw.isascii() or not raw.isdecimal(): raise ValueError()
        fd = int(raw)
        if fd < 3 or fd > 2**31-1 or str(fd) != raw: raise ValueError()
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or not os.get_inheritable(fd): raise ValueError()
        if not lock and (info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o600
                         or info.st_nlink != 1 or info.st_size > 32768
                         or not fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_APPEND
                         or fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_ACCMODE == os.O_RDONLY): raise ValueError()
        return fd
    except (OSError, ValueError, OverflowError):
        if lock: raise Failure("invalid inherited proof lock descriptor", 64)
        return None

proof_lock = None
diag_fd = None
order_format = False
workers = {}
temporary = None
gnark_lock = None
active_phase = None
launching = False
pending_signal = None

def counters(phase, root=None):
    if phase != "shim_workers": return None
    try:
        if root is None:
            relative = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
            root = Path("/sys/fs/cgroup") / relative.lstrip("/")
        def values(name):
            result = {}
            for line in (root / name).read_text().splitlines():
                key, value = line.split()
                if key in result or not value.isascii() or not value.isdecimal(): raise ValueError()
                value = int(value)
                if value > 2**63-1: raise ValueError()
                result[key] = value
            return result
        memory, pids = values("memory.events"), values("pids.events")
        return {"pids_max":pids["max"], "memory_max":memory["max"], "oom":memory["oom"],
                "oom_kill":memory["oom_kill"], "oom_group_kill":memory["oom_group_kill"]}
    except (OSError, ValueError, KeyError, StopIteration): return None

def event(phase, edge, *, worker=None, exit_code=None, signal_number=None, errno=None, cleanup=False, failure=None):
    global active_phase
    if edge == "begin": active_phase = phase
    elif active_phase == phase: active_phase = None
    if diag_fd is None: return
    value = None if order_format else {"v":1, "writer":"shim", "command":"prove", "phase":phase, "edge":edge,
             "exit_code":exit_code, "signal":signal_number, "errno":errno, "failure_class":failure,
             "worker":worker, "cleanup_requested":cleanup, "counters":counters(phase)}
    if order_format:
        root, identity = None, None
        try:
            relative = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
            root = Path("/sys/fs/cgroup") / relative.lstrip("/")
            info = root.stat(); identity = [info.st_dev, info.st_ino]
        except (OSError, ValueError, StopIteration): pass
        sampled = counters(phase, root) if root is not None else None
        values = [sampled[key] for key in ("pids_max", "memory_max", "oom", "oom_kill", "oom_group_kill")] if sampled is not None else None
        stamp = time.monotonic_ns()
        value = {"v":2, "kind":"event", "w":"shim", "c":"prove", "p":phase, "e":edge,
                 "t":stamp if stamp <= 2**63-1 else None, "x":exit_code, "s":signal_number, "n":errno,
                 "f":failure, "r":worker, "k":cleanup, "g":identity, "q":values, "i":None, "a":None}
    try:
        data = (json.dumps(value, separators=(",", ":")) + "\n").encode()
        if len(data) > 512 or os.fstat(diag_fd).st_size + len(data) > 32768:
            os.ftruncate(diag_fd, 32769)
        elif os.write(diag_fd, data) != len(data):
            os.ftruncate(diag_fd, 32769)
    except (OSError, ValueError): pass

def collect(pid, status, cleanup=False):
    role, process = workers.pop(pid)
    process.returncode = os.waitstatus_to_exitcode(status)
    code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else None
    number = os.WTERMSIG(status) if os.WIFSIGNALED(status) else None
    success = code == 0
    event("worker_wait", "ok" if success else "error", worker=role, exit_code=code,
          signal_number=number, cleanup=cleanup,
          failure=None if success else "worker_signal" if number is not None else "worker_exit")
    return success, role

def cleanup():
    for pid in list(workers):
        try:
            observed, status = os.waitpid(pid, os.WNOHANG)
            requested = not observed
            if requested:
                try: os.kill(pid, signal.SIGKILL)
                except ProcessLookupError: pass
                _, status = os.waitpid(pid, 0)
            collect(pid, status, requested)
        except ChildProcessError:
            workers.pop(pid, None)
    if temporary is not None: shutil.rmtree(temporary, ignore_errors=True)

def run():
    global proof_lock, diag_fd, temporary, gnark_lock, launching, order_format
    proof_lock = inherited("VIBEFACE_DELIVERY_PROOF_LOCK_FD", lock=True)
    diag_fd = inherited("VIBEFACE_DELIVERY_DIAGNOSTIC_FD")
    format_name = os.environ.get("VIBEFACE_DELIVERY_DIAGNOSTIC_FORMAT")
    order_format = format_name == "order-job-v2"
    if format_name is not None and not order_format: diag_fd = None
    if diag_fd == proof_lock: diag_fd = None
    arguments = sys.argv[1:]
    if arguments == ["--version"]:
        print("Vibeface confined RISC Zero Groth16 runner v2025-04-03.1")
        return
    event("shim_validate", "begin")
    if len(arguments) != 5 or arguments[:3] != ["run", "--rm", "-v"] or arguments[4] != "risczero/risc0-groth16-prover:v2025-04-03.1":
        event("shim_validate", "error", failure="stage_error")
        raise Failure("unsupported confined Groth16 invocation", 64)
    mount = arguments[3]
    if not mount.startswith("/") or not mount.endswith(":/mnt"):
        raise Failure("invalid confined Groth16 work directory", 64)
    work = mount[:-5]
    try: canonical = str(Path(work).resolve(strict=True))
    except OSError: raise Failure("work directory must be a canonical absolute path", 64) from None
    if work == "/" or ":" in work or "\n" in work or "\r" in work or canonical != work:
        raise Failure("work directory must be a canonical absolute path", 64)
    info = os.stat(work)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid(): raise Failure("work directory must belong to the prover", 64)
    if stat.S_IMODE(info.st_mode) & 0o077: raise Failure("work directory must be private", 64)
    work = Path(work)
    if not (work / "input.json").is_file() or (work / "input.json").is_symlink(): raise Failure("missing regular Groth16 input", 64)
    if os.path.lexists(work / "proof.json"): raise Failure("Groth16 output already exists", 64)
    event("shim_validate", "ok")
    event("shim_lock", "begin")
    gnark_lock = os.open("/opt/vdp/gnark", os.O_RDONLY)
    try: fcntl.flock(gnark_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        event("shim_lock", "error", failure="stage_error")
        raise Failure("confined Groth16 runner is busy", 75)
    event("shim_lock", "ok")
    event("shim_fifo", "begin")
    temporary = Path(tempfile.mkdtemp(prefix="vibeface-gnark.", dir="/tmp"))
    resource.setrlimit(resource.RLIMIT_STACK, (16384 * 1024, 16384 * 1024))
    os.mkfifo(temporary / "witness.wtns", 0o600)
    event("shim_fifo", "ok")
    inherited_fds = tuple(fd for fd in (proof_lock, diag_fd, gnark_lock) if fd is not None)
    commands = [("generator", ["/opt/vdp/bin/stark_verify", str(work / "input.json"), str(temporary / "witness.wtns")]),
                ("prover", ["/opt/vdp/bin/prover", "/opt/vdp/gnark/stark_verify.cs", "/opt/vdp/gnark/stark_verify_final.pk.dmp",
                            str(temporary / "witness.wtns"), str(temporary / "proof.json")])]
    for role, command in commands:
        event(role + "_spawn", "begin", worker=role)
        launching = True
        try:
            with (temporary / (role + ".stdout")).open("wb") as stdout, (temporary / (role + ".stderr")).open("wb") as stderr:
                try: process = subprocess.Popen(command, stdout=stdout, stderr=stderr, pass_fds=inherited_fds)
                except OSError as error:
                    event(role + "_spawn", "error", worker=role, errno=error.errno, failure="spawn")
                    raise Failure("confined Groth16 worker failed") from None
            workers[process.pid] = (role, process)
        finally: launching = False
        if pending_signal is not None: raise Interrupted(pending_signal)
        event(role + "_spawn", "ok", worker=role)
    event("shim_workers", "begin")
    while workers:
        pid, status = os.waitpid(-1, 0)
        success, role = collect(pid, status)
        if not success:
            event("shim_workers", "error", failure="stage_error")
            raise Failure("confined Groth16 worker failed")
    event("shim_workers", "ok")
    event("shim_commit", "begin")
    proof = temporary / "proof.json"
    if not proof.is_file() or proof.is_symlink(): raise Failure("confined Groth16 prover produced no proof")
    with proof.open("rb") as source:
        descriptor = os.open(work / "proof.json", os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "wb") as destination:
            while chunk := source.read(64 * 1024): destination.write(chunk)
            destination.flush()
            os.fsync(destination.fileno())
    event("shim_commit", "ok")

def interrupt(number, _frame):
    global pending_signal
    pending_signal = pending_signal or number
    if not launching: raise Interrupted(pending_signal)

code = 0
try:
    for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
        signal.signal(number, interrupt)
    run()
except Failure as error:
    if active_phase is not None: event(active_phase, "error", failure="stage_error")
    print(error.message, file=sys.stderr)
    code = error.code
except Interrupted as error:
    if active_phase is not None: event(active_phase, "error", signal_number=error.number, failure="worker_signal")
    code = 128 + error.number
except (OSError, ValueError) as error:
    event(active_phase or "shim_validate", "error", errno=getattr(error,"errno",None), failure="io" if isinstance(error,OSError) else "stage_error")
    print("confined Groth16 worker failed", file=sys.stderr)
    code = 1
finally:
    for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM): signal.signal(number, signal.SIG_IGN)
    cleanup()
    if gnark_lock is not None: os.close(gnark_lock)
sys.exit(code)
' "$@"
