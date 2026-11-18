# AUTO-15 verification harness implementation

## Current state

The three local acceptance entry points now consume one platform-neutral helper, [`scripts/verification_harness.py`](../scripts/verification_harness.py), for deadline polling, streaming SHA-256, atomic private JSON replacement, loopback port reservation, and owned-process wait/stop mechanics.

Each adapter still declares its own policy:

- `verify-node.py` retains its OSError/ValueError/AssertionError/HTTPException transient set, 35-second deadline, type-only timeout suffix, TCP+UDP allocation, and POSIX process-group signals.
- `verify-cli-client.py` retains propagation of predicate exceptions, its 45-second deadline, TCP+UDP allocation, and POSIX process-group signals.
- `verify-windows-node.py` retains its OSError/ValueError/AssertionError/URLError transient set, 0.2-second polling interval, full last-error timeout diagnostic, TCP-only allocation, and Win32 handle/PID identity cleanup.

The scripts add their own directory to `sys.path` before imports so direct execution and existing `runpy`/`importlib` loading continue to resolve the shared helper from arbitrary working directories. No Windows handle/PID logic or POSIX-vs-Windows process policy was moved into the shared module.

`check-source.py` includes the helper's offline self-test, and `scripts/ci-registry.toml` includes the helper in the existing `node` path group so the source cannot bypass that scope. `.preflight.toml` was not modified.

## Evidence

See [`evidence/verification-harness-implementation.log`](../evidence/verification-harness-implementation.log) for the exact command output from the current worktree. The proof records:

- helper self-test for deadline, private JSON mode 0600, stream hash, TCP-only and TCP+UDP reservations;
- Python bytecode compilation for all changed Python sources;
- `runpy.run_path` loading of all three verifiers from `/tmp`;
- policy probes showing Node and Windows timeout diagnostics while CLI predicate errors propagate immediately.

No application, external provider, Docker runtime, GUI/e2e test, installation, or remote mutation was run.
