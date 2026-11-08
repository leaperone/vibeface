# Artifact publication and service deployment

Product and website tags publish immutable artifacts only. Official services use the explicit commands in [service deployments](../packages/docs/service-deployments.md), selecting a complete published product tag. Deployment does not rebuild or republish product artifacts.

The request workflow needs `VIBEFACE_RELEASE_DISPATCH_TOKEN` with permission to dispatch to the deployment repository. Runtime credentials and target configuration stay in the deployment environment. An accepted request is not proof that deployment succeeded.
