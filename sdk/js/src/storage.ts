/**
 * Where the SDK persists the app user id and the cached customer info.
 * Methods may be sync or async, e.g. Capacitor Preferences:
 *
 * ```ts
 * const storage = {
 *   get: async (key) => (await Preferences.get({ key })).value,
 *   set: (key, value) => Preferences.set({ key, value }),
 *   remove: (key) => Preferences.remove({ key }),
 * };
 * ```
 */
export interface EntitleStorage {
  get(key: string): string | null | undefined | Promise<string | null | undefined>;
  set(key: string, value: string): void | Promise<void>;
  remove(key: string): void | Promise<void>;
}

/** Non-persistent storage: the fallback when `localStorage` is unavailable. */
export function memoryStorage(): EntitleStorage {
  const data = new Map<string, string>();
  return {
    get: (key) => data.get(key) ?? null,
    set: (key, value) => void data.set(key, value),
    remove: (key) => void data.delete(key),
  };
}

/** `localStorage` if it is usable (it throws in some sandboxed / privacy modes), else memory. */
export function defaultStorage(): EntitleStorage {
  try {
    const ls = globalThis.localStorage;
    if (ls) {
      const probe = "entitle:probe";
      ls.setItem(probe, probe);
      ls.removeItem(probe);
      return {
        get: (key) => ls.getItem(key),
        set: (key, value) => ls.setItem(key, value),
        remove: (key) => ls.removeItem(key),
      };
    }
  } catch {
    // fall through
  }
  return memoryStorage();
}
