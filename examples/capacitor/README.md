# entitle Capacitor example

A minimal Capacitor 8 app (no framework) that buys one product through
[`@oxidt/entitle-capacitor`](../../sdk/capacitor) and shows whether the entitlement is active.
It shows the app user id and has Buy and Restore purchases buttons, plus a log of results and errors
(`pending`, `transfer_refused`, …).

Before a purchase can work end to end, set up entitle and the stores:
[`docs/store-setup.md`](../../docs/store-setup.md).

## Configure

- `src/config.ts`: server URL, `pk_` key, product id (default `quest.example.full`), entitlement
  (default `full_game`). The server must be reachable from the device over HTTPS.
- `capacitor.config.json`: `appId` must be your bundle id / package name, the one registered
  in App Store Connect, Play Console and the entitle store credentials.

## Build

```sh
bun install
bun run typecheck
bun run build            # bundles index.html + src/ into www/
bunx cap add ios         # once; generates ios/ (not committed)
bunx cap add android     # once; generates android/ (not committed)
bun run sync             # build + cap sync, after every change
```

## iOS

1. `bunx cap open ios`.
2. In Xcode, select the App target → Signing & Capabilities: set your team, then
   **+ Capability → In-App Purchase**.
3. Run on a device (or simulator) and buy with a sandbox tester account, or ship a TestFlight build.

Do not use a local StoreKit configuration file (`.storekit`) for this: those transactions are unknown
to Apple's servers, so entitle cannot verify them (see the pitfalls in `docs/store-setup.md`).

## Android

1. `bunx cap open android`, or build a release bundle there.
2. Upload a signed build with the same package name to an internal testing track in Play Console;
   billing does not work for an app that was never uploaded.
3. Install it on a device signed in with a license tester account (from the testing track's
   opt-in link) and buy. Later builds with the same package name can usually be run directly
   (`bunx cap run android`).

## Not covered

The web build runs in a browser, but store purchases need iOS or Android; the Buy button stays
disabled there. Creem web checkout is not part of this example.
