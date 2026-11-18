import { afterEach, describe, expect, setSystemTime, test } from "bun:test";

import {
  ANONYMOUS_PREFIX,
  CACHE_TTL_MS,
  type CustomerInfo,
  Entitle,
  EntitleError,
  type EntitleStorage,
  type FetchLike,
  defaultStorage,
  memoryStorage,
} from "../src/index.js";

const BASE = "https://entitle.test";
const KEY_APP_USER_ID = `entitle:app_user_id:${BASE} pk_test`;
const KEY_CUSTOMER_INFO = `entitle:customer_info:${BASE} pk_test`;
const ANON_RE = /^\$entitle_anon:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

function info(appUserId: string, entitlements: string[] = [], extra: Partial<CustomerInfo> = {}): CustomerInfo {
  return {
    customer_id: "0b6f4c1e-5a0c-4a53-9d0e-2a1f7f0e9c11",
    app_user_id: appUserId,
    aliases: [appUserId],
    entitlements: Object.fromEntries(
      entitlements.map((id) => [
        id,
        {
          identifier: id,
          source: "creem",
          product_id: "prod_1",
          purchased_at: "2026-09-01T10:00:00Z",
          expires_at: null,
          period_type: null,
          will_renew: false,
          billing_issue: false,
          environment: "sandbox",
        },
      ]),
    ),
    purchases: [],
    management_url: null,
    first_seen_at: "2026-09-01T10:00:00Z",
    request_date: new Date().toISOString(),
    ...extra,
  };
}

type Call = { url: string; method: string; headers: Record<string, string>; body: unknown };
type Reply = Response | ((call: Call) => Response) | Error;

