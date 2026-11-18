# @oxidt/entitle

Client SDK for [entitle](https://github.com/oxidt/entitle), the self-hostable
RevenueCat alternative. For web apps and Capacitor webviews, using a **public key
(`pk_…`)**. Zero dependencies; runs in browsers, Bun and Node ≥ 18 (Node 18 needs an
explicit `appUserId`: it has no global Web Crypto).

## Quick start (web checkout)

```ts
import { Entitle } from "@oxidt/entitle";

const entitle = await Entitle.configure({
  apiKey: "pk_…",
  baseUrl: "https://entitle.example.com",
  // appUserId: "user_42",  // omit: a persisted `$entitle_anon:<uuid>` id is used
});

await entitle.logIn("user_42");                   // after your own sign-in
entitle.onCustomerInfoUpdated((info) => render(info));
if (!entitle.isEntitled("pro")) {                 // sync, from the cache
  const { url } = await entitle.createCheckout({
    productId: "prod_…",
    successUrl: "https://example.com/thanks",     // must be in checkout_redirect_origins
  });
  location.href = url;                            // or open it in an in-app browser
}

// On /thanks (Creem appends ?checkout_id=ch_…): link it now instead of waiting for the webhook.
const checkoutId = new URLSearchParams(location.search).get("checkout_id");
if (checkoutId) await entitle.syncPurchase({ store: "creem", id: checkoutId });
```

**Native purchases (planned `@oxidt/entitle-capacitor`):** the plugin runs StoreKit 2 /
Play Billing with `await entitle.getAccountToken()` as `appAccountToken` /
obfuscated account id, then hands the result back via
`entitle.syncPurchase({ store: "app_store", id: transactionId })` (or
`{ store: "play_store", id: purchaseToken }`).

## API

| | |
|---|---|
| `Entitle.configure({ apiKey, baseUrl, appUserId?, storage?, fetch?, identityToken? })` | `Promise<Entitle>` |
| `appUserId`, `isAnonymous` | current app user id |
| `getCustomerInfo({ fresh? })` | cached 5 min (memory + storage); `fresh: true` bypasses |
| `isEntitled(id)`, `activeEntitlements` | sync reads of the cache |
| `getAccountToken()` | the customer's stable UUID (`customer_id`) |
| `logIn(appUserId)` | `{ customerInfo, created }` |
| `logOut()` | new anonymous id, cache cleared |
| `createCheckout({ productId, successUrl?, metadata? })` | `{ id, url }` (Creem) |
| `syncPurchase({ store, id })` | updated `CustomerInfo` |
| `onCustomerInfoUpdated(cb)` | fires when fetched info changes; returns unsubscribe |

Failures reject with `EntitleError { status, code, message }`: `code` is the server's
error code (e.g. `transfer_refused` for a 409 on `syncPurchase`), `network_error`
(status 0) or `unexpected_response`. See [`docs/api.md`](../../docs/api.md).

**Signed identity:** pass `identityToken: (appUserId) => fetchFromYourBackend(appUserId)`
returning a short-lived JWT your backend signs for its signed-in user (see
[Signed identity](../../docs/api.md#signed-identity)). Required once the project
enables `require_signed_identity`; anonymous ids never need one.

**Storage:** `localStorage` when usable, else in-memory. For Capacitor, pass
Preferences:

```ts
storage: {
  get: async (key) => (await Preferences.get({ key })).value,
  set: (key, value) => Preferences.set({ key, value }),
  remove: (key) => Preferences.remove({ key }),
}
```

## Development

```sh
bun install
bun run typecheck && bun test    # unit tests, mocked fetch
bun run build                    # dist/ (ESM + .d.ts)
bun run e2e                      # real server + Postgres via docker compose; tears both down
```

`src/generated/` holds the wire types, generated from the server's Rust types with
[ts-rs](https://github.com/Aleph-Alpha/ts-rs). Never edit them; regenerate from the
repo root after changing a JSON type:

```sh
cargo test -p entitle-server --lib export_bindings
```
