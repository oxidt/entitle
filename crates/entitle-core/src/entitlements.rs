//! Entitlement computation: which entitlements a customer has right now.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{
    Environment, PeriodType, Product, PurchaseKind, PurchaseSnapshot, PurchaseStatus, Store,
};

/// Which entitlements each product unlocks.
#[derive(Debug, Clone, Default)]
pub struct EntitlementMapping {
    by_product: HashMap<Product, Vec<String>>,
}

impl EntitlementMapping {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach(&mut self, product: Product, entitlement: impl Into<String>) {
        self.by_product
            .entry(product)
            .or_default()
            .push(entitlement.into());
    }

    pub fn entitlements_for(&self, store: Store, store_product_id: &str) -> &[String] {
        self.by_product
            .get(&Product::new(store, store_product_id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// An entitlement granted by the developer through the secret API, not by a purchase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionalGrant {
    pub entitlement: String,
    pub granted_at: DateTime<Utc>,
    /// `None` = lifetime.
    pub expires_at: Option<DateTime<Utc>>,
}

/// What granted an active entitlement.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementSource {
    Creem,
    AppStore,
    PlayStore,
    Promotional,
}

impl From<Store> for EntitlementSource {
    fn from(s: Store) -> Self {
        match s {
            Store::Creem => EntitlementSource::Creem,
            Store::AppStore => EntitlementSource::AppStore,
            Store::PlayStore => EntitlementSource::PlayStore,
        }
    }
}

/// An entitlement the customer currently has.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveEntitlement {
    pub identifier: String,
    pub source: EntitlementSource,
    /// Store product id of the winning purchase; `None` for promotional grants.
    pub product_id: Option<String>,
    pub purchased_at: DateTime<Utc>,
    /// `None` = lifetime.
    pub expires_at: Option<DateTime<Utc>>,
    pub period_type: Option<PeriodType>,
    pub will_renew: bool,
    /// `true` while the store is in a billing grace period for the winning purchase.
    pub billing_issue: bool,
    pub environment: Option<Environment>,
}

/// Until when a purchase grants access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AccessUntil {
    At(DateTime<Utc>),
    Forever,
}

fn access_until(p: &PurchaseSnapshot, now: DateTime<Utc>) -> Option<AccessUntil> {
    let until = match p.status {
        PurchaseStatus::Refunded
        | PurchaseStatus::Revoked
        | PurchaseStatus::Expired
        | PurchaseStatus::Paused
        | PurchaseStatus::BillingRetry => return None,
        PurchaseStatus::GracePeriod => p.grace_period_expires_at.or(p.expires_at)?,
        PurchaseStatus::Active | PurchaseStatus::Cancelled => match (p.kind, p.expires_at) {
            (_, Some(t)) => t,
            (PurchaseKind::OneTime, None) => return Some(AccessUntil::Forever),
            // A subscription without an expiry is malformed; never grant on it.
            (PurchaseKind::Subscription, None) => return None,
        },
    };
    (until > now).then_some(AccessUntil::At(until))
}

fn grant_until(g: &PromotionalGrant, now: DateTime<Utc>) -> Option<AccessUntil> {
    match g.expires_at {
        None => Some(AccessUntil::Forever),
        Some(t) if t > now => Some(AccessUntil::At(t)),
        Some(_) => None,
    }
}

