# Static app deployment

Stable `app-v<major>.<minor>.<patch>` releases publish the verified app archive before requesting deployment in `leaperone/leaperone-releases`.
Prereleases do not request production deployment. The receiver uses the exact tag and source commit and checks HTTP entry points after deployment.

The producer requires `VIBEFACE_RELEASE_DISPATCH_TOKEN` with permission to dispatch to the release repository.
