// End-to-end test of the SDK against a real entitle server + Postgres.
//
//   bun run e2e        (from sdk/js; needs docker and cargo)
//
// Starts `docker compose -p entitle up -d postgres`, builds and runs the server on
// 127.0.0.1:18787, sets up a project through the admin API, drives the SDK through
// anon -> logIn -> getCustomerInfo -> createCheckout -> signed Creem webhook ->
// getCustomerInfo({ fresh: true }), then stops the server (SIGTERM) and runs
// `docker compose -p entitle down`.

import { createHmac, randomBytes, randomUUID } from "node:crypto";
import { resolve } from "node:path";

import {
  type ActiveEntitlement,
  type CustomerInfo,
  Entitle,
  EntitleError,
  type FetchLike,
  type Money,
  type PurchaseView,
  memoryStorage,
} from "../src/index.js";
import type { LogInResponse } from "../src/generated/LogInResponse.js";

const ROOT = resolve(import.meta.dir, "../../..");
const BIND = "127.0.0.1:18787";
const BASE = `http://${BIND}`;
const ADMIN_TOKEN = randomBytes(32).toString("hex");
const WEBHOOK_SECRET = "whsec_e2e_secret";
const PRODUCT = "prod_e2e_lifetime";
const ORIGIN = "http://localhost:3000";

// Exactly the keys of the generated types (the compiler rejects missing or extra ones),
// compared at runtime with what the server really sends. 0: secret key only, so a
// public-key response must not have it.
const KEYS = {
  CustomerInfo: {
    customer_id: 1, app_user_id: 1, aliases: 0, entitlements: 1, purchases: 1,
    management_url: 1, first_seen_at: 1, request_date: 1,
  } satisfies Record<keyof CustomerInfo, 0 | 1>,
  ActiveEntitlement: {
    identifier: 1, source: 1, product_id: 1, purchased_at: 1, expires_at: 1,
    period_type: 1, will_renew: 1, billing_issue: 1, environment: 1,
  } satisfies Record<keyof ActiveEntitlement, 1>,
  PurchaseView: {
    store: 1, environment: 1, product_id: 1, original_transaction_id: 0, store_transaction_id: 0,
    kind: 1, status: 1, period_type: 1, purchased_at: 1, expires_at: 1,
    grace_period_expires_at: 1, auto_renew: 1, price: 1,
  } satisfies Record<keyof PurchaseView, 0 | 1>,
  Money: { amount_minor: 1, currency: 1 } satisfies Record<keyof Money, 1>,
  LogInResponse: { created: 1, customer_info: 1 } satisfies Record<keyof LogInResponse, 1>,
};

let failures = 0;
function check(name: string, ok: boolean, detail?: unknown) {
  if (ok) console.log(`  ok   ${name}`);
  else {
    failures++;
    console.log(`  FAIL ${name}`, detail ?? "");
  }
}

function checkKeys(type: keyof typeof KEYS, value: unknown) {
  const got = Object.keys(value as object).sort();
  const want = Object.entries(KEYS[type] as Record<string, 0 | 1>)
    .filter(([, publicKey]) => publicKey === 1)
    .map(([key]) => key)
    .sort();
  check(`${type} keys match the generated type`, JSON.stringify(got) === JSON.stringify(want), { got, want });
}

async function run(cmd: string[], env?: Record<string, string>) {
  const p = Bun.spawn(cmd, { cwd: ROOT, stdout: "inherit", stderr: "inherit", env: { ...process.env, ...env } });
  if ((await p.exited) !== 0) throw new Error(`${cmd.join(" ")} failed`);
}

async function admin<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(`${BASE}/admin/v1${path}`, {
    method,
    headers: { Authorization: `Bearer ${ADMIN_TOKEN}`, "Content-Type": "application/json" },
    body: body === undefined ? null : JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`admin ${method} ${path}: ${res.status} ${await res.text()}`);
  return (res.status === 204 ? undefined : await res.json()) as T;
}

