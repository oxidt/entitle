# entitle HTTP API

This is the contract the JS/npm SDK is built against. All bodies are JSON, all
timestamps are RFC 3339 in UTC (`2026-10-01T12:00:00Z`), all ids are UUIDs
unless noted.

| Prefix | Auth | For |
|---|---|---|
| `/v1` | `Authorization: Bearer pk_…` or `sk_…` | apps, web frontends (public key) and developer backends (secret key) |
| `/v1` (marked **secret**) | `Authorization: Bearer sk_…` | developer backends only |
| `/admin/v1` | `Authorization: Bearer <ENTITLE_ADMIN_TOKEN>` | the instance operator |
| `/webhooks` | store signatures (verified by adapters) | Creem, Apple, Google |
| `/healthz` | none | load balancers (`200 ok` when the DB answers) |
| `/version` | none | operators: `{"version": "0.1.0", "commit": "<git sha>"}` (`commit` is `null` outside the image build) |

CORS: `/v1` answers any origin (`Access-Control-Allow-Origin: *`, no credentials, preflight cached for a day), so browsers and Capacitor webviews can call it directly; `/admin/v1`, `/webhooks` and `/healthz` send no CORS headers.

## Errors

Every non-2xx response has this body:

```json
{ "error": { "code": "not_found", "message": "customer not found" } }
```

| Status | `code` | When |
|---|---|---|
| 400 | `bad_request` | invalid input: malformed JSON body or wrong field types, missing/wrong `Content-Type`, bad path parameter (e.g. a non-UUID project id), bad app user id, unknown store, `success_url` origin not allowed, … |
| 400 | `malformed_payload` | the store payload could not be parsed |
| 401 | `unauthorized` | missing, unknown or revoked key/token |
| 401 | `invalid_signature` | store webhook signature did not verify |
| 403 | `forbidden` | public key used on a secret-only endpoint |
| 404 | `not_found` | resource or route does not exist, or the store is not configured for the project |
| 404 | `store_not_found` | the store does not know the submitted transaction id / token |
| 405 | `method_not_allowed` | the route exists but not for this HTTP method |
| 409 | `conflict` | duplicate product / entitlement, concurrent login race (retry) |
| 409 | `transfer_refused` | `/sync` of a purchase whose store account token belongs to another customer |
| 413 | `payload_too_large` | request body over 2 MB |
| 422 | `store_credentials` | the project's store credentials were rejected |
| 429 | `rate_limited` | over a request budget (see "Request budgets"); retry after `Retry-After` seconds |
| 501 | `not_implemented` | the store adapter is not implemented yet |
| 502 | `store_unavailable` | the store API failed; retry later |
| 500 | `internal` | bug or database failure |

## App user ids

- Any string of 1–256 bytes chosen by the developer (`user_42`), or
- an SDK-generated anonymous id: `$entitle_anon:<uuid v4>`.

Use them URL-encoded in paths (`$` and `:` are allowed as is).
A customer is created on first use of an app user id.

## CustomerInfo

Returned by most `/v1` endpoints.

```json
{
  "customer_id": "0b6f4c1e-5a0c-4a53-9d0e-2a1f7f0e9c11",
  "app_user_id": "user_42",
  "aliases": ["$entitle_anon:6f1d…", "user_42"],
  "entitlements": {
    "pro": {
      "identifier": "pro",
      "source": "app_store",
      "product_id": "com.example.pro.monthly",
      "purchased_at": "2026-09-01T10:00:00Z",
      "expires_at": "2026-10-01T10:00:00Z",
      "period_type": "normal",
      "will_renew": true,
      "billing_issue": false,
      "environment": "production"
    }
  },
  "purchases": [
    {
      "store": "app_store",
      "environment": "production",
      "product_id": "com.example.pro.monthly",
      "original_transaction_id": "2000000123456789",
      "store_transaction_id": "2000000134567890",
      "kind": "subscription",
      "status": "active",
      "period_type": "normal",
      "purchased_at": "2026-09-01T10:00:00Z",
      "expires_at": "2026-10-01T10:00:00Z",
      "grace_period_expires_at": null,
      "auto_renew": true,
      "price": { "amount_minor": 499, "currency": "EUR" }
    }
  ],
  "management_url": "https://apps.apple.com/account/subscriptions",
  "first_seen_at": "2026-08-30T08:00:00Z",
  "request_date": "2026-09-29T12:00:00Z"
}
```

