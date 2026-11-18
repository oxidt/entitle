import { EntitleError } from "./errors.js";
import type { ActiveEntitlement } from "./generated/ActiveEntitlement.js";
import type { CheckoutRequest } from "./generated/CheckoutRequest.js";
import type { CheckoutSession } from "./generated/CheckoutSession.js";
import type { CustomerInfo } from "./generated/CustomerInfo.js";
import type { LogInRequest } from "./generated/LogInRequest.js";
import type { LogInResponse } from "./generated/LogInResponse.js";
import type { SyncRequest } from "./generated/SyncRequest.js";
import { defaultStorage, type EntitleStorage } from "./storage.js";

/** Prefix of SDK-generated anonymous app user ids: `$entitle_anon:<uuid v4>`. */
export const ANONYMOUS_PREFIX = "$entitle_anon:";

/** `getCustomerInfo()` returns the cached info while it is younger than this. */
export const CACHE_TTL_MS = 5 * 60 * 1000;

const KEY_APP_USER_ID = "entitle:app_user_id";
const KEY_CUSTOMER_INFO = "entitle:customer_info";

export type FetchLike = (url: string, init: RequestInit) => Promise<Response>;

export interface EntitleOptions {
  /** Public key (`pk_…`). Secret keys must never ship in a client. */
  apiKey: string;
  /** e.g. `https://entitle.example.com` */
  baseUrl: string;
  /** Your user id. Omit to use a persisted anonymous id and call `logIn()` later. */
  appUserId?: string;
  /** Defaults to `localStorage` when usable, else in-memory. */
  storage?: EntitleStorage;
  /** Defaults to the global `fetch`. */
  fetch?: FetchLike;
  /**
   * Returns a signed identity token for an identified app user id, minted by your
   * backend with the project's identity secret (see docs/api.md, "Signed identity").
   * Called before each request for an identified user; anonymous ids need none.
   * Required once the project sets `require_signed_identity`.
   */
  identityToken?: (appUserId: string) => string | Promise<string>;
}

export interface CheckoutOptions {
  /** Creem product id (`prod_…`), configured as a `creem` product in the project. */
  productId: string;
  /** Must be on one of the project's `checkout_redirect_origins`. */
  successUrl?: string;
  metadata?: Record<string, string>;
}

export interface LogInResult {
  customerInfo: CustomerInfo;
  /** `true` if the app user id had never been seen before. */
  created: boolean;
}

interface CachedInfo {
  /** Server and project the info came from: storage may be shared between configurations. */
  scope: string;
  appUserId: string;
  fetchedAt: number;
  info: CustomerInfo;
}

export class Entitle {
  private cache: CachedInfo | null;
  private readonly listeners = new Set<(info: CustomerInfo) => void>();

  private constructor(
    private readonly apiKey: string,
    private readonly baseUrl: string,
    private readonly storage: EntitleStorage,
    private readonly fetchFn: FetchLike,
    private userId: string,
    cache: CachedInfo | null,
    private readonly identityToken: EntitleOptions["identityToken"],
  ) {
    this.cache = cache;
  }

  static async configure(options: EntitleOptions): Promise<Entitle> {
    if (!options.apiKey.startsWith("pk_")) {
      throw new TypeError("Entitle: apiKey must be a public key (pk_…)");
    }
    const storage = options.storage ?? defaultStorage();
    const stored = await read(storage, KEY_APP_USER_ID);
    const appUserId = options.appUserId ?? stored ?? ANONYMOUS_PREFIX + uuidV4();
    if (appUserId !== stored) await write(storage, KEY_APP_USER_ID, appUserId);

    const baseUrl = options.baseUrl.replace(/\/+$/, "");
    const scope = `${baseUrl} ${options.apiKey}`;
    let cache: CachedInfo | null = null;
    try {
      const raw = await read(storage, KEY_CUSTOMER_INFO);
      const parsed = raw ? (JSON.parse(raw) as CachedInfo) : null;
      // No scope: written by an older SDK. The first configuration after the update
      // adopts it (so an offline unlock survives) and stamps it; any other one then
      // ignores it.
      if (parsed && parsed.scope === undefined && parsed.appUserId === appUserId) {
        cache = { ...parsed, scope };
        await write(storage, KEY_CUSTOMER_INFO, JSON.stringify(cache));
      } else if (parsed?.scope === scope && parsed.appUserId === appUserId) {
        cache = parsed;
      }
    } catch {
      // corrupt cache: refetch
    }

    const fetchFn = options.fetch ?? ((url, init) => globalThis.fetch(url, init));
    return new Entitle(
      options.apiKey,
      baseUrl,
      storage,
      fetchFn,
      appUserId,
      cache,
      options.identityToken,
    );
  }

