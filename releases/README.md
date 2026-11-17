# Independent releases

Each release owns one version and one immutable `<app>-v<version>` tag. The applications are `core`, `app`, `cli`, `desktop`, `mobile`, and `www`. npm distributes the CLI and uses its version. Shared Rust and JavaScript packages are source inputs, not separate public products.

Core and App publish independently and deploy only their own service. CLI bundles exact published Core and App archives. Desktop bundles an exact published CLI runtime. Mobile builds its own shared client/native SDK inputs. Website publication never waits for another application.

## Manifest and dependency pins

Each public release writes `release.json` last, after validating all required assets for that application. Its closed schema contains `schema`, `app`, `version`, `tag`, `source_commit`, `channel`, `contracts`, `dependencies`, and `assets`. `contracts` contains `data_epoch` and `update_protocol`, both currently 1. An asset contains `name`, `size`, and `sha256`.

A dependency pin contains exactly `tag`, `version`, `source_commit`, and `manifest_sha256`. `releases/cli.json` in a release source contains `core` and `app` pins; `releases/desktop.json` contains a `cli` pin. The coordinator adds these files only to the generated release commit. An installed bundle's nine-field `BUILD-INFO.json` records the actual CLI, Core, and App versions and source commits. Those identities may differ and must match the enclosing release's pins.

A consumer downloads an exact release manifest, verifies its hash and immutable tag, then verifies the selected archive against that manifest. It never selects a dependency by an unpinned `latest` URL. The CLI remains `scripts/release.py`; its eight production owners and offline tests are documented in [release module boundaries](../packages/docs/design/release-modules.md). Exact source generation belongs to `release_source.py`, while `hourly.py` selects changed applications and coordinates dispatch. Host activation remains in the deployment scripts.

## Automated publication

The coordinator selects a successful main push CI source. It schedules only applications whose source inputs or exact dependency pins changed. Core, App, Mobile, and WWW do not wait for CLI or Desktop. Publication wakes the coordinator again so consumers can use newly published dependencies without blocking an upstream workflow. A missing or failed dependency blocks only its consumers. Immutable release tags are never moved, and uploads never replace published bytes.

Independent versions use the existing `0.0.1` prerelease baseline. Main retains its fixed development versions. Generated Hourly commits change only the selected application's version, its Cargo lock entry or Mobile build numbers when applicable, and its dependency pins. Each application has its own publication concurrency key.

## Installation and activation

The CLI updater follows only `cli-v` releases. It checks the selected platform's archive and exact Core/App pins before staging the complete bundle. npm remains npm-owned. Existing maintenance leases, data snapshots, activation journals, health checks, and recovery behavior stay in place. There are no legacy `v` tags, compatibility aliases, or fallback manifest formats in the new consumers. A data epoch mismatch is refused.

Platform signing remains part of that platform's release. An unsigned iOS device archive is a build artifact, not an installable App or a TestFlight submission.

## Design choice

We considered a universal signed cohort/channel registry. It would introduce a central release gate, signing-key operations, and another artifact service. Per-application dependency pins provide reproducible packages using the existing GitHub release trust boundary and keep unrelated platforms independent. We retain one CLI/npm identity because npm is a distribution of the same CLI bundle.
