# Store adapter contract

Each store lives in its own crate. Phase-2 work happens **only** inside that
crate (`src/`, `tests/`, `tests/fixtures/`, its `Cargo.toml`). The server, the
database and `entitle-core` are already wired; if the contract seems to need a
change, stop and raise it instead of editing other crates.

| Crate | Type | Implements |
|---|---|---|
| `crates/entitle-creem` | `CreemAdapter` | `StoreAdapter<Credentials = CreemCredentials>` + `CheckoutAdapter` |
| `crates/entitle-apple` | `AppleAdapter` | `StoreAdapter<Credentials = AppleCredentials>` |
| `crates/entitle-google` | `GoogleAdapter` | `StoreAdapter<Credentials = GoogleCredentials>` |

Keep `pub fn new() -> Self` (the server calls `Adapters::real()` →
`XAdapter::new()`). Add a second constructor (e.g. `with_base_url`) if tests
need to point at a mock server.

## The traits (`entitle_core::adapter`)

```rust
#[async_trait]
pub trait StoreAdapter: Send + Sync {
    type Credentials: Send + Sync;

    async fn parse_notification(
        &self,
        credentials: &Self::Credentials,
        notification: &InboundNotification, // lowercase headers + raw body bytes
    ) -> Result<ParsedNotification, AdapterError>;

    async fn fetch_purchases(
        &self,
        credentials: &Self::Credentials,
        id: &str,
    ) -> Result<Vec<PurchaseSnapshot>, AdapterError>;

    // One authenticated store call for the admin "test connection"; a summary on success.
    async fn check_credentials(&self, credentials: &Self::Credentials) -> Result<String, AdapterError>;
}

#[async_trait]
pub trait CheckoutAdapter: StoreAdapter {           // Creem only
    async fn create_checkout(&self, credentials: &Self::Credentials, request: &CheckoutRequest)
        -> Result<CheckoutSession, AdapterError>;   // { id, url }
    async fn portal_url(&self, credentials: &Self::Credentials, store_customer_id: &str)
        -> Result<String, AdapterError>;
}

pub struct ParsedNotification {
    pub event_id: String,                 // dedup key
    pub snapshots: Vec<PurchaseSnapshot>, // may be empty (test pings, irrelevant events)
}
```

Rules:

- **No database, no server types.** Adapters are stateless translators: verify,
  parse, call store APIs, return normalized data. In-memory caches (OAuth access
  tokens, JWKS, Apple root certs) are fine.
- Return **state, not events.** The server diffs the stored snapshot against the
  new one and derives `initial_purchase`, `renewal`, `cancellation`, …
  (`entitle_core::diff_events`). Never try to express an event in a snapshot.
- Every snapshot must have `store` set to the adapter's own store (the server
  rejects others).
- `event_id` must be the store's unique id **for the delivery**, stable across
  retries: Creem `id` (`evt_…`), Apple `notificationUUID`, Pub/Sub `message.messageId`.
- Errors → HTTP status (what stores see on webhooks, so pick carefully):

  | `AdapterError` | HTTP | Use for |
  |---|---|---|
  | `InvalidSignature(_)` | 401 | bad HMAC / JWS chain / OIDC token |
  | `Malformed(_)` | 400 | unparsable payload (store will not fix it by retrying) |
  | `NotFound` | 404 | `fetch_purchases`: unknown id |
  | `Credentials(_)` | 422 | missing/invalid credentials, store rejected auth |
  | `Unsupported(_)` | 422 | a purchase type entitle cannot represent (App Store consumables, non-renewing subscriptions) |
  | `Upstream(_)` | 502 | store API down / 5xx / timeout (store retries the webhook) |
  | `NotImplemented` | 501 | phase-1 stubs only |

## `PurchaseSnapshot` (normalized state of one purchase)

```rust
pub struct PurchaseSnapshot {
    pub store: Store,                                  // Creem | AppStore | PlayStore
    pub environment: Environment,                      // Sandbox | Production
    pub original_transaction_id: String,               // identity of the purchase, see below
    pub store_transaction_id: String,                  // latest transaction / order id
    pub product_id: String,                            // store product id
    pub kind: PurchaseKind,                            // Subscription | OneTime
    pub status: PurchaseStatus,                        // see mapping below
    pub period_type: PeriodType,                       // Trial | Intro | Normal
    pub purchased_at: DateTime<Utc>,                   // start of current period / purchase time (Google: see below)
    pub expires_at: Option<DateTime<Utc>>,             // Some for subscriptions, None for lifetime
    pub grace_period_expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price: Option<Money>,                          // minor units + ISO 4217
    pub customer_ref: Option<CustomerId>,              // the account token echoed back by the store
    pub store_customer_id: Option<String>,             // Creem `cust_…` (portal); None elsewhere
    pub as_of: DateTime<Utc>,                          // store timestamp of this state, for ordering
}
```

