# Artifact publication and service deployment

Product and website tags publish immutable artifacts only. Official services use the explicit commands in [service deployments](../packages/docs/service-deployments.md), selecting a complete published release. Deployment does not rebuild or republish product artifacts.

All commands, container definitions and workflows live in this repository. Each deployment target uses a GitHub Environment with scoped SSH credentials and runtime configuration. No cross-repository dispatch token or shared release repository is used. Starting a workflow is not proof that deployment succeeded.
