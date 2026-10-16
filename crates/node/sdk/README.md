# Codex SDK sidecar

`codex-manager.mjs` is a small JSON-lines adapter for the pinned
`@openai/codex-sdk` `0.163.0` package. The Rust manager creates a native Codex
thread through the local app-server when needed, then starts this sidecar with
that already persisted thread ID.

The first frame must be an `init` object. It contains `prompt`, `session_id`,
`cwd`, optional `model`, and `codex_path`, plus optional `mcp_servers`,
`tool_cli`, and `tool_env`. Paths are absolute and all fields have byte and
count limits. An omitted or empty model uses the account's configured default.
The sidecar emits one `ready` frame containing the same `session_id`, then waits
for `{ "type": "ack", "session_id": "..." }` before invoking the model. A
missing session ID is rejected before the SDK is constructed, so a fresh SDK
thread cannot send a prompt before the Rust store persists its ID.

After the ACK, SDK events are forwarded as `{ "type": "event", "event": ... }`.
The terminal frame is `{ "type": "result", "session_id", "final_response", "usage" }`.
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
