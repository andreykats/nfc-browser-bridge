# To do

Work planned for later. Move an item out of this file once it's done.

## Sign and notarize the macOS app

**Why:** releases are signed only "ad hoc" today, so macOS treats the app as
coming from an unknown developer. On a new Mac the first launch is blocked until
someone clicks **Open Anyway** in System Settings → Privacy & Security, and a
DMG downloaded by hand is blocked the same way. Updates installed from the
bridge's menu aren't affected. With a Developer ID signature and Apple's
notarization, macOS opens the app without any warning.

**Steps for the Apple account holder** (Apple team `S39JCV5G6C`, the same team
as Working Memory):

1. Create a **Developer ID Application** certificate. In Xcode, go to Settings →
   Accounts, select the team, click **Manage Certificates…**, then **+** →
   **Developer ID Application**. Only the account holder can do this. It is a
   different certificate from the ones Working Memory uses for the App Store.
2. Export it from Keychain Access (**My Certificates** → right-click →
   **Export…**) as a `.p12` file with a password.
3. In App Store Connect, create an API key for notarization: Users and Access →
   Integrations → App Store Connect API → **Team Keys** → **+**, with the
   **Developer** role. It must be a team key, because Apple says individual
   keys can't notarize. Download the `.p8` file (it can only be downloaded
   once), and note the **Key ID** and the **Issuer ID**.

**Then in this repo:**

- [ ] Store the certificate, its password, the API key, Key ID and Issuer ID as
      GitHub repository secrets with `gh secret set`, reading straight from the
      files so the values aren't printed anywhere.
- [ ] `packaging/macos/bundle.sh`: when a signing identity is given, sign the
      app with it using the hardened runtime and a secure timestamp
      (`codesign --force --options runtime --timestamp --sign …`, not the
      deprecated `--deep`). Local builds keep the ad hoc signature.
- [ ] Release workflow, macOS job:
  - import the `.p12` into a temporary keychain
  - build and sign the app, then build the DMG and sign it too
  - submit the DMG with `xcrun notarytool submit … --key --key-id --issuer --wait`
    and check the notarization log for warnings
  - staple the ticket with `xcrun stapler staple` on the DMG
  - fail the release if the signing secrets are missing, like the minisign step does
- [ ] Before releasing, sign a build on a Mac and check it with
      `codesign -vvv --deep --strict`, `spctl -vvv --assess --type exec` and
      `codesign -dvv` (look for a `Timestamp` line). Then test it with an
      ACR1255U-J1 and an ACR1552: tag reads, the USB reset
      (`cargo test -p bridge-core --test hardware -- --ignored`), copying the
      system log, and an update from the menu. The hardened runtime is stricter,
      so confirm nothing it controls is blocked.
- [ ] README: drop the **Open Anyway** step from the macOS install
      instructions.

Apple's documentation:
[notarizing](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution),
[custom notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow),
[common issues](https://developer.apple.com/documentation/security/resolving-common-notarization-issues),
[API keys](https://developer.apple.com/documentation/appstoreconnectapi/creating-api-keys-for-app-store-connect-api),
[Developer ID certificates](https://developer.apple.com/help/account/certificates/create-developer-id-certificates/).
The `developer.apple.com/documentation` pages only render in a browser. For a
script, fetch `https://developer.apple.com/tutorials/data/documentation/<path>.json`.

The Windows installer is unsigned too, so SmartScreen warns on a first install.
Signing it needs a separate code-signing certificate; that's a separate
decision, not part of this item.