async function expectError(name: string, p: Promise<unknown>, codes: string[]) {
  try {
    await p;
    check(name, false, "resolved");
  } catch (e) {
    const ok = e instanceof EntitleError && codes.includes(e.code);
    check(`${name} -> ${e instanceof EntitleError ? `${e.status} ${e.code}` : e}`, ok, e);
  }
}

let server: ReturnType<typeof Bun.spawn> | undefined;
try {
  console.log("== postgres + server");
  await run(["docker", "compose", "-p", "entitle", "up", "-d", "--wait", "postgres"]);
  await run(["cargo", "build", "--bin", "entitle"]);
  server = Bun.spawn([resolve(ROOT, "target/debug/entitle")], {
    cwd: ROOT,
    stdout: "ignore",
    stderr: "inherit",
    env: {
      ...process.env,
      DATABASE_URL: "postgres://entitle:entitle@127.0.0.1:55433/entitle",
      ENTITLE_ADMIN_TOKEN: ADMIN_TOKEN,
      ENTITLE_ENCRYPTION_KEY: randomBytes(32).toString("base64"),
      ENTITLE_BIND: BIND,
      RUST_LOG: "warn",
    },
  });
  for (let i = 0; ; i++) {
    const up = await fetch(`${BASE}/healthz`).then((r) => r.ok, () => false);
    if (up) break;
    if (i > 100) throw new Error("server did not become healthy");
    await Bun.sleep(200);
  }

  console.log("== admin setup");
  const project = await admin<{ id: string }>("POST", "/projects", { name: `e2e ${new Date().toISOString()}` });
  const P = `/projects/${project.id}`;
  await admin("PATCH", P, { checkout_redirect_origins: [ORIGIN] });
  const key = await admin<{ key: string }>("POST", `${P}/api-keys`, { kind: "public" });
  await admin("PUT", `${P}/credentials/creem`, { api_key: "creem_test_fake", webhook_secret: WEBHOOK_SECRET, test_mode: true });
  const product = await admin<{ id: string }>("POST", `${P}/products`, { store: "creem", store_product_id: PRODUCT, display_name: "Lifetime" });
  const ent = await admin<{ id: string }>("POST", `${P}/entitlements`, { identifier: "pro" });
  await admin("PUT", `${P}/entitlements/${ent.id}/products/${product.id}`);

  console.log("== sdk");
  let requests = 0;
  const countingFetch: FetchLike = (url, init) => {
    requests++;
    return fetch(url, init);
  };
  const entitle = await Entitle.configure({ apiKey: key.key, baseUrl: BASE, storage: memoryStorage(), fetch: countingFetch });
  const anon = entitle.appUserId;
  check("anonymous id format", /^\$entitle_anon:[0-9a-f-]{36}$/.test(anon) && entitle.isAnonymous, anon);

  const updates: CustomerInfo[] = [];
  entitle.onCustomerInfoUpdated((i) => updates.push(i));

  const first = await entitle.getCustomerInfo();
  checkKeys("CustomerInfo", first);
  check("anonymous customer has no entitlements", Object.keys(first.entitlements).length === 0);

  const user = `e2e_${randomUUID()}`;
  const loginRaw = await fetch(`${BASE}/v1/customers/${encodeURIComponent(`$entitle_anon:${randomUUID()}`)}/login`, {
    method: "POST",
    headers: { Authorization: `Bearer ${key.key}`, "Content-Type": "application/json" },
    body: JSON.stringify({ new_app_user_id: `raw_${randomUUID()}` }),
  }).then((r) => r.json());
  checkKeys("LogInResponse", loginRaw);

  const login = await entitle.logIn(user);
  check("logIn created the user", login.created && entitle.appUserId === user && !entitle.isAnonymous);
  check("public key hides aliases", login.customerInfo.aliases === undefined, login.customerInfo.aliases);
  check("customer id kept across logIn", login.customerInfo.customer_id === first.customer_id);

  const before = requests;
  await entitle.getCustomerInfo();
  check("getCustomerInfo served from cache", requests === before);

  await expectError("checkout: success_url origin not allowed", entitle.createCheckout({ productId: PRODUCT, successUrl: "https://evil.example/x" }), ["bad_request"]);
  await expectError("checkout: unknown product", entitle.createCheckout({ productId: "prod_nope" }), ["not_found"]);
  // Fake Creem key: the request reaches Creem's test API and is rejected there.
  await expectError("checkout: fake Creem key rejected upstream", entitle.createCheckout({ productId: PRODUCT, successUrl: `${ORIGIN}/thanks` }), ["store_credentials", "store_unavailable"]);

  const badKey = await Entitle.configure({ apiKey: "pk_invalid", baseUrl: BASE, storage: memoryStorage() });
  await expectError("unknown key", badKey.getCustomerInfo(), ["unauthorized"]);
  const offline = await Entitle.configure({ apiKey: "pk_x", baseUrl: "http://127.0.0.1:9", storage: memoryStorage() });
  await expectError("network error", offline.getCustomerInfo(), ["network_error"]);

  console.log("== signed creem webhook");
  const event = {
    id: `evt_${randomUUID()}`,
    eventType: "checkout.completed",
    created_at: Date.now(),
    object: {
      id: `ch_${randomUUID()}`,
      object: "checkout",
      order: {
        id: `ord_${randomUUID()}`, customer: "cust_e2e", product: PRODUCT, amount: 4999,
        currency: "EUR", status: "paid", type: "onetime", created_at: new Date().toISOString(), mode: "test",
      },
      product: { id: PRODUCT, object: "product", price: 4999, currency: "EUR", billing_type: "onetime", mode: "test" },
      customer: { id: "cust_e2e", object: "customer", mode: "test" },
      status: "completed",
      metadata: { entitle_customer_id: login.customerInfo.customer_id },
      mode: "test",
    },
  };
  const body = JSON.stringify(event);
  const hook = await fetch(`${BASE}/webhooks/creem/${project.id}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", "creem-signature": createHmac("sha256", WEBHOOK_SECRET).update(body).digest("hex") },
    body,
  });
  check(`webhook accepted (${hook.status})`, hook.status === 200, await hook.text());

  check("cached info still without pro", !entitle.isEntitled("pro"));
  const fresh = await entitle.getCustomerInfo({ fresh: true });
  check("fresh info grants pro", entitle.isEntitled("pro") && fresh.entitlements.pro?.source === "creem", fresh.entitlements);
  checkKeys("CustomerInfo", fresh);
  checkKeys("ActiveEntitlement", fresh.entitlements.pro);
  const purchase = fresh.purchases[0];
  checkKeys("PurchaseView", purchase);
  checkKeys("Money", purchase?.price);
  check("purchase values", purchase?.kind === "one_time" && purchase.status === "active" && purchase.price?.amount_minor === 4999, purchase);
  check("listener fired for the change", updates.at(-1)?.entitlements.pro !== undefined, updates.length);
  check("account token is the customer id", (await entitle.getAccountToken()) === fresh.customer_id);

  // A real store round trip needs real credentials; with the fake key sync fails upstream.
  await expectError("sync with fake Creem key", entitle.syncPurchase({ store: "creem", id: "ch_unknown" }), ["store_credentials", "store_unavailable", "store_not_found"]);

  await entitle.logOut();
  check("logOut -> new anonymous id, empty cache", entitle.isAnonymous && entitle.appUserId !== anon && !entitle.isEntitled("pro"));

  const preflight = await fetch(`${BASE}/v1/customers/x`, {
    method: "OPTIONS",
    headers: { Origin: "https://app.example", "Access-Control-Request-Method": "GET", "Access-Control-Request-Headers": "authorization" },
  });
  console.log(`  info CORS preflight: ${preflight.status}, access-control-allow-origin=${preflight.headers.get("access-control-allow-origin")}`);
} catch (e) {
  failures++;
  console.error(e);
} finally {
  console.log("== teardown");
  if (server) {
    server.kill("SIGTERM");
    console.log(`  server exited with ${await server.exited}`);
  }
  await run(["docker", "compose", "-p", "entitle", "down"]).catch((e) => {
    failures++;
    console.error(e);
  });
}

console.log(failures === 0 ? "\nE2E PASSED" : `\nE2E FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
