# Background activity display name

The current self-signed release certificate has organization `kdnk` and no Team ID. The launchd service label is `git.kdnk.wm`, and the executable and embedded bundle name are `wm`.

macOS displays the signing organization for a legacy LaunchAgent that it cannot associate with an application. The `AssociatedBundleIdentifiers` key requires the helper and app to have matching Team IDs. See [Apple's service-name documentation](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos#Connect-services-to-app-names-in-System-Settings).

## Native checks

A signed `wm.app` using the existing certificate passed strict bundle verification and LaunchServices registration. The background item still displayed `kdnk`. The macOS diagnostic log reported:

```text
Bundle identifiers from launchd plist ignored because the executable doesn't have a Team ID.
```

An app-owned agent registration through `SMAppService` was also attempted with the same certificate. macOS rejected registration with `SMAppServiceErrorDomain`, code `1`, `Operation not permitted`. Changing the caller from a helper to the app's main executable did not resolve that failure. These results do not establish support for this signing identity through another registration mechanism.

Migrating a standalone executable into an app bundle also creates a separate Accessibility permission entry. Keeping the certificate and designated requirement unchanged did not preserve the existing standalone permission for the app bundle in the native check.

The standalone service and installation were restored after the checks. No release-signing identity, certificate pin, keychain, or trust setting was changed. The app-bundle release and installer changes were withdrawn because the native display-name check failed.

## Signing decisions

Changing the self-signed certificate's organization to `wm` would change the organization that macOS uses for the legacy background item. This is a signing-identity migration, not an executable rename. It changes the certificate pin and requires an explicit decision covering the backup, release-signing secrets, installer pin, and permission migration before generating or deploying a replacement certificate.

An Apple-issued signing identity with a Team ID offers the documented app-association path. That also requires an explicit signing-identity migration. A future implementation must prove the actual settings display and service behavior before changing the distribution format.
