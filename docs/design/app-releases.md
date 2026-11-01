# App-scoped releases

Release tags name one application: `www-v`, `app-v`, `core-v`, `cli-v`, or `mobile-v`, followed by its version. Stable versions use `X.Y.Z`; Nightly versions retain `X.Y.Z-nightly.YYYYMMDD.N` with a UTC preparation date and per-app sequence.

Core and CLI own their Cargo package versions. App and www own their package.json versions. Mobile owns its Expo marketing version and release version. A release tag must match its application's declared version and an immutable commit already in main.

CLI distributions contain an explicitly pinned Core and App. The source lock selects immutable component tags; assembly checks each tag against the component release's full source SHA and records those SHAs in the final bundle manifest. This avoids a self-referential future merge SHA during the first release. App artifacts never contain Core.

Ordinary CI does not publish or deploy. Each prefixed tag builds and publishes only its target application. Mobile Nightly is a preview distribution, not an App Store promotion. Existing unprefixed tags remain historical; new installers discover only CLI-prefixed releases.