function mockFetch(...replies: Reply[]) {
  const calls: Call[] = [];
  const fetch: FetchLike = async (url, init) => {
    const call: Call = {
      url,
      method: init.method ?? "GET",
      headers: init.headers as Record<string, string>,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
    };
    calls.push(call);
    const reply = replies.shift();
    if (!reply) throw new Error(`unexpected request ${call.method} ${url}`);
    if (reply instanceof Error) throw reply;
    return typeof reply === "function" ? reply(call) : reply;
  };
  return { fetch, calls };
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
const apiError = (status: number, code: string, message: string) => json({ error: { code, message } }, status);

async function setup(opts: { storage?: EntitleStorage; appUserId?: string; replies?: Reply[] } = {}) {
  const storage = opts.storage ?? memoryStorage();
  const mock = mockFetch(...(opts.replies ?? []));
  const entitle = await Entitle.configure({
    apiKey: "pk_test",
    baseUrl: `${BASE}/`,
    storage,
    fetch: mock.fetch,
    ...(opts.appUserId === undefined ? {} : { appUserId: opts.appUserId }),
  });
  return { entitle, storage, calls: mock.calls };
}

afterEach(() => {
  setSystemTime();
});

describe("configure", () => {
  test("rejects non-public keys", async () => {
    await expect(Entitle.configure({ apiKey: "sk_secret", baseUrl: BASE })).rejects.toThrow(TypeError);
  });

  test("generates and persists an anonymous id", async () => {
    const storage = memoryStorage();
    const a = (await setup({ storage })).entitle;
    expect(a.appUserId).toMatch(ANON_RE);
    expect(a.isAnonymous).toBe(true);
    const b = (await setup({ storage })).entitle;
    expect(b.appUserId).toBe(a.appUserId);
  });

  test("an explicit appUserId wins and is persisted", async () => {
    const storage = memoryStorage();
    const { entitle } = await setup({ storage, appUserId: "user_42" });
    expect(entitle.appUserId).toBe("user_42");
    expect(entitle.isAnonymous).toBe(false);
    expect(await storage.get(KEY_APP_USER_ID)).toBe("user_42");
  });

  test("works with an async storage (Capacitor Preferences style)", async () => {
    const data = new Map<string, string>();
    const storage: EntitleStorage = {
      get: async (k) => data.get(k) ?? null,
      set: async (k, v) => void data.set(k, v),
      remove: async (k) => void data.delete(k),
    };
    const a = (await setup({ storage })).entitle;
    expect((await setup({ storage })).entitle.appUserId).toBe(a.appUserId);
  });

  test("a throwing storage does not break the SDK", async () => {
    const storage: EntitleStorage = {
      get: () => {
        throw new Error("denied");
      },
      set: async () => {
        throw new Error("quota");
      },
      remove: () => {
        throw new Error("denied");
      },
    };
    const { entitle } = await setup({ storage, replies: [json(info("x", ["pro"]))] });
    expect(entitle.appUserId).toMatch(ANON_RE);
    await entitle.getCustomerInfo();
    expect(entitle.isEntitled("pro")).toBe(true);
  });
});

describe("storage scope", () => {
  const configure = (storage: EntitleStorage, apiKey = "pk_test", baseUrl = BASE) =>
    Entitle.configure({ apiKey, baseUrl, storage });

  test("anonymous ids are separate per project and server", async () => {
    const storage = memoryStorage();
    const a = (await configure(storage)).appUserId;
    const otherProject = (await configure(storage, "pk_other")).appUserId;
    const otherServer = (await configure(storage, "pk_test", "https://other.test")).appUserId;
    expect(new Set([a, otherProject, otherServer]).size).toBe(3);
    expect((await configure(storage)).appUserId).toBe(a);
  });

  test("a legacy anonymous id is claimed by the first configuration only", async () => {
    const storage = memoryStorage();
    await storage.set("entitle:app_user_id", "$entitle_anon:legacy");
    expect((await configure(storage)).appUserId).toBe("$entitle_anon:legacy");
    expect(await storage.get("entitle:app_user_id")).toBeNull();
    expect((await configure(storage, "pk_other")).appUserId).toMatch(ANON_RE);
    expect((await configure(storage)).appUserId).toBe("$entitle_anon:legacy");
  });

  test("legacy keys stamped for another project are left to it", async () => {
    const storage = memoryStorage();
    await storage.set("entitle:app_user_id", "a");
    await storage.set(
      "entitle:customer_info",
      JSON.stringify({ scope: `${BASE} pk_other`, appUserId: "a", fetchedAt: Date.now(), info: info("a", ["pro"]) }),
    );
    const mine = await configure(storage);
    expect(mine.appUserId).toMatch(ANON_RE);
    expect(mine.isEntitled("pro")).toBe(false);
    // The project the legacy cache was stamped for adopts both, without a request.
    const theirs = await configure(storage, "pk_other");
    expect(theirs.appUserId).toBe("a");
    expect(theirs.isEntitled("pro")).toBe(true);
    expect(await storage.get("entitle:app_user_id")).toBeNull();
  });
});

describe("default storage", () => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "localStorage");
  afterEach(() => {
    if (original) Object.defineProperty(globalThis, "localStorage", original);
    else delete (globalThis as { localStorage?: unknown }).localStorage;
  });
  const setLocalStorage = (get: () => unknown) =>
    Object.defineProperty(globalThis, "localStorage", { configurable: true, get });

  test("uses localStorage when usable", async () => {
    const data = new Map<string, string>();
    setLocalStorage(() => ({
      getItem: (k: string) => data.get(k) ?? null,
      setItem: (k: string, v: string) => void data.set(k, v),
      removeItem: (k: string) => void data.delete(k),
    }));
    await defaultStorage().set("k", "v");
    expect(data.get("k")).toBe("v");
  });

  test("falls back to memory when localStorage throws or is missing", async () => {
    setLocalStorage(() => {
      throw new DOMException("denied", "SecurityError");
    });
    const s = defaultStorage();
    await s.set("k", "v");
    expect(await s.get("k")).toBe("v");
    setLocalStorage(() => undefined);
    expect(await defaultStorage().get("k")).toBeNull();
  });
});

