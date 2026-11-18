# Store setup: from zero to a sandbox purchase

This guide sets up one project, one product and one entitlement, and ends with a sandbox
purchase unlocking the entitlement in an app. It uses the names of the
[Capacitor example](../examples/capacitor): project `Quest`, product `quest.example.full`
(a non-consumable), entitlement `full_game`. Replace them with yours.

Store console menus change often. Where this guide is unsure of an exact menu name, it
describes the page instead.

## 0. Prerequisites

- An entitle server reachable over **public HTTPS**: Apple and Google deliver notifications
  to it, and the app on a phone calls it. For local development, use a tunnel to
  `localhost:8080`.
- An Apple Developer Program membership and/or a Google Play developer account.
- Apple: the **Paid Applications** agreement is active (App Store Connect → Business).
  Without it, the store returns no products.

## 1. entitle: project, key, product, entitlement

Do this either in the dashboard or with the CLI. Both call the same admin API.

### Dashboard

1. Open `https://<your-server>/dashboard/` and sign in with `ENTITLE_ADMIN_TOKEN`.
   Open it via the public URL: the notification URLs it shows are built from it.
2. Create the project (the form shown when there is none, or **+ New project…** in the header's
   project picker), then open the **Setup** tab.
3. **Project**: copy the **Project id**.
4. **API keys**: **New public key**. Copy the `pk_…` key now; it is not shown again.
5. **Products & entitlements**:
   1. Add entitlement: identifier `full_game`.
   2. Add product: store **App Store**, store product id `quest.example.full`, Renews
      **None (one-time, or infer)**. Repeat with store **Google Play**.
   3. On `full_game`, pick each product under **Attach product…** and press **Attach**.
6. **Stores**: fill in the credentials once you have them (sections 2 and 3). Each store
   shows its **Notification URL** to paste into the store console. Saving credentials runs
   **Test connection** (one real call to the store), and you can rerun it any time.

### CLI

`entitle admin` talks to a running server (`--url` or `ENTITLE_URL`, token from
`ENTITLE_ADMIN_TOKEN`). With Docker, prefix the commands with
`docker compose exec entitle`.

```sh
export ENTITLE_URL=https://entitle.example.com
entitle admin projects create "Quest"
entitle admin projects list                               # the project id
entitle admin keys create "Quest"                         # pk_… printed once
entitle admin entitlements create "Quest" full_game
entitle admin products create "Quest" quest.example.full --store app_store
entitle admin products create "Quest" quest.example.full --store play_store
# --store is needed because the same product id exists in both stores:
entitle admin entitlements attach "Quest" full_game quest.example.full --store app_store
entitle admin entitlements attach "Quest" full_game quest.example.full --store play_store
```

For a subscription, add `--billing-period P1M` (or `P1W`, `P2M`, `P3M`, `P6M`, `P1Y`) to
`products create`.

Put the server URL, the `pk_` key, the product id and the entitlement into the app's config
(`examples/capacitor/src/config.ts` in the example). The project id goes into the store
notification URLs below.

## 2. App Store Connect

1. **Bundle id**: register it under Certificates, Identifiers & Profiles in the Apple Developer
   account, with the In-App Purchase capability. The same id is `appId` in
   `capacitor.config.json`.
2. **App record**: Apps → **+** → New App, with that bundle id.
3. **Product**: in the app's In-App Purchases page (Subscriptions for auto-renewable ones,
   inside a subscription group), create a **Non-Consumable** with product id
   `quest.example.full`. Fill in reference name, price and at least one localization.
   A product with missing metadata is not returned to the app.
4. **In-App Purchase API key**: Users and Access → Integrations → **In-App Purchase**.
   Generate a key and download the `.p8` (downloadable once). Note the **Key ID** and the
   **Issuer ID** shown on that page.