- `customer_id` is stable and is the **account token**: pass it as StoreKit 2
  `appAccountToken` and as Google Play `obfuscatedAccountId` (`setObfuscatedAccountId`)
  so store notifications can be linked to the customer. Creem checkouts carry it
  automatically.
- **Secret key only:** `aliases` and the purchases' `original_transaction_id` /
  `store_transaction_id` are omitted from responses to a **public** key. The public
  key ships in apps, so anyone can call with any app user id; aliases (anonymous ids)
  and transaction ids (for Google: the purchase token) would let them act as that
  customer. Use app user ids that cannot be guessed.
- `entitlements` contains **only active** entitlements. Absent key = no access.
  - `source`: `creem` | `app_store` | `play_store` | `promotional`
  - `expires_at`: `null` = lifetime.
  - `product_id`, `period_type`, `environment`: `null` for promotional grants.
  - `billing_issue`: `true` while in a billing grace period (access continues).
- `purchases`: all purchases ever linked to the customer, newest first.
  - `kind`: `subscription` | `one_time`
  - `status`: `active` | `cancelled` (auto-renew off, still valid until `expires_at`)
    | `grace_period` | `billing_retry` | `paused` | `expired` | `refunded` | `revoked`
  - `period_type`: `trial` | `intro` | `normal`
  - `environment`: `sandbox` | `production`
  - `price`: `null` when the store does not report it. `amount_minor` is in the
    currency's ISO 4217 minor unit (cents for EUR, yen for JPY, fils for KWD).
- `management_url`: store page for managing the latest active subscription;
  `null` for Creem (use `POST …/portal` from your backend) or when nothing is active.

### Entitlement rules

- A purchase grants every entitlement its product is attached to.
- `active` and `cancelled` grant until `expires_at`; `grace_period` until
  `grace_period_expires_at` (else `expires_at`); `billing_retry`, `paused`,
  `expired`, `refunded`, `revoked` grant nothing.
- One-time purchases grant forever unless refunded/revoked.
- Sandbox purchases grant nothing unless the project's `sandbox_grants_entitlements` is on
  (off by default).
- Several sources for one entitlement: the latest expiry wins (lifetime beats any
  date). Entitlements are computed at request time, so expiry needs no job.

## SDK API (`/v1`, public or secret key)

### Signed identity

The public key ships in apps, so by itself it lets anyone act as any app user id.
To prevent that, your backend proves identified ids with a short-lived **HS256 JWT**
signed with the project's identity secret (`POST /admin/v1/projects/{id}/identity-secret`):

```json
{ "sub": "user_42", "aud": "entitle:<project_id>", "exp": 1760000000 }
```

- Send it as `X-Entitle-Identity: <jwt>` for the `{app_user_id}` in the path
  (GET, `login`, `sync`, `checkout`), and as `identity_token` in the `login` body for
  `new_app_user_id`. Only mint it for your authenticated user; keep `exp` short (minutes).
- Tokens are checked whenever sent (403 `forbidden` if invalid, expired, or for
  another id). With the project's `require_signed_identity` on, identified ids
  **need** one (403 otherwise).
- Not needed: anonymous `$entitle_anon:` ids (random and never returned to public
  keys) and secret-key calls. Exception: with `require_signed_identity` on, an
  anonymous id that has been logged in (an alias of an identified customer) needs a
  token whose `sub` is one of that customer's identified ids (403 otherwise). The
  SDK switches to the identified id after `logIn`, so this only affects stale ids.
- Rotating the secret invalidates all tokens signed with the old one.
- `require_signed_identity` is off by default, which keeps apps without a backend
  working. If your users sign in with a backend, **turn it on**: without it, anyone
  who knows or guesses an identified app user id can read its state and act as it.

### Request budgets

Public-key calls that create customers or call a store (`GET /v1/customers/{app_user_id}`,
`login`, `sync`, `checkout`) draw from two in-memory token buckets: one per client IP
(`ENTITLE_SDK_RATE_PER_IP`, default 600 requests per minute) and one per project
(`ENTITLE_SDK_RATE_PER_PROJECT`, default 6000 per minute). Each allows a burst of its
per-minute rate and refills continuously. Over either: `429 rate_limited` with
`Retry-After` (seconds). The client IP is the TCP peer, or the last `X-Forwarded-For`
entry with `ENTITLE_TRUST_PROXY`; IPv6 clients share a bucket per /64. Secret-key calls
are not budgeted. Budgets are per process and reset on restart.