  get appUserId(): string {
    return this.userId;
  }

  get isAnonymous(): boolean {
    return this.userId.startsWith(ANONYMOUS_PREFIX);
  }

  /** Active entitlements from the cached customer info (sync; empty before the first fetch). */
  get activeEntitlements(): Record<string, ActiveEntitlement> {
    const now = Date.now();
    const active: Record<string, ActiveEntitlement> = {};
    for (const [id, e] of Object.entries(this.cache?.info.entitlements ?? {})) {
      if (e && (e.expires_at === null || Date.parse(e.expires_at) > now)) active[id] = e;
    }
    return active;
  }

  /** Whether the cached customer info grants `identifier` (sync). */
  isEntitled(identifier: string): boolean {
    return identifier in this.activeEntitlements;
  }

  /** Cached for {@link CACHE_TTL_MS}; `fresh: true` always asks the server. */
  async getCustomerInfo(options: { fresh?: boolean } = {}): Promise<CustomerInfo> {
    const c = this.cache;
    if (!options.fresh && c && c.appUserId === this.userId && Date.now() - c.fetchedAt < CACHE_TTL_MS) {
      return c.info;
    }
    const forUser = this.userId;
    const info = await this.request<CustomerInfo>("GET", this.customerPath(forUser), forUser);
    await this.update(forUser, info);
    return info;
  }

  /**
   * The customer's stable UUID (`customer_id`). Pass it as StoreKit 2 `appAccountToken`
   * and Google Play obfuscated account id so store notifications reach this customer.
   */
  async getAccountToken(): Promise<string> {
    return (await this.getCustomerInfo()).customer_id;
  }

  /** Switches to a known app user id, merging the anonymous customer into it (see docs/api.md). */
  async logIn(appUserId: string): Promise<LogInResult> {
    const body: LogInRequest = { new_app_user_id: appUserId };
    const token = await this.tokenFor(appUserId);
    if (token !== undefined) body.identity_token = token;
    const res = await this.request<LogInResponse>(
      "POST",
      `${this.customerPath(this.userId)}/login`,
      this.userId,
      body,
    );
    await this.switchUser(appUserId);
    await this.update(appUserId, res.customer_info);
    return { customerInfo: res.customer_info, created: res.created };
  }

  /** Starts over with a new anonymous app user id and clears the cache. */
  async logOut(): Promise<void> {
    await this.switchUser(ANONYMOUS_PREFIX + uuidV4());
  }

  /** Creates a Creem hosted checkout. Open `url` yourself (redirect or in-app browser). */
  async createCheckout(options: CheckoutOptions): Promise<CheckoutSession> {
    const body: CheckoutRequest = { product_id: options.productId };
    if (options.successUrl !== undefined) body.success_url = options.successUrl;
    if (options.metadata !== undefined) body.metadata = options.metadata;
    return this.request<CheckoutSession>(
      "POST",
      `${this.customerPath(this.userId)}/checkout`,
      this.userId,
      body,
    );
  }

  /**
   * Links a store purchase to this customer and returns the updated info.
   * `id`: StoreKit 2 transaction id, Play purchase token, or Creem `sub_…` / `ch_…` id.
   * Rejects with `code: "transfer_refused"` (409) if the purchase provably belongs to
   * another customer.
   */
  async syncPurchase(purchase: SyncRequest): Promise<CustomerInfo> {
    const forUser = this.userId;
    const body: SyncRequest = { store: purchase.store, id: purchase.id };
    const info = await this.request<CustomerInfo>(
      "POST",
      `${this.customerPath(forUser)}/sync`,
      forUser,
      body,
    );
    await this.update(forUser, info);
    return info;
  }

