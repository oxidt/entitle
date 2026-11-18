//! Lifecycle events sent to developer backends, derived by diffing the stored
//! purchase state against the new snapshot. Store-agnostic on purpose: adapters
//! only report state, never events.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::{PurchaseKind, PurchaseSnapshot, PurchaseStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    InitialPurchase,
    Renewal,
    Cancellation,
    Uncancellation,
    Expiration,
    BillingIssue,
    /// Refunded or revoked; check `purchase.status` to tell them apart.
    Refund,
    ProductChange,
    /// A purchase moved to another customer after a restore.
    Transfer,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            EventType::InitialPurchase => "initial_purchase",
            EventType::Renewal => "renewal",
            EventType::Cancellation => "cancellation",
            EventType::Uncancellation => "uncancellation",
            EventType::Expiration => "expiration",
            EventType::BillingIssue => "billing_issue",
            EventType::Refund => "refund",
            EventType::ProductChange => "product_change",
            EventType::Transfer => "transfer",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn has_access(s: PurchaseStatus) -> bool {
    matches!(
        s,
        PurchaseStatus::Active | PurchaseStatus::Cancelled | PurchaseStatus::GracePeriod
    )
}

fn billing_problem(s: PurchaseStatus) -> bool {
    matches!(
        s,
        PurchaseStatus::GracePeriod | PurchaseStatus::BillingRetry
    )
}

fn will_renew(p: &PurchaseSnapshot) -> bool {
    p.kind == PurchaseKind::Subscription && p.auto_renew && p.status != PurchaseStatus::Cancelled
}

/// Events caused by moving from `old` (stored state, if any) to `new`.
/// Call only for snapshots that are actually applied (not stale ones).
pub fn diff_events(old: Option<&PurchaseSnapshot>, new: &PurchaseSnapshot) -> Vec<EventType> {
    let Some(old) = old else {
        // First time we see this purchase.
        let mut v = Vec::new();
        if has_access(new.status) || new.status == PurchaseStatus::BillingRetry {
            v.push(EventType::InitialPurchase);
        }
        if billing_problem(new.status) {
            v.push(EventType::BillingIssue);
        }
        return v;
    };

    if new.status.is_revoked() {
        return if old.status.is_revoked() {
            Vec::new()
        } else {
            vec![EventType::Refund]
        };
    }

    let mut v = Vec::new();
    let product_changed = old.product_id != new.product_id;
    if product_changed {
        v.push(EventType::ProductChange);
    }
    let extended = match (old.expires_at, new.expires_at) {
        (Some(o), Some(n)) => n > o,
        _ => false,
    };
    if extended && !product_changed && has_access(new.status) {
        v.push(EventType::Renewal);
    }
    match (will_renew(old), will_renew(new)) {
        (true, false) if new.status != PurchaseStatus::Expired => v.push(EventType::Cancellation),
        (false, true) if has_access(new.status) => v.push(EventType::Uncancellation),
        _ => {}
    }
    if billing_problem(new.status) && !billing_problem(old.status) {
        v.push(EventType::BillingIssue);
    }
    if new.status == PurchaseStatus::Expired && old.status != PurchaseStatus::Expired {
        v.push(EventType::Expiration);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Store;
    use crate::testing::{at, sub};
    use EventType::*;

    fn base() -> PurchaseSnapshot {
        sub(Store::AppStore, "pro_monthly", at(0), Some(at(30)))
    }

    #[test]
    fn first_sighting() {
        assert_eq!(diff_events(None, &base()), vec![InitialPurchase]);
        let mut p = base();
        p.status = PurchaseStatus::GracePeriod;
        assert_eq!(diff_events(None, &p), vec![InitialPurchase, BillingIssue]);
        p.status = PurchaseStatus::Expired;
        assert!(diff_events(None, &p).is_empty());
        p.status = PurchaseStatus::Refunded;
        assert!(diff_events(None, &p).is_empty());
    }

    #[test]
    fn renewal() {
        let mut new = base();
        new.expires_at = Some(at(60));
        new.store_transaction_id = "t2".into();
        assert_eq!(diff_events(Some(&base()), &new), vec![Renewal]);
    }

    #[test]
    fn identical_snapshot_is_quiet() {
        assert!(diff_events(Some(&base()), &base()).is_empty());
    }

    #[test]
    fn cancellation_and_uncancellation() {
        let mut cancelled = base();
        cancelled.auto_renew = false;
        cancelled.status = PurchaseStatus::Cancelled;
        assert_eq!(diff_events(Some(&base()), &cancelled), vec![Cancellation]);
        assert_eq!(diff_events(Some(&cancelled), &base()), vec![Uncancellation]);
        // auto_renew flag alone is enough.
        let mut off = base();
        off.auto_renew = false;
        assert_eq!(diff_events(Some(&base()), &off), vec![Cancellation]);
    }

    #[test]
    fn expiration() {
        let mut expired = base();
        expired.status = PurchaseStatus::Expired;
        expired.auto_renew = false;
        assert_eq!(diff_events(Some(&base()), &expired), vec![Expiration]);
        assert!(diff_events(Some(&expired), &expired).is_empty());
    }

    #[test]
    fn billing_issue_and_recovery() {
        let mut grace = base();
        grace.status = PurchaseStatus::GracePeriod;
        assert_eq!(diff_events(Some(&base()), &grace), vec![BillingIssue]);
        let mut retry = grace.clone();
        retry.status = PurchaseStatus::BillingRetry;
        assert!(diff_events(Some(&grace), &retry).is_empty());
        let mut recovered = base();
        recovered.expires_at = Some(at(60));
        assert_eq!(diff_events(Some(&retry), &recovered), vec![Renewal]);
    }

    #[test]
    fn refund() {
        let mut refunded = base();
        refunded.status = PurchaseStatus::Refunded;
        assert_eq!(diff_events(Some(&base()), &refunded), vec![Refund]);
        assert!(diff_events(Some(&refunded), &refunded).is_empty());
        let mut revoked = base();
        revoked.status = PurchaseStatus::Revoked;
        assert_eq!(diff_events(Some(&base()), &revoked), vec![Refund]);
    }

    #[test]
    fn product_change() {
        let mut upgraded = base();
        upgraded.product_id = "pro_yearly".into();
        upgraded.expires_at = Some(at(365));
        assert_eq!(diff_events(Some(&base()), &upgraded), vec![ProductChange]);
    }

    #[test]
    fn serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&BillingIssue).unwrap(),
            "\"billing_issue\""
        );
        assert_eq!(BillingIssue.as_str(), "billing_issue");
    }
}