describe("getCustomerInfo", () => {
  test("sends the key and URL-encodes the app user id", async () => {
    const { entitle, calls } = await setup({ replies: [json(info("x"))] });
    await entitle.getCustomerInfo();
    expect(calls[0]!.method).toBe("GET");
    expect(calls[0]!.url).toBe(`${BASE}/v1/customers/${encodeURIComponent(entitle.appUserId)}`);
    expect(calls[0]!.headers.Authorization).toBe("Bearer pk_test");
    expect(calls[0]!.headers["Content-Type"]).toBeUndefined();
  });

  test("caches until stale; fresh bypasses", async () => {
    setSystemTime(new Date("2026-10-01T00:00:00Z"));
    const { entitle, calls } = await setup({
      replies: [json(info("x")), json(info("x", ["pro"])), json(info("x", ["pro", "max"]))],
    });
    expect(entitle.isEntitled("pro")).toBe(false);
    await entitle.getCustomerInfo();
    await entitle.getCustomerInfo();
    expect(calls).toHaveLength(1);

    await entitle.getCustomerInfo({ fresh: true });
    expect(calls).toHaveLength(2);
    expect(entitle.isEntitled("pro")).toBe(true);

    setSystemTime(new Date(Date.now() + CACHE_TTL_MS + 1));
    await entitle.getCustomerInfo();
    expect(calls).toHaveLength(3);
    expect(Object.keys(entitle.activeEntitlements).sort()).toEqual(["max", "pro"]);
  });

  test("the persisted cache is used after a restart", async () => {
    const storage = memoryStorage();
    const first = await setup({ storage, replies: [json(info("x", ["pro"]))] });
    await first.entitle.getCustomerInfo();
    const second = await setup({ storage });
    expect(second.entitle.isEntitled("pro")).toBe(true);
    await second.entitle.getCustomerInfo();
    expect(second.calls).toHaveLength(0);
  });

  test("a cache for another app user id is ignored", async () => {
    const storage = memoryStorage();
    const first = await setup({ storage, appUserId: "a", replies: [json(info("a", ["pro"]))] });
    await first.entitle.getCustomerInfo();
    const second = await setup({ storage, appUserId: "b" });
    expect(second.entitle.isEntitled("pro")).toBe(false);
  });

  test("a cache from another project or server is ignored", async () => {
    const storage = memoryStorage();
    const first = await setup({ storage, appUserId: "a", replies: [json(info("a", ["pro"]))] });
    await first.entitle.getCustomerInfo();
    for (const [apiKey, baseUrl] of [
      ["pk_other", BASE],
      ["pk_test", "https://other.test"],
    ]) {
      const other = await Entitle.configure({ apiKey: apiKey!, baseUrl: baseUrl!, storage, appUserId: "a" });
      expect(other.isEntitled("pro")).toBe(false);
    }
    expect((await setup({ storage, appUserId: "a" })).entitle.isEntitled("pro")).toBe(true);
  });

  test("an unscoped cache from an older SDK is refetched, never adopted", async () => {
    const storage = memoryStorage();
    await storage.set("entitle:app_user_id", "a");
    await storage.set("entitle:customer_info", JSON.stringify({ appUserId: "a", fetchedAt: Date.now(), info: info("a", ["pro"]) }));
    const { entitle, calls } = await setup({ storage, replies: [json(info("a"))] });
    expect(entitle.appUserId).toBe("a");
    expect(entitle.isEntitled("pro")).toBe(false);
    await entitle.getCustomerInfo();
    expect(calls).toHaveLength(1);
    expect(await storage.get("entitle:customer_info")).toBeNull();
  });

  test("locally expired entitlements are not active", async () => {
    setSystemTime(new Date("2026-10-01T00:00:00Z"));
    const i = info("x", ["pro"]);
    i.entitlements.pro!.expires_at = "2026-10-01T00:01:00Z";
    const { entitle } = await setup({ replies: [json(i)] });
    await entitle.getCustomerInfo();
    expect(entitle.isEntitled("pro")).toBe(true);
    setSystemTime(new Date("2026-10-01T00:02:00Z"));
    expect(entitle.isEntitled("pro")).toBe(false);
  });
});