5. **Credentials in entitle**:
   - Dashboard: Setup → Stores → App Store → **Set up**: the `.p8` file, Key ID, Issuer ID,
     Bundle ID. Leave Apple ID empty until the app is live.
   - CLI:
     ```sh
     entitle admin credentials set-apple "Quest" --issuer-id <issuer-id> --key-id <key-id> \
       --key-file SubscriptionKey_<key-id>.p8 --bundle-id com.example.quest
     ```
   `app_apple_id` (`--app-apple-id`, the app's **Apple ID** under App Information) is required
   once the app is live: production notifications are not verified without it.

   Check them: **Test connection**, or `entitle admin credentials test "Quest" app_store`. This
   asks Apple for a sandbox test notification, so it also checks the key, the issuer and the
   bundle id. Once step 6 is done, Apple delivers that test notification to your server.
6. **App Store Server Notifications**: App Information → App Store Server Notifications. Set
   **Version 2** and enter `https://<your-server>/webhooks/apple/<project_id>` as both the
   production and the sandbox server URL.
7. **Sandbox tester**: Users and Access → Sandbox → create a test account with an email address
   that is not an existing Apple Account.
8. **Test**:
   - **Device, sandbox account**: run the app from Xcode (In-App Purchase capability added) on a
     device. Sign in with the sandbox tester when the purchase sheet asks, or in the device's
     App Store / developer settings under Sandbox Account.
   - **TestFlight**: upload a build and install it via TestFlight. TestFlight purchases are
     sandbox purchases with the tester's own Apple Account, and are free.

## 3. Google Play Console

1. **App**: create the app in Play Console. The package name is `appId` in
   `capacitor.config.json` and cannot be changed later.
2. **Internal testing build**: build a signed release bundle (`.aab`) and upload it to the
   **internal testing** track, with your test accounts added as testers. Billing does not work,
   and products cannot be created, until a build that includes the Play Billing library has
   been uploaded (`@oxidt/entitle-capacitor` brings it).
3. **License testers**: in Play Console's account-level settings (not the app's), under
   License testing, add the Google accounts that will buy. Their purchases are test purchases
   (no charge) and are recorded as `sandbox` in entitle.
4. **Product**: in the app's Monetize section, create a one-time product with product id
   `quest.example.full` and activate it. For a subscription, create the subscription, add a
   **base plan**, and activate the base plan; a subscription without an active base plan is
   not offered.
5. **Service account** (lets entitle read and acknowledge purchases):
   1. In a Google Cloud project (e.g. the one used for Pub/Sub below), enable the
      **Google Play Android Developer API**.
   2. In the same project, create a service account and a **JSON key** for it
      (IAM & Admin → Service accounts).
   3. Play Console → **Users and permissions** → invite the service account's email. Grant
      access to the app with permission to view financial data and to manage orders and
      subscriptions. Permission changes can take a while (reportedly up to a day) to apply.
6. **Real-time developer notifications (RTDN)**:
   1. Google Cloud → Pub/Sub: create a topic, e.g. `play-rtdn`.
   2. On the topic, grant `google-play-developer-notifications@system.gserviceaccount.com` the
      **Pub/Sub Publisher** role.
   3. Create a service account for the push, e.g. `pubsub-push@<gcp_project>.iam.gserviceaccount.com`.
   4. Create a **push** subscription on the topic:
      - endpoint: `https://<your-server>/webhooks/google/<project_id>`
      - enable authentication, with the push service account from step 3
      - audience: the endpoint URL (whatever you set here must match `pubsub_audience` in
        entitle)
   5. Play Console → the app's monetization setup → Real-time developer notifications: enter
      the full topic name `projects/<gcp_project>/topics/play-rtdn` and send a test
      notification.
