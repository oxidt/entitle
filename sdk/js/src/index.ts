export {
  ANONYMOUS_PREFIX,
  CACHE_TTL_MS,
  Entitle,
  type CheckoutOptions,
  type EntitleOptions,
  type FetchLike,
  type LogInResult,
} from "./entitle.js";
export { EntitleError, type EntitleErrorCode, type ServerErrorCode } from "./errors.js";
export { defaultStorage, memoryStorage, type EntitleStorage } from "./storage.js";

// Wire types, generated from the server's Rust types (see README).
export type { ActiveEntitlement } from "./generated/ActiveEntitlement.js";
export type { CheckoutSession } from "./generated/CheckoutSession.js";
export type { CustomerInfo } from "./generated/CustomerInfo.js";
export type { EntitlementSource } from "./generated/EntitlementSource.js";
export type { Environment } from "./generated/Environment.js";
export type { Money } from "./generated/Money.js";
export type { PeriodType } from "./generated/PeriodType.js";
export type { PurchaseKind } from "./generated/PurchaseKind.js";
export type { PurchaseStatus } from "./generated/PurchaseStatus.js";
export type { PurchaseView } from "./generated/PurchaseView.js";
export type { Store } from "./generated/Store.js";
export type { SyncRequest } from "./generated/SyncRequest.js";
