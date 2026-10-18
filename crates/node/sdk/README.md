# Codex SDK sidecar

`codex-manager.mjs` adapts the pinned `@openai/codex-sdk` `0.163.0` package to
Node's JSON-lines manager protocol. The trusted execution image contains Node.js
18 or newer, this sidecar, the locked npm dependencies, and the Codex CLI. Build
the image from the same commit as Node; see [execution images](../../../docs/execution-image.md).
Host SDK paths and Node tool callback credentials are not passed to the manager.

The initial frame contains `type: "init"`, `prompt`, `session_id`, `cwd`,
`codex_path`, optional `model`, and `tools`. `tools` is one bounded object mapping
tool names to `{ description, inputSchema }`. Node sends only the tools allowed
for this invocation. An empty object disables tools. The sidecar rejects the old
`tool_cli`, `tool_env`, and `custom_tools` fields. Paths are absolute; fields,
frames, results, and the total event stream have size limits.

The sidecar emits `ready` with the requested native session ID and waits for
`ack` before invoking the SDK. For a new thread, the first `thread.started` event
produces a `session` frame. Node must durably save that native ID and respond with
`{ "type": "session_ack", "session_id": "..." }` before events continue.
Node marks the manager invocation running before the initial ACK. Failures after
that boundary remain uncertain and cannot be retried as new work.

SDK events are forwarded as `{ "type": "event", "event": ... }`. The terminal
frame contains `type: "result"`, `session_id`, `final_response`, and `usage`.
Item events are snapshots: an `agent_message` update replaces text for its
`item_id`. It must not append the same text repeatedly. Node adds conversation,
invocation, attempt, source, and sequence identities to client progress. Temporary
progress is separate from durable conversation history.

The official SDK wraps `codex exec --experimental-json` and has no native custom
tool callback. The sidecar therefore exposes its own CLI through the Codex shell:

```sh
/path/to/codex-manager.mjs tool nodes --json '{}'
/path/to/codex-manager.mjs tool command --call-ref 'intent-123' --json '{"command":"pwd"}'
```

The CLI connects to `VIBEFACE_MANAGER_TOOL_SOCKET`, a private Unix socket created
by this sidecar. It forwards `{ name, arguments }`. The sidecar emits a
`tool_call` frame with a fresh transport `call_id`; Node replies with a
`tool_result` bearing that same `call_id`. The transport ID is independent of
the durable intent reference. The CLI places `--call-ref` in `arguments.call_ref`
and rejects conflicting values. Every write tool requires a stable reference.
After a lost result, query `call_lookup` with that reference before retrying the
same intent. Node performs persistent deduplication and authorization.

The sidecar displays schemas from that same `tools` object and rejects names
outside it. Read-only result callbacks receive a read-only catalog, and Node
also enforces the restriction at admission. Prompts and schemas are not security
boundaries. The configured container, scoped mounts, network policy, Node grants,
and execution leases enforce access. Provider access uses the configured trusted
model gateway. Node callback secrets are not available to SDK shell commands.

Control frames accept `cancel` only for the current native session. EOF, SIGTERM,
and SIGINT abort the run as well. The sidecar observes its managed Codex process,
cleans up listeners, and closes the private tool channel. It reports an uncertain
error with `confirmation=managed_cli_exited` after cancellation: CLI exit alone
cannot prove that detached tool processes or external side effects stopped.
Node retains the unknown state until its execution supervision and reconciliation
supply the required evidence. Cancellation never grants an automatic replay.

Capabilities distinguish cancellation requests from confirmed cancellation and
native session resume from provider-side run reconciliation. `verified=false`
means the configured provider and model have not passed current acceptance.
No source or syntax check proves a real model run.

Release acceptance must compare `BUILD-INFO.json.source_commit`, binary, web
JS/WASM, sidecar, and execution image against the same clean commit. A package
made with `--skip-build` or `source_commit=null` is not that evidence.

References:

- [Codex SDK guide](https://learn.chatgpt.com/docs/codex-sdk.md)
- [Pinned SDK source](https://github.com/openai/codex/tree/rust-v0.163.0/sdk/typescript)
