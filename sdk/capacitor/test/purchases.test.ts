import { beforeEach, describe, expect, mock, test } from 'bun:test';

import { type CustomerInfo, Entitle, EntitleError, type FetchLike, memoryStorage } from '@oxidt/entitle';

import type { NativePurchaseResult, PurchaseOptions, StorePurchase } from '../src/definitions.js';

// --- native plugin mock -------------------------------------------------------------------

const log: string[] = [];
let nextPurchase: () => Promise<NativePurchaseResult>;
let currentPurchases: StorePurchase[] = [];
let purchaseListener: ((p: StorePurchase) => void) | undefined;
const removed = mock(() => Promise.resolve());

const native = {
  purchase: mock(async (options: PurchaseOptions) => {
    log.push(`native.purchase ${options.productId} ${options.accountToken}`);
    return nextPurchase();
  }),
  finishTransaction: mock(async ({ transactionId }: { transactionId: string }) => {
    log.push(`finish ${transactionId}`);
  }),
  getCurrentPurchases: mock(async () => ({ purchases: currentPurchases })),
  addListener: mock(async (_event: 'purchaseUpdated', listener: (p: StorePurchase) => void) => {
    purchaseListener = listener;
    return { remove: removed };
  }),
};

mock.module('../src/plugin.js', () => ({ EntitleStore: native }));
const { purchase, restorePurchases, startPurchaseSync } = await import('../src/index.js');

// --- entitle server mock ------------------------------------------------------------------

const CUSTOMER_ID = '0b6f4c1e-5a0c-4a53-9d0e-2a1f7f0e9c11';

function info(entitlements: string[] = []): CustomerInfo {
  return {
    customer_id: CUSTOMER_ID,
    app_user_id: 'user_42',
    aliases: ['user_42'],
    entitlements: Object.fromEntries(
      entitlements.map((id) => [
        id,
        {
          identifier: id,
          source: 'app_store',
          product_id: 'pro.monthly',
          purchased_at: '2026-09-01T10:00:00Z',
          expires_at: null,
          period_type: 'normal',
          will_renew: true,
          billing_issue: false,
          environment: 'sandbox',
        },
      ]),
    ),
    purchases: [],
    management_url: null,
    first_seen_at: '2026-09-01T10:00:00Z',
    request_date: new Date().toISOString(),
  };
}

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

type Handler = (method: string, path: string, body: unknown) => Response | Promise<Response>;
let server: Handler;

/** Answers GET customer with `info()` and delegates `/sync` to `sync`. */
function serve(sync: (body: { store: string; id: string }) => Response | Promise<Response>): void {
  server = (method, path, body) => {
    if (method === 'GET') return json(info());
    if (path.endsWith('/sync')) return sync(body as { store: string; id: string });
    throw new Error(`unexpected ${method} ${path}`);
  };
}

async function entitle(): Promise<Entitle> {
  const fetch: FetchLike = async (url, init) => {
    const path = new URL(url).pathname;
    const body = typeof init.body === 'string' ? JSON.parse(init.body) : undefined;
    log.push(`${init.method} ${path.replace('/v1/customers/user_42', '')}${body ? ` ${body.store}:${body.id}` : ''}`);
    return server(init.method ?? 'GET', path, body);
  };
  return Entitle.configure({
    apiKey: 'pk_test',
    baseUrl: 'https://entitle.test',
    appUserId: 'user_42',
    storage: memoryStorage(),
    fetch,
  });
}

const appStore = (transactionId: string): StorePurchase => ({
  store: 'app_store',
  transactionId,
  productId: 'pro.monthly',
});
const playStore = (purchaseToken: string): StorePurchase => ({
  store: 'play_store',
  purchaseToken,
  productId: 'pro.monthly',
});

beforeEach(() => {
  log.length = 0;
  currentPurchases = [];
  purchaseListener = undefined;
  for (const m of [native.purchase, native.finishTransaction, native.getCurrentPurchases, native.addListener, removed]) {
    m.mockClear();
  }
  serve(() => json(info(['pro'])));
});

// --- tests --------------------------------------------------------------------------------