`sync` fetches the purchase from the store before it creates the customer, so ids the
store does not know create nothing.

### `GET /v1/customers/{app_user_id}`

Returns `CustomerInfo`, creating the customer on first sight.

### `POST /v1/customers/{app_user_id}/login`

Logs the current app user id (`{app_user_id}`, typically anonymous) in as a known one.

```json
{ "new_app_user_id": "user_42", "identity_token": "<jwt for user_42>" }
```

`identity_token` is optional unless the project requires signed identity.

Response:

```json
{ "created": true, "customer_info": { …CustomerInfo for user_42… } }
```

`created` is `true` if `new_app_user_id` had never been seen before.

Rules (RevenueCat-like):

| Current customer | `new_app_user_id` | Result |
|---|---|---|
| anonymous | unknown | `new_app_user_id` becomes an alias of the current customer |
| anonymous | belongs to another customer | the anonymous customer is **merged into** that customer (aliases, account tokens, purchases, grants move; anonymous customer is deleted) |
| identified | anything | no merge; switches to `new_app_user_id`, creating it if unknown |
| already the same customer | – | no-op |

A customer is anonymous while all of its aliases are `$entitle_anon:` ids.
A customer has at most 100 app user ids; a login that would exceed it → 409 `conflict`.
`new_app_user_id` must not be anonymous (400).

### `POST /v1/customers/{app_user_id}/sync`

Restore / sync a purchase from the device. Entitle fetches the authoritative
state from the store and links the purchase to this customer.

```json
{ "store": "app_store", "id": "2000000134567890" }
```

| `store` | `id` |
|---|---|
| `app_store` | a StoreKit 2 transaction id |
| `play_store` | the purchase token |
| `creem` | a subscription id (`sub_…`) or checkout id (`ch_…`) |

Response: `CustomerInfo`.

Ownership:

- unlinked purchase, or already this customer's: linked to this customer.
- owned by another customer: **transferred** to this one (a `transfer` webhook
  event is sent), **unless** the store data carries an account token
  (`appAccountToken`, `obfuscatedExternalAccountId`, Creem checkout metadata)
  that resolves to a different existing customer with a non-anonymous app user
  id. Then the purchase provably belongs to that customer: `409 transfer_refused`,
  nothing changes. The token entitle stored from earlier store data counts too:
  a store response without a token (e.g. a Creem `sub_…` lookup, which lacks
  the checkout metadata) does not erase it. If the stored and the new token
  resolve to different customers, the sync is refused as well. Purchases made without an account token, whose token's
  customer was erased, or whose token's customer is purely anonymous still
  transfer (an anonymous id does not survive a reinstall or a new device, so
  restoring with the same store account must keep working).

### `POST /v1/customers/{app_user_id}/checkout`

Creates a Creem hosted checkout (web purchase or link-out from a mobile app).

```json
{
  "product_id": "prod_4a…",
  "success_url": "https://example.com/thanks",
  "metadata": { "campaign": "spring" }
}
```

`product_id` must be configured as a `creem` product in the project (else 404).
`success_url` and `metadata` are optional; metadata values are strings.

`success_url` must be on one of the project's `checkout_redirect_origins`
(same scheme, host and port; see `PATCH /admin/v1/projects/{project_id}`), else
`400 bad_request`. The list is empty by default, so **configure it before
passing a `success_url`**; checkouts without one always work.

Response:

```json
{ "id": "ch_1x…", "url": "https://checkout.creem.io/…" }
```

## Secret-key API (`/v1`, **secret**)

### `POST /v1/customers/{app_user_id}/portal`

Secret key only (a public key gets `403 forbidden`): the link opens the
customer's billing details, so hand it out from your backend.

Creem customer-portal link for the customer's latest Creem purchase.

Response: `{ "url": "https://creem.io/my-orders/login/…" }`. 404 if the customer
has no Creem purchase.

### `POST /v1/customers/{app_user_id}/entitlements/{identifier}/grant`

Grants a promotional entitlement. Body optional:

```json
{ "expires_at": "2026-12-31T23:59:59Z" }
```

Omitted / `null` = lifetime. `expires_at` must be in the future. Returns `CustomerInfo`.

### `POST /v1/customers/{app_user_id}/entitlements/{identifier}/revoke`

Revokes all promotional grants of that entitlement (store purchases are
untouched). Returns `CustomerInfo`.

### `GET /v1/customers/{app_user_id}/export`

GDPR access request. Returns everything stored about the customer:

```json
{
  "customer_id": "…",
  "created_at": "…",
  "aliases": ["user_42"],
  "account_tokens": ["…"],
  "entitlements": { …as in CustomerInfo… },
  "purchases": [ { …purchase fields…, "customer_ref": "…", "store_customer_id": "cust_…", "as_of": "…" } ],
  "purchase_history": [ { "purchase_id": "…", …purchase state fields…, "as_of": "…", "events": ["renewal"], "backfilled": false, "recorded_at": "…" } ],
  "promotional_grants": [ { "entitlement": "pro", "granted_at": "…", "expires_at": null, "revoked_at": null } ],
  "webhook_deliveries": [ { "payload": { … }, "status": "delivered", "attempts": 1 } ],
  "exported_at": "…"
}
```

404 if the app user id is unknown.

### `DELETE /v1/customers/{app_user_id}`

GDPR erasure: hard-deletes the customer, all aliases, account tokens,
purchases, promotional grants and outgoing webhook payloads about them.
Purchase state history is kept for aggregate revenue metrics but unlinked
from the customer.
`204` on success, 404 if unknown. Irreversible. Store notifications arriving
later for the erased purchases are stored unlinked (the old account token no
longer resolves).

## Admin API (`/admin/v1`)

### Projects

- `POST /admin/v1/projects` `{"name": "My App"}` → `201` `Project`
  `{"id", "name", "checkout_redirect_origins": [], "reporting_currency": "USD",
  "require_signed_identity": false, "has_identity_secret": false,
  "sandbox_grants_entitlements": false, "created_at"}`