  /** Called whenever fetched customer info differs from the cached one. Returns an unsubscribe fn. */
  onCustomerInfoUpdated(listener: (info: CustomerInfo) => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private customerPath(appUserId: string): string {
    return `/v1/customers/${encodeURIComponent(appUserId)}`;
  }

  private async switchUser(appUserId: string): Promise<void> {
    if (appUserId === this.userId) return;
    this.userId = appUserId;
    this.cache = null;
    await write(this.storage, KEY_APP_USER_ID, appUserId);
    await remove(this.storage, KEY_CUSTOMER_INFO);
  }

  private async update(forUser: string, info: CustomerInfo): Promise<void> {
    // The user switched while the request was in flight: don't cache someone else's info.
    if (forUser !== this.userId) return;
    const previous = this.cache?.info;
    this.cache = { scope: `${this.baseUrl} ${this.apiKey}`, appUserId: forUser, fetchedAt: Date.now(), info };
    await write(this.storage, KEY_CUSTOMER_INFO, JSON.stringify(this.cache));
    if (previous && sameInfo(previous, info)) return;
    for (const listener of [...this.listeners]) {
      try {
        listener(info);
      } catch (e) {
        console.error("Entitle: onCustomerInfoUpdated listener threw", e);
      }
    }
  }

  private async tokenFor(appUserId: string): Promise<string | undefined> {
    if (!this.identityToken || appUserId.startsWith(ANONYMOUS_PREFIX)) return undefined;
    return this.identityToken(appUserId);
  }

  /** `asUser`: the app user id in `path`, proven with an identity token if configured. */
  private async request<T>(method: string, path: string, asUser: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { Authorization: `Bearer ${this.apiKey}` };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    const token = await this.tokenFor(asUser);
    if (token !== undefined) headers["X-Entitle-Identity"] = token;
    let status = 0;
    let text: string;
    try {
      const res = await this.fetchFn(this.baseUrl + path, {
        method,
        headers,
        body: body === undefined ? null : JSON.stringify(body),
      });
      status = res.status;
      text = await res.text();
    } catch (e) {
      throw new EntitleError(0, "network_error", `network error: ${String(e)}`, e);
    }
    let json: unknown;
    try {
      json = JSON.parse(text);
    } catch {
      json = undefined;
    }
    if (status >= 200 && status < 300) {
      if (json === undefined) {
        throw new EntitleError(status, "unexpected_response", "response is not JSON");
      }
      return json as T;
    }
    const err = (json as { error?: { code?: unknown; message?: unknown } } | undefined)?.error;
    if (err && typeof err.code === "string") {
      throw new EntitleError(status, err.code, typeof err.message === "string" ? err.message : err.code);
    }
    throw new EntitleError(status, "unexpected_response", `HTTP ${status}`);
  }
}

/** Equal apart from `request_date`, which changes on every response. */
function sameInfo(a: CustomerInfo, b: CustomerInfo): boolean {
  return JSON.stringify({ ...a, request_date: "" }) === JSON.stringify({ ...b, request_date: "" });
}

// Persistence is best effort: a failing storage must not break purchases.
async function read(storage: EntitleStorage, key: string): Promise<string | null> {
  try {
    return (await storage.get(key)) ?? null;
  } catch {
    return null;
  }
}

async function write(storage: EntitleStorage, key: string, value: string): Promise<void> {
  try {
    await storage.set(key, value);
  } catch {
    // ignore
  }
}

async function remove(storage: EntitleStorage, key: string): Promise<void> {
  try {
    await storage.remove(key);
  } catch {
    // ignore
  }
}

/** `crypto.randomUUID` needs a secure context; fall back to `getRandomValues` (e.g. http LAN dev). */
function uuidV4(): string {
  const c = globalThis.crypto;
  if (typeof c?.randomUUID === "function") return c.randomUUID();
  if (typeof c?.getRandomValues !== "function") {
    throw new Error("Entitle: no Web Crypto available; pass appUserId to Entitle.configure");
  }
  const b = c.getRandomValues(new Uint8Array(16));
  b[6] = (b[6]! & 0x0f) | 0x40;
  b[8] = (b[8]! & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}
