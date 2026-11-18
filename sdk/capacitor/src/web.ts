import { WebPlugin } from '@capacitor/core';

import type { EntitleStorePlugin, NativePurchaseResult, StoreProduct, StorePurchase } from './definitions.js';

/** No store on the web: use `entitle.createCheckout()` (Creem) there. */
export class EntitleStoreWeb extends WebPlugin implements EntitleStorePlugin {
  async getProducts(): Promise<{ products: StoreProduct[] }> {
    throw this.unimplemented('Not available on web.');
  }

  async purchase(): Promise<NativePurchaseResult> {
    throw this.unimplemented('Not available on web.');
  }

  async finishTransaction(): Promise<void> {
    throw this.unimplemented('Not available on web.');
  }

  async getCurrentPurchases(): Promise<{ purchases: StorePurchase[] }> {
    throw this.unimplemented('Not available on web.');
  }
}
