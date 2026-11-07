# iOS device build

The iOS device workflow builds a generic `arm64` iPhoneOS archive from the
same React Native app and Rust secure-native XCFramework used by the iOS
simulator build. It checks the committed release version, bundle identifier,
app version, and build number against the archive metadata.

The current `release-ios.yml` workflow is callable only. Without signing
secrets, it produces a source-bound
`vibeface-<releaseVersion>-ios-device-unsigned.xcarchive.zip`, checksum, and
provenance receipt. That archive is unsigned and for development inspection;
it is not an installable iPhone app or an IPA.

When all three `IOS_CERTIFICATE_P12_BASE64`, `IOS_CERTIFICATE_PASSWORD`, and
`IOS_PROVISIONING_PROFILE_BASE64` secrets are configured, the workflow checks
the profile's exact bundle ID, team, expiration, distribution entitlements,
and requested distribution method. It then signs the archive and exports
`vibeface-<releaseVersion>-ios.ipa`, with a code-signature/profile verified
checksum and provenance receipt. Partial signing configuration fails closed.
`distribution_method` defaults to `app-store-connect`; select `ad-hoc` only
with a profile that includes the intended registered devices. An App Store
Connect IPA is marked TestFlight eligible and requires a separate authorized
upload before users can install it. An Ad Hoc IPA is marked installable only
on devices registered in its profile.

Those iOS credentials are not currently configured. Existing Apple API key
material used for notarization and the macOS Developer ID certificate do not
provide iOS app distribution signing. No IPA is emitted when signing
credentials are absent. TestFlight or App Store upload is a separate,
explicitly authorized operation.