- `GET /admin/v1/projects` → `[Project]`
- `GET /admin/v1/projects/{project_id}` → `Project`
- `PATCH /admin/v1/projects/{project_id}`
  `{"checkout_redirect_origins": ["https://example.com", "http://localhost:3000"], "reporting_currency": "EUR", "require_signed_identity": true, "sandbox_grants_entitlements": false}`
  → `Project`. All fields are optional; absent ones are left unchanged.
  - `checkout_redirect_origins` replaces the list of origins a checkout `success_url`
    may point to. Each entry is `http(s)://host[:port]` without path, query or
    credentials (400 otherwise); entries are normalized (lowercase host, default port dropped).
  - `reporting_currency`: the currency revenue is reported in (default `USD`). A
    3-letter ISO 4217 code (400 otherwise), stored uppercase. Amounts are converted
    with ECB euro reference rates, which cover ~30 currencies.
  - `require_signed_identity`: public-key calls for identified app user ids need a
    signed identity token (see [Signed identity](#signed-identity)). Enabling it
    without an identity secret → 400.
  - `sandbox_grants_entitlements` (default `false`): whether sandbox purchases
    (TestFlight, Play license testers, Creem test mode) grant entitlements. Turn it
    **on for test projects only**: sandbox purchases are free, so in a production
    project anyone with a tester build could unlock the app. Either way they are
    recorded and listed in `purchases`. Projects created before this default changed
    keep the value they had (then `true`); check production projects and turn it off.
- `POST /admin/v1/projects/{project_id}/identity-secret` → `201` `{"identity_secret": "ids_…"}`.
  Creates or replaces the secret your backend signs identity tokens with; shown once.
- `DELETE /admin/v1/projects/{project_id}` → `204` (deletes everything in it)

### API keys

- `POST /admin/v1/projects/{project_id}/api-keys` `{"kind": "public" | "secret"}` → `201`

  ```json
  { "id": "…", "kind": "public", "key_prefix": "pk_AbCdEfGh", "created_at": "…", "revoked_at": null,
    "key": "pk_AbCdEfGh…" }
  ```

  `key` is returned **only here**; only its SHA-256 hash is stored.
- `GET …/api-keys` → list without `key`
- `DELETE …/api-keys/{key_id}` → `204` (revokes)

### Store credentials

- `PUT /admin/v1/projects/{project_id}/credentials/{store}` → `204`. Stored
  encrypted (XChaCha20-Poly1305). Body by store:

  `creem`:

  ```json
  { "api_key": "creem_…", "webhook_secret": "whsec_…", "test_mode": true }
  ```

  `app_store`:

  ```json
  { "issuer_id": "…", "key_id": "ABC123", "private_key": "-----BEGIN PRIVATE KEY-----\n…",
    "bundle_id": "com.example.app", "app_apple_id": 1234567890 }
  ```

  (`app_apple_id` may be `null` until the app is live; it is required to verify
  production notifications.)

  `play_store`:

  ```json
  { "service_account_json": { "type": "service_account", … },
    "package_name": "com.example.app",
    "pubsub_audience": "https://entitle.example.com/webhooks/google/<project_id>",
    "pubsub_service_account_email": "pubsub-push@<gcp_project>.iam.gserviceaccount.com" }
  ```

  (`pubsub_audience` and `pubsub_service_account_email` are the audience and
  service account configured for authentication on the Pub/Sub push
  subscription; pushes whose OIDC token has another `email` are rejected with 401.)

- `GET …/credentials` → `[{"store": "creem", "updated_at": "…"}]` (secrets are never returned)
- `DELETE …/credentials/{store}` → `204`
- `POST …/credentials/{store}/test` → `200` `{"ok": true, "message": "…"}`: one
  authenticated call to the store with the saved credentials. A rejection is
  `{"ok": false, "message": "…"}`; 404 when no credentials are saved.
  - `app_store`: requests a test notification in sandbox, so Apple also calls
    the sandbox notification URL (visible as a delivery, not as purchase activity).
  - `play_store`: lists voided purchases (the permission purchase verification needs).
  - `creem`: looks up a product id that does not exist.

### Products and entitlements

- `POST …/products` `{"store": "app_store", "store_product_id": "com.example.pro.monthly", "display_name": "Pro monthly", "billing_period": "P1M"}`
  → `201` `{"id", "store", "store_product_id", "display_name", "billing_period", "created_at"}` (409 if it exists).
  `display_name` and `billing_period` are optional.
  `billing_period` is how often the subscription renews, as an ISO 8601 duration:
  `P1W`, `P1M`, `P2M`, `P3M`, `P6M` or `P1Y` (400 otherwise). It normalizes prices
  to monthly revenue; when `null`, the period is inferred from each purchase's
  current billing cycle.
- `PATCH …/products/{product_id}` `{"display_name": "Pro annual", "billing_period": "P1Y"}` → `Product`.
  Absent fields are left unchanged, `null` clears them. 404 if unknown.
- `GET …/products`, `DELETE …/products/{product_id}`
- `POST …/entitlements` `{"identifier": "pro", "description": "Pro features"}`
  → `201` `{"id", "identifier", "description", "product_ids": [], "created_at"}`
- `GET …/entitlements` (with `product_ids`), `DELETE …/entitlements/{entitlement_id}`
- `PUT …/entitlements/{entitlement_id}/products/{product_id}` → `204` (attach, idempotent)
- `DELETE …/entitlements/{entitlement_id}/products/{product_id}` → `204` (detach)

### Outgoing webhook endpoints

- `POST …/webhooks` `{"url": "https://api.example.com/entitle"}` → `201`
  `{"id", "url", "created_at", "secret": "whsec_…"}` (secret shown only here)
  - The url must be **public https** without credentials (400 otherwise). Every
    delivery re-checks the resolved addresses (no private, loopback or link-local
    targets) and follows no redirects. Set `ENTITLE_WEBHOOK_ALLOW_PRIVATE=true` to
    deliver to http or private-network endpoints.
- `GET …/webhooks`, `DELETE …/webhooks/{webhook_id}`
- `GET …/webhooks/deliveries?status=failed&limit=50` → newest deliveries first.
  `status`: `pending`, `delivered` or `failed` (all when absent); `limit` 1–200,
  default 50. 400 for an invalid parameter.

  ```json
  [{ "id": "…", "endpoint_id": "…", "url": "https://api.example.com/entitle",
     "event_id": "…", "event_type": "renewal", "status": "pending", "attempts": 3,
     "last_error": "endpoint answered 500 Internal Server Error",
     "next_attempt_at": "2026-10-01T10:04:00Z", "created_at": "…", "delivered_at": null,
     "payload": { … } }]
  ```

  `next_attempt_at` is set for pending deliveries only. A delivery is `failed`
  after 15 attempts (backoff 30 s doubling, capped at 6 h: about 1.4 days).
- `POST …/webhooks/deliveries/{delivery_id}/retry` → `204`: due immediately.
  Pending deliveries skip their backoff; failed and delivered ones are queued
  again (a failed one gets one more attempt). 409 while it is being sent, 404
  if unknown.

### Revenue metrics

`GET /admin/v1/projects/{project_id}/metrics?from=2026-01-01&to=2026-04-01&interval=month`

- `from` (inclusive) and `to` (exclusive): `YYYY-MM-DD`, UTC. `interval`: `day`, `week`
  (weeks start on Monday) or `month`. Buckets are aligned to interval starts; the
  first and last bucket are partial when `from` / `to` are not aligned (flows are
  counted from `from` / until `to` only, `start` is still the interval start).
- 400 for missing or malformed parameters, `to <= from`, or more than 1000 buckets.
  404 for an unknown project.
- Production purchases only (sandbox is excluded).

```json
{
  "currency": "USD",
  "history_start": "2026-10-01T00:00:00Z",
  "points": [{
    "start": "2026-01-01",
    "mrr_minor": 1234500, "active_subscriptions": 321, "active_trials": 12,
    "new_mrr_minor": 50000, "expansion_mrr_minor": 1200, "reactivation_mrr_minor": 900,
    "contraction_mrr_minor": -400, "churned_mrr_minor": -3000,
    "revenue_minor": 80000, "refunded_minor": 1000, "transactions": 95,
    "new_customers": 40, "new_trials": 20, "trial_conversions": 8,
    "churned_subscriptions": 5, "churn_rate": 0.0153
  }],
  "unconverted": [{ "currency": "AED", "revenue_minor": 1000, "mrr_minor": 500 }]
}
```

- `currency`: the project's `reporting_currency`; every `*_minor` amount is in its
  minor units.
- `history_start`: when purchase state recording began for this project (`null` if
  nothing was recorded yet). Everything before is unknown: purchases that existed
  when recording began appear with their state at that time (their MRR shows up as
  new MRR on that date), and earlier revenue is missing.

Metrics are computed from the purchase state history (one row per applied state
change). Definitions:

- **Access** of a subscription at time `t`: its latest state reported at or
  before `t` is `active`/`cancelled` and `t < expires_at`, or `grace_period` and
  `t < grace_period_expires_at` (`expires_at` if unknown). Billing retry, paused,
  expired, refunded and revoked have no access. Lapses are dated at the end of the
  paid (or grace) period, not when the store reported them.
- **Late renewals**: when access lapses and the purchase's next state grants access
  again within **3 days** after the lapse, access is continuous (no churn, no
  reactivation). An `active`, auto-renewing subscription whose period ended less
  than 3 days ago is still counted as active (renewal pending). Recovery from
  billing retry after that is a reactivation.
- **MRR** (`mrr_minor`): sum over subscriptions with access of their price
  normalized to one month (yearly ÷ 12, weekly × 52/12, …). Trials contribute
  nothing; paid introductory prices count at the intro price. One-time purchases
  have no MRR. Stock values (`mrr_minor`, `active_subscriptions`, `active_trials`)
  are taken at the end of the bucket, or now for the current (and future) bucket.
- **Billing period** used for normalization, first match: the product's configured
  `billing_period`; else inferred from the purchase's history for that product —
  the difference between consecutive `expires_at` values when a state extended the
  previous one without a lapse, otherwise `expires_at - purchased_at` of a state
  (for Google Play only on the purchase's first recorded state, since Play reports
  the subscription's first start as `purchased_at`). The latest inference at or
  before a state is used, else the first later one. Subscriptions whose period
  cannot be determined (no match within 10% of a supported period) count as active
  but contribute no MRR: configure `billing_period` on the product to avoid this.
- `active_subscriptions`: subscriptions with access that are not in a trial (intro
  periods included). `active_trials`: subscriptions with access in a free trial.
- **MRR movement** is per customer (a purchase never linked to a customer, or
  unlinked by GDPR erasure, is its own customer; states recorded before a purchase
  was linked belong to the customer it was first linked to). Every change of a
  customer's MRR is classified: 0 → >0 is `new_mrr_minor` if the customer never had
  MRR before, else `reactivation_mrr_minor`; >0 → 0 is `churned_mrr_minor`; other
  increases are `expansion_mrr_minor`, decreases `contraction_mrr_minor` (e.g. a
  product change). Churn and contraction are ≤ 0. A purchase moving to another
  customer is churn/contraction for one and new/expansion for the other.
- **FX**: amounts are converted with ECB reference rates (latest on or before the
  date). Stocks use the rate of the bucket's last day (or today); within a bucket,
  movement converts both sides of every change at that same rate, so the five
  movement fields sum exactly to the change of MRR within the bucket at that rate.
  With several currencies, `mrr_minor` of consecutive buckets can therefore differ
  from the movement sum by the FX drift between the two rates. Revenue and refunds
  use the rate of the charge date.
- `unconverted`: amounts in currencies without an ECB rate, in their own minor
  units, excluded from every other field: `revenue_minor` over the whole range and
  `mrr_minor` at the end of the last bucket. Refunds in such currencies are not
  reported anywhere.
- `revenue_minor`: gross charges (refunds not subtracted): the price of every
  recorded initial purchase (not trials, including one-time purchases) and renewal
  (including trial conversions), dated by when the store reported it.
  `transactions` counts them (also when a price is unknown or unconvertible).
  Product changes are not counted as charges. `refunded_minor`: the price of every
  refunded or revoked purchase, dated by the refund. States seeded when recording
  began are never revenue. Known limitation: after a GDPR erasure, a later store
  notification re-creates the purchase and records another initial purchase.
- `new_customers`: customers created (first use of an app user id) in the bucket,
  for any environment. Customers deleted by GDPR erasure or merged at login are no
  longer counted.
- `new_trials`: trials started. `trial_conversions`: subscriptions that moved from
  a trial to a paid (or intro) period with access.
- `churned_subscriptions`: paid (non-trial) subscriptions that lost access in the
  bucket. `churn_rate`: `churned_subscriptions` / `active_subscriptions` at the start
  of the bucket, `null` if that is 0.

### Purchase activity

`GET /admin/v1/projects/{project_id}/activity?environment=sandbox&app_user_id=user_42&limit=50`

The newest recorded purchase state changes (from `sync` and store notifications),
newest first. Meant for checking that a store integration works end to end, e.g.
with sandbox or TestFlight purchases.

- `environment`: `sandbox` or `production`; both when absent. `app_user_id`: only
  that customer's purchases. `limit`: 1–200, default 50.
- 400 for an invalid parameter, 404 for an unknown project or `app_user_id`.
- States seeded when recording began are not listed.

```json
[{
  "id": 1042,
  "recorded_at": "2026-09-30T10:05:02Z",
  "store": "app_store",
  "environment": "sandbox",
  "product_id": "com.example.pro.monthly",
  "app_user_id": "user_42",
  "events": ["renewal"],
  "status": "active",
  "period_type": "normal",
  "expires_at": "2026-09-30T10:10:00Z",
  "auto_renew": true,
  "price": { "amount_minor": 499, "currency": "EUR" }
}]
```

- `recorded_at`: when entitle applied the change. `events`: the lifecycle events
  derived from it (webhook event names); empty when only other fields changed.
- `app_user_id`: the customer's most recently added app user id; `null` when the
  purchase is not linked to a customer.

### Customer support

For looking up customers and granting access from the dashboard or admin tools.

- `GET /admin/v1/projects/{project_id}/customers?status=active&store=app_store&environment=production&limit=50&offset=0`
  → customers, most recently active first:

  ```json
  [{
    "customer_id": "0b7c…",
    "app_user_id": "user_42",
    "first_seen_at": "2026-08-01T09:12:00Z",
    "last_activity_at": "2026-09-30T10:05:02Z",
    "status": "active",
    "stores": ["app_store"]
  }]
  ```

  - `status` comes from store purchases only (promotional grants do not count):
    `active` (a non-trial purchase grants access now), `trial` (access only through
    trials), `churned` (purchases, none granting access), `none` (no purchases).
  - `store`, `environment`: only customers with a matching purchase; `status` and
    `stores` then count only those purchases. All filters are optional.
  - `app_user_id`: the most recently added app user id. `last_activity_at`: the
    latest purchase change, or `first_seen_at` without purchases.
  - `limit`: 1–200, default 50. `offset`: default 0. 400 for an invalid parameter,
    404 for an unknown project.
- `GET /admin/v1/projects/{project_id}/customers/{app_user_id}` → `CustomerInfo`
  (as `GET /v1/customers/{app_user_id}`). 404 for an unknown app user id: unlike the
  SDK endpoint, a lookup never creates a customer.
- `POST …/customers/{app_user_id}/entitlements/{identifier}/grant` and
  `POST …/customers/{app_user_id}/entitlements/{identifier}/revoke`: as the secret-key
  endpoints of the same name, with the admin token.

## MCP (`/mcp`)

Enabled when `ENTITLE_PUBLIC_URL` is set. A Streamable HTTP MCP server (stateless, JSON
responses) with tools over the admin API. Results are the admin API's JSON; its errors
come back as tool errors.

Customer results leave out what would let the reader act as a customer, as the SDK
does for public keys: anonymous app user ids (`$entitle_anon:…`) are removed from
`aliases` and shown as `app_user_id: null` in customer lists and activity, and purchases
have no `original_transaction_id` / `store_transaction_id`. `customer_id` identifies a
customer instead: `get_customer` and `list_activity` take it in place of `app_user_id`.

- Read (scope `mcp`): `list_projects`, `list_products`, `list_entitlements`, `get_metrics`,
  `list_customers`, `get_customer`, `list_activity`.
- Write (scope `mcp:write`): `grant_entitlement`, `revoke_entitlement` (promotional grants,
  as the admin endpoints of the same name).

Connect a client to `{ENTITLE_PUBLIC_URL}/mcp`:

- **OAuth** (claude.ai, ChatGPT, MCP gateways, Claude Code): without a token, `/mcp`
  answers `401` with a `WWW-Authenticate` challenge pointing at
  `/.well-known/oauth-protected-resource/mcp`. Clients register dynamically
  (`POST /oauth/register`), and the consent page (`GET /oauth/authorize`) asks for the
  admin token to approve. Write access is granted only when "Allow changes" is checked
  there (preselected if the client asked for `mcp:write`); the token response's `scope`
  says which. Codes use PKCE (S256). Access tokens (`mcp_…`) do not expire and there are
  no refresh tokens; rotating `ENTITLE_ADMIN_TOKEN` revokes all of them.
  Allowed callbacks: claude.ai / claude.com, ChatGPT, `https://mcpi.app/gateway/oauth/callback`
  and loopback.
- **Static header**: `Authorization: Bearer <ENTITLE_ADMIN_TOKEN>` (read and write).

`/oauth/*` allows 30 requests per minute per client IP, then answers `429` with
`Retry-After: 60` (see `ENTITLE_TRUST_PROXY`). Registration keeps at most 1000 clients
that have no token yet; clients without a token are deleted after a day.

## Inbound store webhooks

Configure these URLs in the store consoles:

| Store | URL |
|---|---|
| Creem | `POST /webhooks/creem/{project_id}` |
| App Store Server Notifications V2 | `POST /webhooks/apple/{project_id}` |
| Google Play RTDN (Pub/Sub push subscription, with OIDC auth) | `POST /webhooks/google/{project_id}` |

Answers `200` when processed or already processed (deduplicated by the store's
event id). Out-of-order deliveries never overwrite newer state.

## Outgoing webhooks

Every configured endpoint receives `POST` requests:

```
Content-Type: application/json
Entitle-Signature: t=1759147200,v1=5f2c…
```

```json
{
  "id": "6c3a…",
  "type": "renewal",
  "created_at": "2026-10-01T10:00:03Z",
  "project_id": "…",
  "customer": { "id": "…", "app_user_ids": ["user_42"] },
  "purchase": { …purchase fields as in CustomerInfo… },
  "entitlements": { …active entitlements after the change… },
  "transferred_from": { "customer_id": "…" }
}
```

`transferred_from` is only present on `transfer`.

Event `type`s:

| Type | When |
|---|---|
| `initial_purchase` | a purchase is linked to a customer for the first time |
| `renewal` | expiry moved forward (renewal, billing recovered) |
| `cancellation` | auto-renew turned off / cancellation scheduled |
| `uncancellation` | auto-renew turned back on |
| `expiration` | the subscription expired |
| `billing_issue` | entered grace period / billing retry |
| `refund` | refunded or revoked (see `purchase.status`). Partial refunds keep access and send no event |
| `product_change` | the subscription switched product |
| `transfer` | a restore moved the purchase to another customer |

Verify: compute `hex(HMAC-SHA256(secret, "{t}.{raw body}"))`, compare it with `v1`
in constant time and reject old `t` (e.g. more than 5 minutes).

```js
import crypto from "node:crypto";
const [t, v1] = header.split(",").map((p) => p.split("=")[1]);
const expected = crypto.createHmac("sha256", secret).update(`${t}.${rawBody}`).digest("hex");
const ok = crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(v1))
  && Math.abs(Date.now() / 1000 - Number(t)) < 300;
```

Delivery: any 2xx is success. Otherwise retried with exponential backoff (30 s,
1 min, 2 min, … capped at 6 h) for up to 15 attempts (~1.4 days). Deliveries are
at-least-once: deduplicate on `id`. Order is not guaranteed.
