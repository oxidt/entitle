import type { CustomerInfo, Entitle } from '@oxidt/entitle';

import type { StorePurchase } from './definitions.js';
import { EntitleStore } from './plugin.js';

export type PurchaseResult =
  | { status: 'purchased'; purchase: StorePurchase; customerInfo: CustomerInfo }
  /** The user closed the payment sheet. */
  | { status: 'cancelled' }
  /** Awaiting approval or payment; `startPurchaseSync` syncs it once it completes. */
  | { status: 'pending' };

export interface PurchaseSyncOptions {
  /** A purchase update failed to sync (it stays unfinished on iOS and is retried on the next launch). Default: `console.error`. */
  onError?: (error: unknown, purchase: StorePurchase) => void;
}

/**
 * Syncs in flight per SDK instance, by app user id and purchase: `purchase()` and
 * `startPurchaseSync` share one sync per purchase, but never across instances or customers.
 */
const inFlight = new WeakMap<Entitle, Map<string, Promise<CustomerInfo>>>();

/**
 * Buys `productId` in the native store for the current entitle customer, syncs it with
 * entitle and (iOS) finishes the transaction only after the sync succeeded.
 *
 * Cancelled and pending purchases resolve as `{ status }`. Sync failures reject with the SDK's
 * `EntitleError` (e.g. `code: "transfer_refused"`); the iOS transaction then stays unfinished and
 * is delivered again via `startPurchaseSync` on the next launch.
 */
export async function purchase(
  entitle: Entitle,
  options: { productId: string; offerToken?: string },
): Promise<PurchaseResult> {
  const accountToken = await entitle.getAccountToken();
  const result = await EntitleStore.purchase({ ...options, accountToken });
  if (result.status !== 'purchased') return result;
  const customerInfo = await syncAndFinish(entitle, result.purchase);
  return { status: 'purchased', purchase: result.purchase, customerInfo };
}

/**
 * Syncs every purchase the store reports as owned and returns the resulting customer info.
 * All purchases are attempted; if any failed, rejects with the first error afterwards.
 */
export async function restorePurchases(entitle: Entitle): Promise<CustomerInfo> {
  const { purchases } = await EntitleStore.getCurrentPurchases();
  let info: CustomerInfo | undefined;
  let firstError: unknown;
  let failed = false;
  for (const p of purchases) {
    try {
      info = await syncAndFinish(entitle, p);
    } catch (e) {
      if (!failed) firstError = e;
      failed = true;
    }
  }
  if (failed) throw firstError;
  return info ?? entitle.getCustomerInfo({ fresh: true });
}

/**
 * Syncs (and on iOS finishes) every purchase the store reports outside `purchase()`: renewals,
 * Ask to Buy approvals, purchases on other devices, refunds, pending → purchased. Call once at
 * app start; updates that arrived earlier are delivered then. Returns an unsubscribe function.
 */
export function startPurchaseSync(entitle: Entitle, options: PurchaseSyncOptions = {}): () => void {
  const onError =
    options.onError ?? ((e: unknown) => console.error('EntitleStore: syncing a purchase update failed', e));
  const handle = EntitleStore.addListener('purchaseUpdated', (p) => {
    syncAndFinish(entitle, p).catch((e: unknown) => onError(e, p));
  });
  return () => {
    void handle.then((h) => h.remove());
  };
}

function syncAndFinish(entitle: Entitle, p: StorePurchase): Promise<CustomerInfo> {
  const forUser = entitle.appUserId;
  const key = `${forUser}\n${p.store === 'app_store' ? `app_store:${p.transactionId}` : `play_store:${p.purchaseToken}`}`;
  let running = inFlight.get(entitle);
  if (!running) inFlight.set(entitle, (running = new Map()));
  const existing = running.get(key);
  if (existing) return existing;
  const sync = (async () => {
    let info: CustomerInfo;
    if (p.store === 'play_store') {
      // The entitle server acknowledges the purchase after verifying it.
      info = await entitle.syncPurchase({ store: 'play_store', id: p.purchaseToken });
    } else {
      info = await entitle.syncPurchase({ store: 'app_store', id: p.transactionId });
      await EntitleStore.finishTransaction({ transactionId: p.transactionId });
    }
    // The account switched while syncing: that info is the previous customer's.
    return entitle.appUserId === forUser ? info : entitle.getCustomerInfo();
  })().finally(() => running.delete(key));
  running.set(key, sync);
  return sync;
}
