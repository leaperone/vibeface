# Official service deployments

Core, App, and Website releases deploy independently after their complete `release.json` is published. The release workflow dispatches a purpose-specific deployment from `main` and waits for its actual terminal result. It does not rebuild dependencies or treat a successful dispatch as a successful deployment.

| Service | Exact tag | Entry |
| --- | --- | --- |
| Node | `core-v<version>` | `deploy-node.yml` |
| App | `app-v<version>` | `deploy-app.yml` |
| WWW | `www-v<version>` | `deploy-www.yml` |

Each entry defaults to `apply=false`, has a fixed target, and calls `deploy-services.yml`. Only `main` may apply. The common implementation retains the global host activation lock; independent release jobs do not cancel an activation already in progress. There is no Product/`all` deployment or shared service version.

The deployment planner verifies the final manifest, immutable tag source and green-main provenance, contracts, and GitHub asset digests. It downloads only the selected component, validates its internal receipt, then builds its container. Node activation first rehearses the previous complete Core release to candidate upgrade, preserves identity/data, and uses the existing maintenance, backup, journal, health, and recovery rules.

Host addresses and credentials stay in deployment environment secrets. Follow [service deployments](../packages/docs/service-deployments.md) for commands and verification. Manual dispatch responses with unknown outcomes must be reconciled against exact tag/intent runs before another write.
