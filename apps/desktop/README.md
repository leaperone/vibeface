# Vibeface Desktop (Electron)

Trusted launcher + sandboxed business window. Loads Core’s web UI (local listen **4791**); uses `--control` JSON with bundled `vibeface-core` (no token logging to Node logs).

## Layout

- `dist/main/index.cjs` — main process (esbuild CJS)
- `dist/preload/*.cjs` — narrow IPC bridges
- Packaged runtime: `resources/runtime/` → `VIBEFACE_DESKTOP_RUNTIME` in dev when unpackaged

## Environment

| Variable | Purpose |
|----------|---------|
| `VIBEFACE_DESKTOP_RUNTIME` | Directory with `vibeface-core`, `vibeface` CLI, `web/` |
| `VIBEFACE_DESKTOP_CORE` | Optional absolute path to `vibeface-core` binary |
| `VIBEFACE_DESKTOP_PROFILE_DIR` | Isolated settings dir (tests) |
| `VIBEFACE_DESKTOP_FAKE_CONTROL` | **Tests only:** Node script mimicking `--control` |
| `VIBEFACE_DESKTOP_FAKE_INSPECT_JSON` | **Tests only:** payload for fake inspect |

## Scripts (from repo root)

```bash
npm install --prefix apps/desktop --ignore-scripts --no-package-lock
npm run typecheck --prefix apps/desktop
npm run test --prefix apps/desktop
npm run build --prefix apps/desktop
npm run package --prefix apps/desktop   # needs scripts/build-desktop.mjs from delivery C
```

`package` requires `scripts/build-desktop.mjs` from worktree C. Runtime **electron@45.2.3**, **electron-builder**, **@electron/notarize**, **@electron/osx-sign** are delivery-owned pins in root **pnpm-lock.yaml**; this package lists intent in comments only to avoid a large local Electron download in B.

Note: Delivery C should add to the lockfile (exact pins): `electron@45.2.3`, `electron-builder@25.1.8`, `@electron/notarize@2.5.0`, `@electron/osx-sign@1.3.1`.

## Protocol handler

Registers `vibeface://` when packaged. Parses `vibeface://join?link=…` and `http(s)://…/join#vibeface1…` only; rejects `file:`.

## Requirements for Root

- Core flag `--desktop-parent-stdin`: parent holds stdin open; EOF triggers graceful shutdown (contract).
- Optional: guard so CLI-only bundles are not used as standalone Core for Electron-owned installs.