describe("identity", () => {
  test("logIn switches the user, caches and persists", async () => {
    const storage = memoryStorage();
    const { entitle, calls } = await setup({
      storage,
      replies: [json({ created: true, customer_info: info("user_42", ["pro"]) })],
    });
    const anon = entitle.appUserId;
    const res = await entitle.logIn("user_42");
    expect(res.created).toBe(true);
    expect(res.customerInfo.app_user_id).toBe("user_42");
    expect(calls[0]!.method).toBe("POST");
    expect(calls[0]!.url).toBe(`${BASE}/v1/customers/${encodeURIComponent(anon)}/login`);
    expect(calls[0]!.headers["Content-Type"]).toBe("application/json");
    expect(calls[0]!.body).toEqual({ new_app_user_id: "user_42" });
    expect(entitle.appUserId).toBe("user_42");
    expect(entitle.isAnonymous).toBe(false);
    expect(entitle.isEntitled("pro")).toBe(true);
    await entitle.getCustomerInfo();
    expect(calls).toHaveLength(1);
    expect((await setup({ storage })).entitle.appUserId).toBe("user_42");
  });

  test("identityToken signs identified users only, and the logIn target", async () => {
    const mock = mockFetch(
      json({ created: true, customer_info: info("user_42") }),
      json(info("user_42")),
    );
    const asked: string[] = [];
    const entitle = await Entitle.configure({
      apiKey: "pk_test",
      baseUrl: BASE,
      storage: memoryStorage(),
      fetch: mock.fetch,
      identityToken: async (id) => {
        asked.push(id);
        return `jwt-for-${id}`;
      },
    });
    await entitle.logIn("user_42");
    // Anonymous source: no header; the target's token goes in the body.
    expect(mock.calls[0]!.headers["X-Entitle-Identity"]).toBeUndefined();
    expect(mock.calls[0]!.body).toEqual({ new_app_user_id: "user_42", identity_token: "jwt-for-user_42" });
    await entitle.getCustomerInfo({ fresh: true });
    expect(mock.calls[1]!.headers["X-Entitle-Identity"]).toBe("jwt-for-user_42");
    expect(asked).toEqual(["user_42", "user_42"]);
  });

  test("logOut starts a new anonymous id and clears the cache", async () => {
    const storage = memoryStorage();
    const { entitle } = await setup({ storage, appUserId: "user_42", replies: [json(info("user_42", ["pro"]))] });
    await entitle.getCustomerInfo();
    await entitle.logOut();
    expect(entitle.appUserId).toMatch(ANON_RE);
    expect(entitle.appUserId.startsWith(ANONYMOUS_PREFIX)).toBe(true);
    expect(entitle.isEntitled("pro")).toBe(false);
    expect(await storage.get(KEY_CUSTOMER_INFO)).toBeNull();
    expect(await storage.get(KEY_APP_USER_ID)).toBe(entitle.appUserId);
  });

  test("a response for a user who logged out meanwhile is not cached", async () => {
    const { entitle } = await setup({ appUserId: "user_42", replies: [json(info("user_42", ["pro"]))] });
    const pending = entitle.getCustomerInfo(); // in flight while the user logs out
    await entitle.logOut();
    await pending;
    expect(entitle.isEntitled("pro")).toBe(false);
  });

  test("getAccountToken returns the customer id", async () => {
    const { entitle } = await setup({ replies: [json(info("x"))] });
    expect(await entitle.getAccountToken()).toBe("0b6f4c1e-5a0c-4a53-9d0e-2a1f7f0e9c11");
  });
});

