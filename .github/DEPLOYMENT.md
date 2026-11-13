# Artifact publication and service deployment

Product and website tags publish immutable artifacts. When a complete Release is published, the corresponding release workflow dispatches the matching service deployment from `main`; the deployment consumes that exact published release and does not rebuild or republish product artifacts.

All commands, container definitions and workflows live in this repository. Each deployment target uses a GitHub Environment with scoped SSH credentials and runtime configuration. No cross-repository dispatch token or shared release repository is used. Starting a workflow is not proof that deployment succeeded.

Automatic dispatch covers product Releases with `deploy-product.yml` (Node and App) and website Releases with `deploy-www.yml`. Manual entries remain fixed by target: `deploy-node.yml`, `deploy-app.yml`, `deploy-www.yml`, and `deploy-product.yml`. They require an exact published `tag` and default to `apply=false` for read-only validation. All entries call `deploy-services.yml`, which owns the single global deployment lock. New entries become dispatchable after they reach the default branch.
