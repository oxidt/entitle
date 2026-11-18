import { Capacitor } from '@capacitor/core';
import { Preferences } from '@capacitor/preferences';
import { Entitle, EntitleError } from '@oxidt/entitle';
import { EntitleStore, purchase, restorePurchases, startPurchaseSync } from '@oxidt/entitle-capacitor';

import { API_KEY, ENTITLEMENT, PRODUCT_ID, SERVER_URL } from './config';

const $ = (id: string) => document.getElementById(id)!;
const buy = $('buy') as HTMLButtonElement;
const restore = $('restore') as HTMLButtonElement;

function log(message: string) {
  $('log').textContent = `${new Date().toLocaleTimeString()} ${message}\n${$('log').textContent}`;
}

/** `EntitleError` from the server (e.g. `transfer_refused`), or a native store error with a `code`. */
function logError(action: string, e: unknown) {
  if (e instanceof EntitleError) log(`${action} failed: ${e.code} (${e.status}) ${e.message}`);
  else log(`${action} failed: ${(e as { code?: string }).code ?? ''} ${e instanceof Error ? e.message : String(e)}`);
}

const entitle = await Entitle.configure({
  apiKey: API_KEY,
  baseUrl: SERVER_URL,
  storage: {
    get: async (key) => (await Preferences.get({ key })).value,
    set: (key, value) => Preferences.set({ key, value }),
    remove: (key) => Preferences.remove({ key }),
  },
});

function render() {
  const active = entitle.isEntitled(ENTITLEMENT);
  $('status').textContent = active ? 'active' : 'not active';
  $('status').className = active ? 'on' : 'off';
  $('user').textContent = entitle.appUserId;
}

$('entitlement').textContent = ENTITLEMENT;
entitle.onCustomerInfoUpdated(render);
render();

if (Capacitor.isNativePlatform()) {
  startPurchaseSync(entitle, { onError: (e) => logError('Background sync', e) }); // once, at app start
  buy.disabled = restore.disabled = false;
  EntitleStore.getProducts({ productIds: [PRODUCT_ID] })
    .then(({ products }) => {
      const p = products[0];
      if (p) buy.textContent = `Buy ${p.title} (${p.price.formatted})`;
      else log(`Product ${PRODUCT_ID} not found in the store`);
    })
    .catch((e) => logError('Loading the product', e));
} else {
  log('Web: store purchases need iOS or Android.');
}

entitle
  .getCustomerInfo({ fresh: true })
  .then(render)
  .catch((e) => logError('Loading customer info', e));

buy.onclick = async () => {
  buy.disabled = true;
  try {
    const result = await purchase(entitle, { productId: PRODUCT_ID });
    if (result.status === 'purchased') log(`Purchased ${result.purchase.productId} and synced`);
    else if (result.status === 'pending') log('Pending: waiting for approval or payment');
    else log('Cancelled');
    render();
  } catch (e) {
    logError('Purchase', e);
  } finally {
    buy.disabled = false;
  }
};

restore.onclick = async () => {
  restore.disabled = true;
  try {
    await restorePurchases(entitle);
    log(`Restored: ${ENTITLEMENT} is ${entitle.isEntitled(ENTITLEMENT) ? 'active' : 'not active'}`);
    render();
  } catch (e) {
    logError('Restore', e);
  } finally {
    restore.disabled = false;
  }
};
