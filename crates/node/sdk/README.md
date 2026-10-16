# Codex SDK sidecar

`codex-manager.mjs` is a small JSON-lines adapter for the pinned
`@openai/codex-sdk` `0.163.0` package. The Rust manager starts this sidecar
with a persisted thread ID when resuming, or with a null ID for a fresh SDK
thread.

The first frame must be an `init` object. It contains `prompt`, `session_id`,
`cwd`, optional `model`, and `codex_path`, plus optional `mcp_servers`,
`tool_cli`, and `tool_env`. Paths are absolute and all fields have byte and
count limits. An omitted or empty model uses the account's configured default.
The sidecar emits one `ready` frame containing the requested `session_id` and
waits for an `ack` before invoking the model. For a fresh request, the SDK's
first `thread.started` event is converted to a `session` frame. The sidecar
waits for `{ "type": "session_ack", "session_id": "..." }` before forwarding
that event or any later event, so the Rust store can persist the native ID.

After the ACK, SDK events are forwarded as `{ "type": "event", "event": ... }`.
The terminal frame is `{ "type": "result", "session_id", "final_response", "usage" }`.
The control stream accepts `{ "type": "cancel", "session_id": "..." }` and
aborts the SDK run. EOF and SIGTERM also abort the run.
For a fresh thread, the native ID is learned only after the first SDK
submission begins. The caller must durably mark the operation as running before
sending the initial ACK; any failure after that point is uncertain and must not
be replayed automatically. `session_ack` only confirms persistence of the ID.
Validation failures and failed turns produce a bounded `error` frame. Secrets in
tool environments and inherited API-key variables are redacted from forwarded
frames. Stderr is diagnostic only and is never part of the protocol.

The sidecar uses the official TypeScript SDK, which wraps `codex exec
--experimental-json`. The SDK has no native custom-tool callback. `tool_cli` is
therefore available through the Codex shell tool with the `vibeface_manager`
permissions profile. That profile denies filesystem writes and requests
loopback network access for `127.0.0.1` and `localhost`. The sidecar disables
unified exec, child-agent features, web search, apps, plugins, and host skill
discovery. Codex's standalone exec path does not currently prove that an
external-domain allow-list is enforced, so the sidecar makes no such claim.
The SDK does not expose a command allow-list either. The prompt and CLI wrapper
remain responsible for restricting shell commands. Optional MCP servers are
accepted only when explicitly supplied in `init` and may use `enabled_tools`
allow-lists.

Install the pinned dependency from this directory with `npm ci`.

References:

- [Codex SDK guide](https://learn.chatgpt.com/docs/codex-sdk.md)
- [TypeScript SDK source](https://github.com/openai/codex/tree/rust-v0.163.0/sdk/typescript)
