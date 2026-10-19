# Codex SDK sidecar

`codex-manager.mjs` is a small JSON-lines adapter for the pinned
`@openai/codex-sdk` `0.163.0` package. The Rust manager starts this sidecar
with a persisted thread ID when resuming, or with a null ID for a fresh SDK
thread.

Run it with Node.js 18 or newer and an authenticated Codex CLI. Install the
pinned dependency from this directory with `npm ci`, or use `npm ci --omit=dev`
in a release package's `sidecars/` directory. Set absolute runtime and sidecar
paths in the Node configuration when using a background service. Credentials,
the Codex CLI, and npm dependencies are not included in the archive.

The first frame must be an `init` object. It contains `prompt`, `session_id`,
`cwd`, optional `model`, and `codex_path`, plus optional `tool_cli` and
`tool_env`, and `read_only`, which defaults to `false`. Paths are absolute.
All fields have byte and count limits. An omitted or empty model uses the
account's configured default.
The sidecar emits one `ready` frame containing the requested `session_id` and
waits for an `ack` before invoking the model. For a fresh request, the SDK's
first `thread.started` event is converted to a `session` frame. The sidecar
waits for `{ "type": "session_ack", "session_id": "..." }` before forwarding
that event or any later event, so the Rust store can persist the native ID.

After the ACK, SDK events are forwarded as `{ "type": "event", "event": ... }`.
The terminal frame is `{ "type": "result", "session_id", "final_response", "usage" }`.
The control stream accepts `{ "type": "cancel", "session_id": "..." }` and
aborts the SDK run. The session ID must match the current native thread. EOF
and SIGTERM also abort the run.
For a fresh thread, the native ID is learned only after the first SDK
submission begins. The caller must durably mark the operation as running before
sending the initial ACK; any failure after that point is uncertain and must not
be replayed automatically. `session_ack` only confirms persistence of the ID.
Validation failures and failed turns produce a bounded `error` frame. Secrets in
tool environments and inherited API-key variables are redacted from forwarded
frames. Stderr is diagnostic only and is never part of the protocol.

The Rust adapter converts native item updates into `text_snapshot`, `status`,
and `tool` progress. A Codex `agent_message` update contains the current text
for its `item_id`; replace that item's text instead of appending it. The Node
adds conversation, trigger, attempt, source, and sequence identities before
forwarding progress to the client. Native SDK events are not the browser
protocol, and temporary progress is not durable conversation history.

Cancellation confirms less than normal completion. The pinned CLI can start
tools in separate process groups, so exiting the sidecar and CLI does not
prove that every tool process has exited. After a cancellation request, this
sidecar reports an uncertain `error` with `confirmation=managed_cli_exited`.
The real adapter therefore keeps the manager job `unknown`, rather than
claiming that cancellation succeeded. Node tool credentials are fenced and
admitted tools are accounted for separately. Already dispatched background
tasks and external side effects retain their original task and native session
identities.
An uncertain invocation must not be retried under a new ID.

Manager capabilities advertise streaming, cancellation requests, and local
thread resume. Native steer and Provider run reconciliation are unavailable.
`cancel_confirmation=false` records the process limit above; `verified=false`
does not assert that the configured account and model have passed acceptance.
Cursor SDK uses its separate native callback adapter and supports deltas as
well as snapshots. Its cancellation also needs matching terminal or managed
process evidence.

The sidecar uses the official TypeScript SDK, which wraps `codex exec
--experimental-json`. The SDK has no native custom-tool callback. `tool_cli` is
therefore available through the Codex shell tool with the `vibeface_manager`
permissions profile. Code mode, its host, and unified exec are enabled so the
model can call that CLI. Normal invocations have host filesystem write access,
network access, and explicit access to `VIBEFACE_CODEX_SOCKET`. They can work in
the configured directory with shell and file tools. `read_only=true` keeps
filesystem writes disabled, retains the limited network profile, and exposes
only the scoped read tools supplied by the Node. There is no shell command
allow-list. Prompt instructions and scoped CLI credentials do not isolate an
untrusted model or plugin.

The Codex child process and its shell use explicit environment-variable lists.
Normal calls pass every supplied `tool_env` entry to the child and shell, including
`VIBEFACE_CODEX_SOCKET`, `VIBEFACE_NODE_TRIGGER`, and
`VIBEFACE_MANAGER_ATTEMPT`. Provider credential variables stay outside the shell
list. Cursor SDK receives the same tool environment in its actual bridge
process. Normal Cursor calls use its default host toolset. Read-only Cursor
calls keep the MCP-only tool selection.
Read-only calls omit `VIBEFACE_CODEX_SOCKET`; use the scoped Node status tools
to observe existing work. The Codex read-only profile enforces filesystem
restrictions, but does not isolate arbitrary Unix socket access on macOS.
The sidecar disables child-agent features, web search, apps, plugins, and host
skill discovery. Native App Server requests retain their own approval policy.
Neither the sidecar nor the native client answers those approvals automatically.

`vibeface codex METHOD --json 'JSON_OBJECT' --socket /absolute/codex.sock`
sends the native method and parameters without translating them into Node
commands. `--socket` can be omitted when `VIBEFACE_CODEX_SOCKET` is set. `--watch`
keeps the connection open and prints native notifications as JSON lines.
To answer a pending native request, provide an explicit stdin line such as
`{"id":27,"result":{"decision":"decline"}}`. The response must match a request
on that connection. Closing the watcher does not interrupt background work.
The CLI does not retry a method after disconnect or an uncertain result.

After starting native work, call the Node `observe_session` tool with its exact
thread and turn IDs. The coordinating SDK's final response does not mark that
background work complete. The observer reads current metadata, subscribes only
to threads already loaded on the configured Socket, and attempts read-only
history queries to recover missed progress. The installed Codex `0.163.0`
backend can reject resume and turn pagination with `list_turns is not supported
yet`; live broadcasts and the exact turn cache remain usable on that connection.
An observer connection can answer a broadcast approval and interrupt the
original turn after the caller's short CLI connection has closed. The Node
still requires a human response and the original task, thread, and turn match.
A `notLoaded` thread remains unconfirmed; historical turns do not prove another
stdio process is controllable or stopped.

Select a model through `manager.model` that the configured account supports.
An empty model uses the Codex account default. `auto` or a fast model name does
not guarantee immediate output. The 2026-10-18 direct SDK probes measured about
7 to 10 seconds to the first model output across Codex and Cursor, with two
turns per provider. Those probes verified model responses and native session
continuity; they did not verify the full Node, browser, and background-task
path.

For release acceptance, build the web assets, Rust binary, and sidecar from
the same clean commit. Check `BUILD-INFO.json.source_commit` against that
commit and verify the package checksum. A package made with `--skip-build` or
with `source_commit=null` is not evidence that the running artifact matches
the source. See [Node deployment and use](../../../docs/node.md).

References:

- [Codex SDK guide](https://learn.chatgpt.com/docs/codex-sdk.md)
- [TypeScript SDK source](https://github.com/openai/codex/tree/rust-v0.163.0/sdk/typescript)
