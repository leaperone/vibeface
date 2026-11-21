"""Cgroup snapshots and OOM accounting for the bounded live proof sandbox.

This leaf does not import verification entry points or run acceptance scenarios.
"""

import re
from pathlib import Path


def resources():
    membership = next(line[3:] for line in Path("/proc/self/cgroup").read_text().splitlines() if line.startswith("0::"))
    cgroup = Path("/sys/fs/cgroup") / membership.lstrip("/")
    fields = {name: (cgroup / name).read_text().strip() for name in (
        "memory.max", "memory.swap.max", "memory.current", "memory.events", "pids.max", "pids.current")}
    if fields["memory.max"] != str(11 * 1024 ** 3) or fields["memory.swap.max"] != "0" or fields["pids.max"] != "64":
        raise ValueError("Live proof sandbox has incorrect kernel memory, swap or process limits")
    for name in ("memory.peak", "pids.peak"):
        if (cgroup / name).is_file():
            fields[name] = (cgroup / name).read_text().strip()
    return cgroup, fields


def oom_delta(initial, final):
    def counts(snapshot):
        values = {}
        for line in snapshot["memory.events"].splitlines():
            name, value = line.split()
            if name in values or not re.fullmatch(r"[0-9]+", value):
                raise ValueError("Invalid cgroup memory event counter")
            values[name] = int(value)
        if not {"oom", "oom_kill"}.issubset(values):
            raise ValueError("Required cgroup OOM counters are missing")
        return {name: values.get(name, 0) for name in ("oom", "oom_kill", "oom_group_kill")}
    before, after = counts(initial), counts(final)
    delta = {name: after[name] - before[name] for name in before}
    if any(value < 0 for value in delta.values()):
        raise ValueError("Cgroup OOM counters decreased during live acceptance")
    return delta