/// Computes the active entitlements for one customer.
///
/// Rules:
/// - A purchase grants the entitlements its product is attached to.
/// - Active and cancelled-but-not-yet-expired purchases grant until `expires_at`.
/// - Grace period grants until `grace_period_expires_at` (falling back to `expires_at`).
/// - Billing retry, paused, expired, refunded and revoked grant nothing.
/// - One-time purchases without `expires_at` grant forever (until refunded/revoked).
/// - Promotional grants count like purchases.
/// - When several sources grant the same entitlement, the latest expiry wins
///   (lifetime beats any date); ties go to the most recent purchase.
pub fn compute_entitlements(
    purchases: &[PurchaseSnapshot],
    mapping: &EntitlementMapping,
    grants: &[PromotionalGrant],
    now: DateTime<Utc>,
) -> BTreeMap<String, ActiveEntitlement> {
    let mut best: BTreeMap<String, (AccessUntil, ActiveEntitlement)> = BTreeMap::new();

    let mut offer = |until: AccessUntil, ent: ActiveEntitlement| {
        let replace = match best.get(&ent.identifier) {
            None => true,
            Some((cur_until, cur)) => (until, ent.purchased_at) > (*cur_until, cur.purchased_at),
        };
        if replace {
            best.insert(ent.identifier.clone(), (until, ent));
        }
    };

    for p in purchases {
        let Some(until) = access_until(p, now) else {
            continue;
        };
        for identifier in mapping.entitlements_for(p.store, &p.product_id) {
            offer(
                until,
                ActiveEntitlement {
                    identifier: identifier.clone(),
                    source: p.store.into(),
                    product_id: Some(p.product_id.clone()),
                    purchased_at: p.purchased_at,
                    expires_at: match until {
                        AccessUntil::At(t) => Some(t),
                        AccessUntil::Forever => None,
                    },
                    period_type: Some(p.period_type),
                    will_renew: p.kind == PurchaseKind::Subscription
                        && p.auto_renew
                        && matches!(
                            p.status,
                            PurchaseStatus::Active | PurchaseStatus::GracePeriod
                        ),
                    billing_issue: p.status == PurchaseStatus::GracePeriod,
                    environment: Some(p.environment),
                },
            );
        }
    }

    for g in grants {
        let Some(until) = grant_until(g, now) else {
            continue;
        };
        offer(
            until,
            ActiveEntitlement {
                identifier: g.entitlement.clone(),
                source: EntitlementSource::Promotional,
                product_id: None,
                purchased_at: g.granted_at,
                expires_at: g.expires_at,
                period_type: None,
                will_renew: false,
                billing_issue: false,
                environment: None,
            },
        );
    }

    best.into_iter().map(|(k, (_, e))| (k, e)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{at, sub};

    fn mapping() -> EntitlementMapping {
        let mut m = EntitlementMapping::new();
        m.attach(Product::new(Store::AppStore, "pro_monthly"), "pro");
        m.attach(Product::new(Store::PlayStore, "pro_yearly"), "pro");
        m.attach(Product::new(Store::Creem, "prod_lifetime"), "pro");
        m.attach(Product::new(Store::Creem, "prod_lifetime"), "extras");
        m
    }

    #[test]
    fn active_subscription_grants_until_expiry() {
        let p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        let ents = compute_entitlements(&[p], &mapping(), &[], at(20));
        let pro = &ents["pro"];
        assert_eq!(pro.expires_at, Some(at(40)));
        assert_eq!(pro.source, EntitlementSource::AppStore);
        assert!(pro.will_renew);
        assert!(!pro.billing_issue);
    }

    #[test]
    fn past_expiry_grants_nothing_even_if_status_active() {
        let p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        assert!(compute_entitlements(std::slice::from_ref(&p), &mapping(), &[], at(40)).is_empty());
        assert!(compute_entitlements(&[p], &mapping(), &[], at(41)).is_empty());
    }

    #[test]
    fn unmapped_product_grants_nothing() {
        let p = sub(Store::AppStore, "unknown", at(10), Some(at(40)));
        assert!(compute_entitlements(&[p], &mapping(), &[], at(20)).is_empty());
    }

    #[test]
    fn same_product_id_on_other_store_grants_nothing() {
        let p = sub(Store::PlayStore, "pro_monthly", at(10), Some(at(40)));
        assert!(compute_entitlements(&[p], &mapping(), &[], at(20)).is_empty());
    }

    #[test]
    fn cancelled_but_unexpired_still_grants_without_renewal() {
        let mut p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        p.status = PurchaseStatus::Cancelled;
        p.auto_renew = false;
        let ents = compute_entitlements(&[p], &mapping(), &[], at(20));
        assert!(!ents["pro"].will_renew);
    }

    #[test]
    fn grace_period_grants_until_grace_end() {
        let mut p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        p.status = PurchaseStatus::GracePeriod;
        p.grace_period_expires_at = Some(at(56));
        let ents = compute_entitlements(&[p], &mapping(), &[], at(50));
        assert_eq!(ents["pro"].expires_at, Some(at(56)));
        assert!(ents["pro"].billing_issue);
    }

    #[test]
    fn grace_period_without_grace_date_falls_back_to_expiry() {
        let mut p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        p.status = PurchaseStatus::GracePeriod;
        assert_eq!(
            compute_entitlements(&[p], &mapping(), &[], at(20))["pro"].expires_at,
            Some(at(40))
        );
    }

    #[test]
    fn inactive_statuses_grant_nothing() {
        for status in [
            PurchaseStatus::BillingRetry,
            PurchaseStatus::Paused,
            PurchaseStatus::Expired,
            PurchaseStatus::Refunded,
            PurchaseStatus::Revoked,
        ] {
            let mut p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
            p.status = status;
            assert!(
                compute_entitlements(&[p], &mapping(), &[], at(20)).is_empty(),
                "{status:?} must not grant"
            );
        }
    }

    #[test]
    fn one_time_purchase_is_lifetime() {
        let mut p = sub(Store::Creem, "prod_lifetime", at(10), None);
        p.kind = PurchaseKind::OneTime;
        p.auto_renew = false;
        let ents = compute_entitlements(&[p], &mapping(), &[], at(10_000));
        assert_eq!(ents.len(), 2);
        assert_eq!(ents["pro"].expires_at, None);
        assert_eq!(ents["extras"].expires_at, None);
        assert!(!ents["pro"].will_renew);
    }

    #[test]
    fn refunded_one_time_purchase_revokes() {
        let mut p = sub(Store::Creem, "prod_lifetime", at(10), None);
        p.kind = PurchaseKind::OneTime;
        p.status = PurchaseStatus::Refunded;
        assert!(compute_entitlements(&[p], &mapping(), &[], at(20)).is_empty());
    }

    #[test]
    fn subscription_without_expiry_is_ignored() {
        let p = sub(Store::AppStore, "pro_monthly", at(10), None);
        assert!(compute_entitlements(&[p], &mapping(), &[], at(20)).is_empty());
    }

    #[test]
    fn latest_expiry_wins_across_stores() {
        let apple = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        let google = sub(Store::PlayStore, "pro_yearly", at(5), Some(at(400)));
        let ents = compute_entitlements(&[apple.clone(), google.clone()], &mapping(), &[], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::PlayStore);
        assert_eq!(ents["pro"].expires_at, Some(at(400)));
        // Order of input must not matter.
        let ents = compute_entitlements(&[google, apple], &mapping(), &[], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::PlayStore);
    }

    #[test]
    fn lifetime_beats_any_expiry() {
        let apple = sub(Store::AppStore, "pro_monthly", at(10), Some(at(4000)));
        let mut creem = sub(Store::Creem, "prod_lifetime", at(1), None);
        creem.kind = PurchaseKind::OneTime;
        let ents = compute_entitlements(&[apple, creem], &mapping(), &[], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::Creem);
        assert_eq!(ents["pro"].expires_at, None);
    }

    #[test]
    fn refund_of_one_source_falls_back_to_another() {
        let apple = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        let mut google = sub(Store::PlayStore, "pro_yearly", at(5), Some(at(400)));
        google.status = PurchaseStatus::Refunded;
        let ents = compute_entitlements(&[apple, google], &mapping(), &[], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::AppStore);
    }

    #[test]
    fn expiry_tie_prefers_most_recent_purchase() {
        let a = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        let b = sub(Store::PlayStore, "pro_yearly", at(12), Some(at(40)));
        let ents = compute_entitlements(&[a, b], &mapping(), &[], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::PlayStore);
    }

    #[test]
    fn promotional_grants() {
        let grants = [
            PromotionalGrant {
                entitlement: "beta".into(),
                granted_at: at(1),
                expires_at: Some(at(30)),
            },
            PromotionalGrant {
                entitlement: "old".into(),
                granted_at: at(1),
                expires_at: Some(at(5)),
            },
            PromotionalGrant {
                entitlement: "forever".into(),
                granted_at: at(1),
                expires_at: None,
            },
        ];
        let ents = compute_entitlements(&[], &mapping(), &grants, at(20));
        assert_eq!(ents.len(), 2);
        assert_eq!(ents["beta"].source, EntitlementSource::Promotional);
        assert_eq!(ents["forever"].expires_at, None);
    }

    #[test]
    fn promotional_grant_competes_on_expiry() {
        let apple = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        let short = PromotionalGrant {
            entitlement: "pro".into(),
            granted_at: at(15),
            expires_at: Some(at(30)),
        };
        let ents = compute_entitlements(std::slice::from_ref(&apple), &mapping(), &[short], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::AppStore);
        let long = PromotionalGrant {
            entitlement: "pro".into(),
            granted_at: at(15),
            expires_at: Some(at(90)),
        };
        let ents = compute_entitlements(&[apple], &mapping(), &[long], at(20));
        assert_eq!(ents["pro"].source, EntitlementSource::Promotional);
    }

    #[test]
    fn will_renew_false_for_grace_without_auto_renew() {
        let mut p = sub(Store::AppStore, "pro_monthly", at(10), Some(at(40)));
        p.status = PurchaseStatus::GracePeriod;
        p.auto_renew = false;
        assert!(!compute_entitlements(&[p], &mapping(), &[], at(20))["pro"].will_renew);
    }
}
