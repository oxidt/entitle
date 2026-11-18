import { registerPlugin } from '@capacitor/core';

import type { EntitleStorePlugin } from './definitions.js';

/** The native store layer. See `purchase`, `restorePurchases` and `startPurchaseSync` for the entitle flows. */
export const EntitleStore = registerPlugin<EntitleStorePlugin>('EntitleStore', {
  web: () => import('./web.js').then((m) => new m.EntitleStoreWeb()),
});