- **Identity** `(store, original_transaction_id)`: Apple `originalTransactionId`;
  Google the purchase token; Creem the subscription id (`sub_…`) for
  subscriptions, the order id (`ord_…`) for one-time payments.
- **`as_of`** decides ordering. A snapshot with an older `as_of` than the stored
  one is ignored. Use the store's own clock: Apple `signedDate` of the
  notification (or of the signed transaction/renewal info from the API),
  Google `eventTimeMillis` (for API fetches: the fetch time), Creem the event
  `created_at` (ms) (for API fetches: the fetch time). Never use a time that can
  go backwards for the same purchase.
- **`customer_ref`**: parse the account token the app/server set and return it
  as `CustomerId(uuid)`. Invalid/absent → `None` (purchase is stored unlinked
  until restored).
  - Apple: `appAccountToken` of the transaction.
  - Google: `externalAccountIdentifiers.obfuscatedExternalAccountId`.
  - Creem: `metadata["entitle_customer_id"]` (`CREEM_CUSTOMER_METADATA_KEY`) on
    the checkout/subscription.
- **`price`**: the currency's ISO 4217 minor units (2 decimals for EUR, 0 for
  JPY/KRW, 3 for KWD/BHD/…). Use `entitle_core::currency`: Apple milli-units
  (`price: 4990` EUR = 4.99 → 499; `480000` JPY → 480) via
  `minor_from_milliunits`, Google `google.type.Money` (`units` + `nanos`) via
  `minor_from_units_nanos`. Creem already uses cents. `None` if unknown.

### Status mapping

| `PurchaseStatus` | Access? | Apple | Google (subscriptionsv2) | Creem |
|---|---|---|---|---|
| `Active` | until `expires_at` | status 1, auto-renew on | `SUBSCRIPTION_STATE_ACTIVE`, auto-renew on | `active`, `trialing` (+ `PeriodType::Trial`), paid one-time order |
| `Cancelled` | until `expires_at` | status 1, `autoRenewStatus` 0 | `SUBSCRIPTION_STATE_CANCELED` before expiry | `scheduled_cancel` |
| `GracePeriod` | until `grace_period_expires_at` | status 4 (`gracePeriodExpiresDate`) | `SUBSCRIPTION_STATE_IN_GRACE_PERIOD` | `past_due` (grace = period end + 7 days) |
| `BillingRetry` | no | status 3 | `SUBSCRIPTION_STATE_ON_HOLD` | `unpaid` |
| `Paused` | no | – | `SUBSCRIPTION_STATE_PAUSED` | `paused` |
| `Expired` | no | status 2 | `SUBSCRIPTION_STATE_EXPIRED` | `expired`, `canceled` |
| `Refunded` | no | `REFUND` / `revocationReason` set (own purchase) | voided purchase, full refund | `refund.created`, full refund |
| `Revoked` | no | status 5 / `REVOKE` / revoked `FAMILY_SHARED` transaction | – | `dispute.created` |

Partial refunds do not change the purchase (access continues, no snapshot, so no
outgoing `refund` event): Creem `refund.created` with `transaction.status =
partialRefund` or `transaction.refunded_amount < transaction.amount_paid`;
Google `voidedPurchaseNotification.refundType = 2` (quantity-based partial).

`auto_renew` should mirror the store's flag; `Cancelled` implies `auto_renew = false`.

## Per store

### Creem (`entitle-creem`)

Wraps the `oxidt-creem` client
(<https://github.com/oxidt/oxidt-kit>, pin rev
`b2e462619b6b5afe8dd72a3f14a16efde1156f47`, git dependency, not a path dep, so
Docker builds work): `Client::new(api_key, test_mode)` (or `with_base_url`, used by
`CreemAdapter::with_base_url` for tests), `checkout`, `checkout_session`,
`subscription`, `billing_portal`, and `verify_webhook(secret, signature, body) -> Event`.

- `parse_notification`: header `creem-signature` = hex HMAC-SHA256(webhook_secret,
  raw body). Events: `checkout.completed`, `subscription.active|paid|trialing|
  scheduled_cancel|past_due|unpaid|paused|expired|canceled|update`,
  `refund.created`, `dispute.created`. Unrelated events (`credits.*`) → empty
  `snapshots`. `test_mode` → `Environment::Sandbox`.
- `fetch_purchases(id)`: `sub_…` → subscription, `ch_…` → checkout session
  (its subscription or one-time order, mapped like `checkout.completed`).
- `create_checkout`: `product_id`, `success_url`, `metadata` + always
  `entitle_customer_id = request.customer`. Use the customer id as `request_id` too.
