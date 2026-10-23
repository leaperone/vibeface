# Vibeface Nightly

Install Node.js 22 or newer, then install the Nightly CLI:

```sh
npm install -g @leaperone/vibeface@nightly
vibeface --version
vibeface
```

Supports macOS Apple Silicon and Linux x64/ARM64 with glibc. The package contains the native Rust programs, embedded Web client and service scripts. Installation does not download executable code from another host, run a postinstall script, initialize node data or start a service.

After initialization, stop the foreground node before `vibeface service install`. When migrating a service, wait for idle and stop it through the previous installation first. Installation refuses an active updater, a data directory still locked by a running node, or a pending update. It holds the shared update lock until service installation finishes. If `update-pending.json` exists, run `update apply` through the previous managed installation to recover before migrating. Use a persistent global npm installation for a background service. Keep Node.js and this npm installation available; do not bind a service to a temporary `npx` cache.

npm owns updates for this installation. The Rust self-updater is disabled. Before upgrading a background node, wait for active operations to finish and resolve unknown results, then stop its service:

```sh
vibeface service stop
npm install -g @leaperone/vibeface@nightly
```

Then start the service and check its status:

```sh
vibeface service start
vibeface status
```

An upgrade preserves data but does not convert an incompatible database. Different data epochs require a fresh data directory. Removing the npm package does not delete node data. Uninstall a background service with `vibeface service uninstall` before `npm uninstall -g @leaperone/vibeface`.

Provider runtimes and credentials are configured separately. See [installation](https://github.com/leaperone/vibeface/blob/main/docs/install.md) and [releases](https://github.com/leaperone/vibeface/releases).