describe("purchases", () => {
  test("createCheckout maps options to the request body", async () => {
    const { entitle, calls } = await setup({
      appUserId: "user_42",
      replies: [json({ id: "ch_1", url: "https://checkout.creem.io/ch_1" })],
    });
    const session = await entitle.createCheckout({
      productId: "prod_1",
      successUrl: "https://example.com/thanks",
      metadata: { campaign: "spring" },
    });
    expect(session).toEqual({ id: "ch_1", url: "https://checkout.creem.io/ch_1" });
    expect(calls[0]!.url).toBe(`${BASE}/v1/customers/user_42/checkout`);
    expect(calls[0]!.body).toEqual({
      product_id: "prod_1",
      success_url: "https://example.com/thanks",
      metadata: { campaign: "spring" },
    });
  });

  test("createCheckout omits unset optional fields", async () => {
    const { entitle, calls } = await setup({ replies: [json({ id: "ch_1", url: "u" })] });
    await entitle.createCheckout({ productId: "prod_1" });
    expect(calls[0]!.body).toEqual({ product_id: "prod_1" });
  });

  test("syncPurchase updates the cache", async () => {
    const { entitle, calls } = await setup({ appUserId: "u", replies: [json(info("u", ["pro"]))] });
    const res = await entitle.syncPurchase({ store: "app_store", id: "2000000134567890" });
    expect(res.entitlements.pro).toBeDefined();
    expect(calls[0]!.url).toBe(`${BASE}/v1/customers/u/sync`);
    expect(calls[0]!.body).toEqual({ store: "app_store", id: "2000000134567890" });
    expect(entitle.isEntitled("pro")).toBe(true);
  });

  test("syncPurchase surfaces transfer_refused", async () => {
    const { entitle } = await setup({
      replies: [apiError(409, "transfer_refused", "this purchase belongs to another customer")],
    });
    const err = await entitle.syncPurchase({ store: "play_store", id: "token" }).catch((e) => e);
    expect(err).toBeInstanceOf(EntitleError);
    expect(err.status).toBe(409);
    expect(err.code).toBe("transfer_refused");
    expect(err.message).toBe("this purchase belongs to another customer");
    expect(err.isNetworkError).toBe(false);
  });
});

describe("errors", () => {
  test("parses the error envelope", async () => {
    const { entitle } = await setup({ replies: [apiError(401, "unauthorized", "missing or invalid credentials")] });
    const err = await entitle.getCustomerInfo().catch((e) => e);
    expect(err).toBeInstanceOf(EntitleError);
    expect(err).toMatchObject({ status: 401, code: "unauthorized", message: "missing or invalid credentials" });
  });

  test("network failures are distinguishable", async () => {
    const cause = new TypeError("Failed to fetch");
    const { entitle } = await setup({ replies: [cause] });
    const err = await entitle.getCustomerInfo().catch((e) => e);
    expect(err).toMatchObject({ status: 0, code: "network_error", isNetworkError: true });
    expect(err.cause).toBe(cause);
  });

  test("non-JSON error bodies are unexpected_response", async () => {
    const { entitle } = await setup({ replies: [new Response("no available server", { status: 503 })] });
    await expect(entitle.getCustomerInfo()).rejects.toMatchObject({ status: 503, code: "unexpected_response" });
  });

  test("a non-JSON success body is unexpected_response", async () => {
    const { entitle } = await setup({ replies: [new Response("<html>", { status: 200 })] });
    await expect(entitle.getCustomerInfo()).rejects.toMatchObject({ status: 200, code: "unexpected_response" });
  });
});

describe("onCustomerInfoUpdated", () => {
  test("fires on change only, and stops after unsubscribe", async () => {
    const same = info("u", ["pro"]);
    const { entitle } = await setup({
      appUserId: "u",
      replies: [
        json(same),
        json({ ...same, request_date: "2030-01-01T00:00:00Z" }),
        json(info("u", ["pro", "max"])),
        json(info("u")),
      ],
    });
    const seen: string[][] = [];
    const off = entitle.onCustomerInfoUpdated((i) => seen.push(Object.keys(i.entitlements).sort()));
    await entitle.getCustomerInfo({ fresh: true });
    await entitle.getCustomerInfo({ fresh: true }); // only request_date differs
    await entitle.getCustomerInfo({ fresh: true });
    off();
    await entitle.getCustomerInfo({ fresh: true });
    expect(seen).toEqual([["pro"], ["max", "pro"]]);
  });

  test("a throwing listener does not break the call", async () => {
    const { entitle } = await setup({ replies: [json(info("x"))] });
    const errors: unknown[] = [];
    const original = console.error;
    console.error = (...args: unknown[]) => void errors.push(args);
    try {
      entitle.onCustomerInfoUpdated(() => {
        throw new Error("boom");
      });
      await expect(entitle.getCustomerInfo()).resolves.toBeDefined();
    } finally {
      console.error = original;
    }
    expect(errors).toHaveLength(1);
  });
});
