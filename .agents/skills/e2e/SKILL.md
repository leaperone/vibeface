---
name: e2e
description: Write, run and debug Vibeface browser and native end-to-end tests with the installed e2e runner, or inspect its live UI through the project e2e MCP server.
---

# Vibeface e2e

Use the existing e2e runner and fixtures. Read `packages/docs/e2e.md` for the project commands, isolated Node setup, artifact paths and platform boundaries.

## Read the installed guide

The dependency ships version-matched instructions. Run the relevant guide from the repository root before the corresponding operation:

```sh
npx --no-install e2e guide <topic>
```

| Operation | Topic |
| --- | --- |
| Configure targets or start the app | `setup` |
| Write or change tests and fixtures | `writing-tests` |
| Run existing tests and read reports | `running` |
| Diagnose a failed test | `debugging` |
| Use live UI tools | `mcp` |
| Add model-driven steps | `agent` |
| Explore a flow | `explore` |
| Run a bug bash | `bug-bash` |

If dependencies are missing, install the repository's frozen dependencies using the setup in `packages/docs/e2e.md`. Do not silently download a newer runner or replace the project's config. The same guides are available under the installed `node_modules/e2e` package.

## Project constraints

- Reuse the configured tests and fixtures. Select the affected test by file or line while fixing it; read its recorded failure before another run. Do not mask failures by adding sleeps or retries.
- Use task-owned Nodes, directories and processes. Keep `workers: 1` and `retries: 0`; preserve other running Nodes, services and devices.
- Use the controlled Provider fixtures. Do not call a real model, read personal Agent credentials, or reuse a personal browser profile for CI acceptance.
- Keep credentials in e2e's declared secrets. Store large runner outputs and downloads in an ignored directory inside the task worktree; preserve compact evidence in the task planning directory.
- Browser, native mobile and real Provider results prove different things. Follow the repository's iOS runtime and task-owned simulator requirements before native acceptance.
- Reporting a runner problem to an external issue tracker requires explicit authorization. A failing application test is not automatically a runner bug.