describe('purchase', () => {
  test('App Store: account token → native purchase → sync → finish, in that order', async () => {
    nextPurchase = async () => ({ status: 'purchased', purchase: appStore('2000000001') });
    const e = await entitle();

    const result = await purchase(e, { productId: 'pro.monthly' });

    expect(result.status).toBe('purchased');
    if (result.status === 'purchased') expect(result.customerInfo.entitlements.pro).toBeDefined();
    expect(log).toEqual([
      'GET ',
      `native.purchase pro.monthly ${CUSTOMER_ID}`,
      'POST /sync app_store:2000000001',
      'finish 2000000001',
    ]);
    expect(e.isEntitled('pro')).toBe(true);
  });

  test('Play Store: syncs the purchase token and never finishes', async () => {
    nextPurchase = async () => ({ status: 'purchased', purchase: playStore('tok_1') });
    const e = await entitle();

    await purchase(e, { productId: 'pro.monthly', offerToken: 'offer_1' });

    expect(native.purchase.mock.calls[0]?.[0]).toEqual({
      productId: 'pro.monthly',
      offerToken: 'offer_1',
      accountToken: CUSTOMER_ID,
    });
    expect(log.slice(-1)).toEqual(['POST /sync play_store:tok_1']);
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });

  test.each(['cancelled', 'pending'] as const)('%s resolves as a typed result without syncing', async (status) => {
    nextPurchase = async () => ({ status });
    const e = await entitle();

    expect(await purchase(e, { productId: 'pro.monthly' })).toEqual({ status });
    expect(log.some((l) => l.startsWith('POST'))).toBe(false);
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });

  test('a failed sync leaves the transaction unfinished', async () => {
    nextPurchase = async () => ({ status: 'purchased', purchase: appStore('2000000002') });
    serve(() => json({ error: { code: 'store_unavailable', message: 'apple is down' } }, 502));
    const e = await entitle();

    const err = await purchase(e, { productId: 'pro.monthly' }).catch((x: unknown) => x);

    expect(err).toBeInstanceOf(EntitleError);
    expect((err as EntitleError).code).toBe('store_unavailable');
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });

  test('transfer_refused surfaces as EntitleError', async () => {
    nextPurchase = async () => ({ status: 'purchased', purchase: appStore('2000000003') });
    serve(() => json({ error: { code: 'transfer_refused', message: 'owned by another customer' } }, 409));
    const e = await entitle();

    const err = await purchase(e, { productId: 'pro.monthly' }).catch((x: unknown) => x);

    expect(err).toBeInstanceOf(EntitleError);
    expect(err).toMatchObject({ status: 409, code: 'transfer_refused' });
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });

  test('native errors propagate', async () => {
    nextPurchase = async () => {
      throw Object.assign(new Error('owned'), { code: 'ITEM_ALREADY_OWNED' });
    };
    const e = await entitle();

    await expect(purchase(e, { productId: 'pro.monthly' })).rejects.toMatchObject({ code: 'ITEM_ALREADY_OWNED' });
  });
});

describe('restorePurchases', () => {
  test('syncs every current purchase and returns the final info', async () => {
    currentPurchases = [appStore('1'), appStore('2')];
    let n = 0;
    serve(() => json(info(++n === 2 ? ['pro', 'lifetime'] : ['pro'])));
    const e = await entitle();

    const result = await restorePurchases(e);

    expect(Object.keys(result.entitlements).sort()).toEqual(['lifetime', 'pro']);
    expect(log).toEqual(['POST /sync app_store:1', 'finish 1', 'POST /sync app_store:2', 'finish 2']);
  });

  test('nothing to restore: fresh customer info', async () => {
    const e = await entitle();

    const result = await restorePurchases(e);

    expect(result.customer_id).toBe(CUSTOMER_ID);
    expect(log).toEqual(['GET ']);
  });

  test('attempts all purchases, then rejects with the first error', async () => {
    currentPurchases = [appStore('1'), appStore('2')];
    serve(({ id }) =>
      id === '1' ? json({ error: { code: 'transfer_refused', message: 'no' } }, 409) : json(info(['pro'])),
    );
    const e = await entitle();

    await expect(restorePurchases(e)).rejects.toMatchObject({ code: 'transfer_refused' });
    expect(log).toEqual(['POST /sync app_store:1', 'POST /sync app_store:2', 'finish 2']);
  });
});

describe('startPurchaseSync', () => {
  test('syncs and finishes each update; unsubscribe removes the listener', async () => {
    const e = await entitle();
    const stop = startPurchaseSync(e);
    await Promise.resolve();

    purchaseListener!(appStore('7'));
    await until(() => log.includes('finish 7'));
    expect(log).toEqual(['POST /sync app_store:7', 'finish 7']);

    stop();
    await Promise.resolve();
    expect(removed).toHaveBeenCalledTimes(1);
  });

  test('does not double-sync a purchase that purchase() is already syncing', async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    serve(async () => {
      await gate;
      return json(info(['pro']));
    });
    nextPurchase = async () => ({ status: 'purchased', purchase: appStore('9') });
    const e = await entitle();
    await e.getCustomerInfo();
    startPurchaseSync(e);
    await Promise.resolve();

    const bought = purchase(e, { productId: 'pro.monthly' });
    await until(() => log.includes('POST /sync app_store:9'));
    purchaseListener!(appStore('9')); // same transaction while purchase() syncs it
    release();
    await bought;
    await Promise.resolve();

    expect(log.filter((l) => l.startsWith('POST'))).toEqual(['POST /sync app_store:9']);
    expect(native.finishTransaction).toHaveBeenCalledTimes(1);
  });

  test('sync errors go to onError and leave the transaction unfinished', async () => {
    serve(() => json({ error: { code: 'transfer_refused', message: 'no' } }, 409));
    const errors: unknown[] = [];
    const e = await entitle();
    startPurchaseSync(e, { onError: (err) => errors.push(err) });
    await Promise.resolve();

    purchaseListener!(appStore('8'));
    await until(() => errors.length > 0);

    expect(errors[0]).toBeInstanceOf(EntitleError);
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });

  test('Play updates sync the purchase token without finishing', async () => {
    const e = await entitle();
    startPurchaseSync(e);
    await Promise.resolve();

    purchaseListener!(playStore('tok_2'));
    await until(() => log.length > 0);
    await Promise.resolve();

    expect(log).toEqual(['POST /sync play_store:tok_2']);
    expect(native.finishTransaction).not.toHaveBeenCalled();
  });
});

async function until(cond: () => boolean): Promise<void> {
  for (let i = 0; i < 100 && !cond(); i++) await new Promise((r) => setTimeout(r, 1));
  if (!cond()) throw new Error('condition not met');
}
