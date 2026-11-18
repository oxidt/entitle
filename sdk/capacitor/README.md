# @oxidt/entitle-capacitor

Capacitor 8 plugin for [entitle](https://github.com/oxidt/entitle), the self-hostable
RevenueCat alternative: native **App Store (StoreKit 2)** and **Google Play Billing 8**
purchases of auto-renewable subscriptions and non-consumables, synced to your entitle server.

The plugin only runs the store flows. Entitlement state lives on the entitle server and is
read with [`@oxidt/entitle`](../js). No consumables.

## Install

```sh
bun add @oxidt/entitle @oxidt/entitle-capacitor
bunx cap sync
```

**iOS** (Swift Package Manager; iOS 15+): `cap sync` adds the package to `CapApp-SPM`. In Xcode,
add the **In-App Purchase** capability. In entitle, set the `app_store` credentials and point App
Store Server Notifications V2 at `/webhooks/apple/{project_id}` (see [`docs/api.md`](../../docs/api.md)).

**Android** (minSdk 24): nothing to add; the Play Billing library brings the `BILLING`
permission. In entitle, set the `play_store` credentials and the RTDN Pub/Sub push to
`/webhooks/google/{project_id}`. The entitle server acknowledges purchases after verifying them,
so the app must sync every purchase (the helpers do) or Google refunds it after 3 days.

Register each store product id (`app_store` / `play_store`) in entitle and attach it to an entitlement.

Step by step, including App Store Connect, Play Console and sandbox testing:
[`docs/store-setup.md`](../../docs/store-setup.md). A minimal example app:
[`examples/capacitor`](../../examples/capacitor).

## Usage

```ts
import { Capacitor } from '@capacitor/core';
import { Preferences } from '@capacitor/preferences';
import { Entitle } from '@oxidt/entitle';
import { EntitleStore, purchase, restorePurchases, startPurchaseSync } from '@oxidt/entitle-capacitor';

const entitle = await Entitle.configure({
  apiKey: 'pk_…',
  baseUrl: 'https://entitle.example.com',
  storage: {
    get: async (key) => (await Preferences.get({ key })).value,
    set: (key, value) => Preferences.set({ key, value }),
    remove: (key) => Preferences.remove({ key }),
  },
});
if (Capacitor.isNativePlatform()) startPurchaseSync(entitle); // once, at app start

const { products } = await EntitleStore.getProducts({ productIds: ['pro.monthly'] });
showPaywall(products); // title, price.formatted, subscriptionPeriod, (Android) offers

const result = await purchase(entitle, { productId: 'pro.monthly' });
if (result.status === 'pending') showMessage('Waiting for approval');
if (entitle.isEntitled('pro')) unlockPro();

// "Restore purchases" button:
await restorePurchases(entitle);
```

`purchase` returns `{ status: 'purchased', purchase, customerInfo } | { status: 'cancelled' } |
{ status: 'pending' }`. Sync failures reject with the SDK's `EntitleError`; `code:
"transfer_refused"` means the purchase belongs to another entitle customer (its store account).

## API

Helpers (use these):

| | |
|---|---|
| `purchase(entitle, { productId, offerToken? })` | account token → store purchase → `entitle.syncPurchase` → (iOS) finish, only after the sync succeeded |
| `restorePurchases(entitle)` | syncs every owned purchase; `CustomerInfo` |
| `startPurchaseSync(entitle, { onError? })` | syncs (+ finishes) renewals, Ask to Buy, other-device purchases, refunds, pending → purchased; returns unsubscribe |

A purchase that fails to sync stays unfinished on iOS; StoreKit delivers it again at the next
launch and `startPurchaseSync` retries it. Android purchases also reach entitle through RTDN.

Native layer (`EntitleStore`):

| | |
|---|---|
| `getProducts({ productIds })` | `{ products }`: title, description, `price` (`formatted`, `amountMinor`, `currency`), `kind`, `subscriptionPeriod` (ISO 8601), Android `offers` (base plan / offer tokens, pricing phases) |
| `purchase({ productId, accountToken, offerToken? })` | `{ status: 'purchased', purchase }` \| `cancelled` \| `pending`; never finishes / acknowledges |
| `finishTransaction({ transactionId })` | iOS; no-op on Android |
| `getCurrentPurchases()` | iOS `Transaction.currentEntitlements`, Android owned subs + one-time products |
| `addListener('purchaseUpdated', cb)` | purchases not started by `purchase()`; kept until a listener is added |

`purchase` is `{ store: 'app_store', transactionId, productId }` or
`{ store: 'play_store', purchaseToken, productId }`. On the web every method rejects with
`UNIMPLEMENTED`.

## Web checkout (Creem)

Creem needs no native code: open `(await entitle.createCheckout({ productId })).url` in the
browser (e.g. `@capacitor/browser`). On iOS in the EU, linking out to an external purchase
requires Apple's **External Purchase Link** entitlement (StoreKit External Purchase Link
Entitlement, EU); this plugin does not implement it.

## Development

```sh
(cd ../js && bun install && bun run build)   # the plugin builds against the local @oxidt/entitle
bun install
bun run typecheck && bun test test            # helpers, with the native plugin and fetch mocked
bun run build                                 # dist/ (ESM + .d.ts, CJS, IIFE)
xcodebuild test -scheme OxidtEntitleCapacitor -destination 'platform=iOS Simulator,name=iPhone 17'
cd android && ./gradlew assembleDebug         # needs JDK 21 + Android SDK (CI job `android`)
```

StoreKit purchase flows are tested by hand: `SKTestSession` needs an app host (a hostless SPM
test bundle gets no products). Select `ios/StoreKit/Products.storekit` as the StoreKit
configuration of an app scheme (Edit Scheme → Run → Options) and buy in the simulator; Xcode's
Transaction Manager shows whether the transaction was finished after the sync. This only
exercises the native layer: the entitle server looks transactions up via Apple's App Store Server
API, where local Xcode transactions do not exist, so they never sync (and stay unfinished). To test
the whole flow with entitle, use a sandbox account or TestFlight; see
[`docs/store-setup.md`](../../docs/store-setup.md).
