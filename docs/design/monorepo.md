# Monorepo application boundaries

Vibeface separates the user interface, the backend process, and local process management so that the same web application can run on a public host or connect to a local Core.

## Target layout

The five applications have distinct responsibilities.

| Path | Responsibility |
| --- | --- |
| `apps/www` | Public website and documentation. |
| `apps/app` | Browser interface, built as static files for public and local use. |
| `apps/core` | Rust backend with storage, networking, scheduling, execution, and authenticated APIs. |
| `apps/cli` | Human and Agent commands, process startup, service installation, and updates. |
| `apps/mobile` | Native mobile application. |

Shared Rust libraries stay in `crates`. Shared TypeScript libraries stay in `packages`.

## Process boundaries

App does not contain or start Core. A browser connects to Core through HTTP and WebSocket endpoints with the existing authentication and Noise protocol. The public App serves the same interface as a local installation.

CLI manages a separate Core process. CLI does not link the Core runtime or open its SQLite database. Core owns its identity, configuration validation, and persistent business data. Local management reuses the owner-only Unix socket. One-shot initialization must also execute in Core.

Core can build and run without an App build. A complete local distribution provides both processes and the App's static files. Local browser access keeps its one-time capability exchange and fixed loopback origin.

## Migration contract

The split moves the existing applications and their callers together. Cargo and pnpm workspaces, build receipts, installers, release packages, update activation, CI, and verification scripts must all use the final paths. Old source paths do not remain as compatibility aliases.

This structural change preserves personal network isolation, command behavior, update leases, and reconciliation of writes with unknown results. It does not introduce a new relay protocol or change the data schema. Acceptance includes independent Core builds, CLI dependency checks, and real processes started from the complete package.
