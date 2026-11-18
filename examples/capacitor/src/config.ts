// ─── Configure the example here ─────────────────────────────────────────────
// The app id (bundle id / package name) is `appId` in capacitor.config.json.

/** Your entitle server, reachable from the device (not `localhost` on a phone). */
export const SERVER_URL = 'https://entitle.example.com';
/** A public key of the project: `entitle admin keys create "<project>"`. */
export const API_KEY = 'pk_replace_me';
/** Store product id, registered in App Store Connect / Play Console and in entitle. */
export const PRODUCT_ID = 'quest.example.full';
/** Entitlement the product unlocks in entitle. */
export const ENTITLEMENT = 'full_game';