- `portal_url(store_customer_id)`: billing portal link for `cust_…`.
- `check_credentials`: `GET /products/prod_entitle_check`; 404 means the key was
  accepted (Creem has no "who am I" endpoint).
- Set `store_customer_id` to the Creem customer id on every snapshot.
- `past_due` → `GracePeriod` with `grace_period_expires_at = current_period_end +
  7 days` (`PAST_DUE_GRACE_PERIOD`): Creem retries the charge but reports no
  grace end, and the period end has usually passed when the event arrives.

### Apple (`entitle-apple`)

- `parse_notification`: body `{"signedPayload": "<JWS>"}`. Verify the JWS x5c
  chain up to Apple Root CA G3 (embed the root cert), check `bundleId` and, in
  production, `appAppleId`. Decode `data.signedTransactionInfo` and
  `data.signedRenewalInfo` (both JWS) → one snapshot. `event_id =
  notificationUUID`; `TEST` → empty snapshots. `as_of = signedDate`.
- `fetch_purchases(transaction_id)`: App Store Server API with an ES256 JWT
  (`iss` = issuer id, `kid` = key id, `aud` = `appstoreconnect-v1`, `bid` =
  bundle id). Use Get All Subscription Statuses
  (`/inApps/v1/subscriptions/{transactionId}`) for subscriptions, Get Transaction
  Info (`/inApps/v1/transactions/{transactionId}`) for one-time purchases. Try
  production, fall back to sandbox on 404, and on 401 (production rejects apps
  that have no release yet).
- Product `type`: `Auto-Renewable Subscription` → `Subscription`, `Non-Consumable` →
  `OneTime` (lifetime). Anything else (consumables, non-renewing subscriptions, whose
  term the app defines, missing or future types) → `Unsupported` from
  `fetch_purchases`; notifications for them carry no snapshots.
- `period_type` from `offerType` (1 intro → `Intro`, free trial → `Trial`).
- `check_credentials`: Request a Test Notification in sandbox
  (`POST /inApps/v1/notifications/test`). Proves key, issuer and bundle id, and
  Apple then notifies the sandbox notification URL.

### Google (`entitle-google`)

- `parse_notification`: Pub/Sub push body
  `{"message": {"data": "<base64 DeveloperNotification>", "messageId": "…"}, "subscription": "…"}`.
  Verify the `Authorization: Bearer <OIDC JWT>` against Google's JWKS with
  `aud = pubsub_audience`, `email = pubsub_service_account_email` and
  `email_verified = true` (any service account can mint a token for the
  audience, so the email check is what authenticates the push). `event_id = message.messageId`. The notification only
  has a purchase token: call the API to get the state.
  `subscriptionNotification` → subscriptionsv2; `oneTimeProductNotification` →
  product purchase; `voidedPurchaseNotification` → mark `Refunded`/`Revoked`;
  `testNotification` → empty snapshots. Check `packageName`.
- `fetch_purchases(purchase_token)`: `purchases.subscriptionsv2.get`, falling
  back to the one-time product purchase API. OAuth via the service account JSON
  (scope `androidpublisher`).
- Acknowledge unacknowledged purchases (Google refunds them after 3 days) —
  this is an API call, so it belongs in the adapter.
- `testPurchase` present → `Environment::Sandbox`.
- `check_credentials`: `purchases.voidedpurchases.list` (`maxResults=1`): needs
  the same financial-data permission as purchase verification; 400/404 → unknown
  package.
- `purchased_at` is the subscription's `startTime` (first granted), not the
  current period start: subscriptionsv2 does not expose a period start.

## Tests and fixtures

- Unit/integration tests live in the adapter crate: `crates/entitle-<store>/tests/*.rs`.
- Fixtures go in `crates/entitle-<store>/tests/fixtures/` (real payloads from the
  store docs or sandbox, with secrets replaced). Load with
  `include_bytes!("fixtures/<name>.json")`.
- For signed payloads, generate test keys in the test (HMAC secret for Creem,
  a test CA + leaf for Apple JWS, a test RSA key + JWKS for Google OIDC) so the
  verification path is exercised, and keep one fixture per event type.
- Mock store APIs with a local HTTP server (e.g. `wiremock` as a dev-dependency)
  via the adapter's test constructor.
- `entitle-core` has a `testing` feature with snapshot builders.
- The server's own tests use a fake adapter (`crates/entitle-server/tests/common`)
  and must keep passing; run `cargo test --workspace`.

## Done means

`cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace` pass, and no method returns `NotImplemented` any more.
`crates/entitle-server/tests/webhooks.rs::real_adapters_are_wired_to_webhooks`
sends an unsigned payload to the real adapters: it must be rejected with 401
(`InvalidSignature`) or 400 (`Malformed`) without network access.
