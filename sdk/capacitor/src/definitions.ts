import type { PluginListenerHandle } from '@capacitor/core';

/** A price in the currency's ISO 4217 minor unit (cents for EUR, yen for JPY), like entitle's `Money`. */
export interface StorePrice {
  /** Localized, e.g. `4,99 €`. */
  formatted: string;
  amountMinor: number;
  /** ISO 4217, e.g. `EUR`. */
  currency: string;
}

/** One phase of a Google Play subscription offer (free trial, intro price, recurring price). */
export interface PlayPricingPhase {
  price: StorePrice;
  /** ISO 8601 duration, e.g. `P1M`. */
  billingPeriod: string;
  /** Number of billing periods; 0 for the infinitely recurring phase. */
  billingCycleCount: number;
}

/** Google Play only: a base plan / offer of a subscription, or a purchase option of a one-time product. */
export interface PlayOffer {
  /** Pass to `purchase({ offerToken })`. */
  offerToken: string;
  /** Subscriptions only. */
  basePlanId?: string;
  /** Absent for a subscription's base plan itself. */
  offerId?: string;
  tags: string[];
  /** Subscriptions: phases in order, the last one recurs. One-time products: a single phase with `billingPeriod: ""`. */
  pricingPhases: PlayPricingPhase[];
}

export interface StoreProduct {
  productId: string;
  kind: 'subscription' | 'one_time';
  title: string;
  description: string;
  /** Subscriptions: the recurring price (Google: of the base plan). */
  price: StorePrice;
  /** Subscriptions only: ISO 8601 duration, e.g. `P1M`, `P1Y`. */
  subscriptionPeriod?: string;
  /** Google Play only. */
  offers?: PlayOffer[];
}

/** A store purchase, identified the way `entitle.syncPurchase()` expects it. */
export type StorePurchase =
  | { store: 'app_store'; transactionId: string; productId: string }
  | { store: 'play_store'; purchaseToken: string; productId: string };

export type NativePurchaseResult =
  | { status: 'purchased'; purchase: StorePurchase }
  /** The user closed the payment sheet. */
  | { status: 'cancelled' }
  /** Awaiting approval (Ask to Buy) or payment (Play pending purchase); arrives later via `purchaseUpdated`. */
  | { status: 'pending' };

export interface PurchaseOptions {
  productId: string;
  /** `await entitle.getAccountToken()`: set as StoreKit `appAccountToken` / Play obfuscated account id. */
  accountToken: string;
  /** Google Play only: the offer to buy. Default: the subscription's base plan / the product's first purchase option. */
  offerToken?: string;
}

/**
 * The native store layer. Most apps use the helpers (`purchase`, `restorePurchases`,
 * `startPurchaseSync`) instead, which sync each purchase with entitle.
 *
 * Errors reject with a Capacitor error whose `code` is e.g. `UNIMPLEMENTED` (web),
 * `PRODUCT_NOT_FOUND`, `UNVERIFIED` (iOS) or the Play `BillingResponseCode` name
 * (`ITEM_ALREADY_OWNED`, `SERVICE_UNAVAILABLE`, …).
 */
export interface EntitleStorePlugin {
  /** Unknown ids are left out of the result. */
  getProducts(options: { productIds: string[] }): Promise<{ products: StoreProduct[] }>;

  /**
   * Starts the store purchase. Never finishes (iOS) or acknowledges (Android) it: sync it with
   * entitle first, then call `finishTransaction` (iOS). The entitle server acknowledges Play purchases.
   */
  purchase(options: PurchaseOptions): Promise<NativePurchaseResult>;

  /** iOS: finishes the transaction after entitle synced it. Android: no-op. */
  finishTransaction(options: { transactionId: string }): Promise<void>;

  /** iOS `Transaction.currentEntitlements`; Android owned subscriptions + one-time products. */
  getCurrentPurchases(): Promise<{ purchases: StorePurchase[] }>;

  /**
   * Purchases not started by `purchase()`: iOS `Transaction.updates` (renewals, Ask to Buy,
   * other devices, refunds, unfinished transactions at launch); Android pending → purchased.
   * Events that fire before a listener is added are kept until one is.
   */
  addListener(
    eventName: 'purchaseUpdated',
    listener: (purchase: StorePurchase) => void,
  ): Promise<PluginListenerHandle>;

  removeAllListeners(): Promise<void>;
}