7. **Credentials in entitle**:
   - Dashboard: Setup → Stores → Google Play → **Set up**: the service account JSON key file,
     Package name, Pub/Sub push audience, Pub/Sub push service account.
   - CLI, with `play.json`:
     ```json
     { "service_account_json": { "type": "service_account", "…": "contents of the JSON key" },
       "package_name": "com.example.quest",
       "pubsub_audience": "https://<your-server>/webhooks/google/<project_id>",
       "pubsub_service_account_email": "pubsub-push@<gcp_project>.iam.gserviceaccount.com" }
     ```
     ```sh
     entitle admin credentials set "Quest" play_store play.json
     entitle admin credentials test "Quest" play_store   # or Test connection in the dashboard
     ```
   The test lists voided purchases, which needs the same permission as purchase verification.
   A `403` usually means the Play Console permissions have not applied yet.
   Pushes whose OIDC token has a different audience or service account email are rejected
   with `401`.
8. **Test**: on a device signed in with a license tester, install the app from the internal
   testing track (opt-in link) and buy.

## 4. Verify

1. Buy in the app. With the example app, the log shows `Purchased quest.example.full and
   synced` and the entitlement turns **active**. In code: `entitle.isEntitled('full_game')`.
2. Dashboard → **Revenue** tab → **Purchase activity** → **Sandbox**. It refreshes every 10 s and
   lists each purchase state change with store, product, app user id and events. The same data:
   ```sh
   curl -H "Authorization: Bearer $ENTITLE_ADMIN_TOKEN" \
     "$ENTITLE_URL/admin/v1/projects/<project_id>/activity?environment=sandbox"
   ```
3. Store notifications show up there too, e.g. a refund or (for subscriptions) a renewal.
4. **Customers** tab: look up the app user id to see its entitlements, purchases and history.
   It can also grant `full_game` for a while without a purchase:
   `entitle admin customers grant "Quest" <app_user_id> full_game --days 7`.
5. If you use outgoing webhooks: Setup → Outgoing webhooks → **Recent deliveries** shows each
   delivery with its status and last error, with **Retry now**.

## Pitfalls

- **Xcode local StoreKit testing (`Products.storekit`) cannot work end to end.** entitle looks
  every transaction up with Apple's App Store Server API (production, then sandbox). Transactions
  from a local StoreKit configuration exist only in Xcode's local test environment, so the lookup fails, the
  sync fails, and the transaction stays unfinished: StoreKit delivers it again at every launch and
  `startPurchaseSync` retries it, failing each time. Use it only to test the native purchase UI;
  use a sandbox account or TestFlight for anything that involves entitle.
- **Sandbox purchases never appear in the revenue charts.** Revenue metrics count production
  purchases only. Use the Purchase activity panel with **Sandbox** selected.
- **Google refunds unacknowledged purchases after 3 days.** entitle acknowledges a Play purchase
  only after verifying it, on a sync from the app or an RTDN. If neither reaches entitle (wrong
  server URL in the app, broken RTDN, missing service account permission), the purchase is refunded.
- **Sandbox subscriptions renew on an accelerated schedule** (a monthly subscription renews within
  minutes) and stop after a limited number of renewals. Expect renewal and expiration events
  shortly after the purchase.
- **`transfer_refused`** (409 on sync): the store purchase belongs to a different entitle
  customer, e.g. the same sandbox account was used with another app user id. See
  [`docs/api.md`](api.md#post-v1customersapp_user_idsync).
- **A purchase shows up in entitle but the app stays locked.** Apple's server notification
  records the purchase even when the app's own sync failed, so the dashboard looks healthy while
  the app shows an error. Read the server log first: `store adapter error` names the cause.
- **Test connection passing does not prove syncs work.** It asks Apple's *sandbox* API only;
  syncs try production first. Before the app's first release, Apple's production API answers 401
  for it, which entitle treats as "try sandbox" (since 99c494e). On an older server the same 401
  failed every sandbox purchase with `store_credentials` while Test connection still passed.
- **Check which build is deployed** with `curl $ENTITLE_URL/version` before debugging a fix you
  think is live.
- **Production notifications from Apple need `app_apple_id`** in the App Store credentials.
