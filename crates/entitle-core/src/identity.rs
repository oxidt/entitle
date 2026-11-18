//! Identity rules: anonymous ids, logging in (aliasing / merging) and which
//! customer owns a purchase.

use thiserror::Error;
use uuid::Uuid;

use crate::model::CustomerId;

/// Prefix of SDK-generated anonymous app user ids: `$entitle_anon:<uuid>`.
pub const ANONYMOUS_PREFIX: &str = "$entitle_anon:";
pub const MAX_APP_USER_ID_LEN: usize = 256;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("app user id must not be empty")]
    Empty,
    #[error("app user id must be at most {MAX_APP_USER_ID_LEN} bytes")]
    TooLong,
    #[error("anonymous app user ids must look like `{ANONYMOUS_PREFIX}<uuid>`")]
    BadAnonymousId,
    #[error("cannot log in to an anonymous app user id")]
    LoginToAnonymous,
}

pub fn is_anonymous(app_user_id: &str) -> bool {
    app_user_id.starts_with(ANONYMOUS_PREFIX)
}

/// A fresh anonymous app user id.
pub fn new_anonymous_id() -> String {
    format!("{ANONYMOUS_PREFIX}{}", Uuid::new_v4())
}

pub fn validate_app_user_id(app_user_id: &str) -> Result<(), IdentityError> {
    if app_user_id.trim().is_empty() {
        return Err(IdentityError::Empty);
    }
    if app_user_id.len() > MAX_APP_USER_ID_LEN {
        return Err(IdentityError::TooLong);
    }
    if let Some(rest) = app_user_id.strip_prefix(ANONYMOUS_PREFIX)
        && Uuid::parse_str(rest).is_err()
    {
        return Err(IdentityError::BadAnonymousId);
    }
    Ok(())
}

/// A customer as seen by the login logic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownCustomer {
    pub id: CustomerId,
    pub aliases: Vec<String>,
}

impl KnownCustomer {
    /// Anonymous = never been logged in (all aliases are anonymous ids).
    pub fn is_anonymous(&self) -> bool {
        self.aliases.iter().all(|a| is_anonymous(a))
    }
}

/// What `log_in(current_app_user_id, new_app_user_id)` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginPlan {
    /// `new` already points at the current customer. Nothing to do.
    AlreadyLinked(CustomerId),
    /// `new` is unknown and the current customer is anonymous:
    /// add `new` as an alias of the current customer.
    AttachAlias(CustomerId),
    /// `new` belongs to another customer and the current one is anonymous:
    /// move aliases, account tokens, purchases and grants from `from` into `into`,
    /// then delete `from`.
    Merge { from: CustomerId, into: CustomerId },
    /// The current customer is identified (or unknown): never merge two identified
    /// customers. Switch to `target`, creating it when `None`.
    Switch { target: Option<CustomerId> },
}

/// RevenueCat-like login rule: only anonymous customers are ever merged, and
/// always *into* the identified one.
pub fn plan_login(current: Option<&KnownCustomer>, target: Option<CustomerId>) -> LoginPlan {
    match (current, target) {
        (Some(c), Some(t)) if c.id == t => LoginPlan::AlreadyLinked(t),
        (Some(c), None) if c.is_anonymous() => LoginPlan::AttachAlias(c.id),
        (Some(c), Some(t)) if c.is_anonymous() => LoginPlan::Merge {
            from: c.id,
            into: t,
        },
        (_, target) => LoginPlan::Switch { target },
    }
}

/// Who claims a purchase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// A store notification; `token_owner` is the customer the store's account
    /// token (appAccountToken / obfuscatedExternalAccountId / Creem metadata) resolves to.
    Notification { token_owner: Option<CustomerId> },
    /// A customer explicitly synced / restored this purchase from their device.
    /// `token_owner` is the customer the store's account token resolves to, if any.
    Restore {
        customer: CustomerId,
        token_owner: Option<CustomerId>,
    },
}

/// Outcome of [`resolve_owner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Leave the owner as it is (possibly still unlinked).
    Keep,
    /// Link a so-far unlinked purchase.
    Assign(CustomerId),
    /// Move the purchase to another customer (emits a `transfer` event).
    Transfer { from: CustomerId, to: CustomerId },
    /// Refuse the restore: the purchase is owned by another customer and the store's
    /// account token says it belongs to a customer other than the restoring one.
    Refuse,
}

/// Ownership rule (RevenueCat "transfer to new app user id" default):
/// - Notifications only link purchases that have no owner yet; they never move
///   an owned purchase (the token may be stale after a restore/transfer).
/// - An explicit restore links unowned purchases and transfers owned ones to the
///   restoring customer, unless the store's account token resolves to another
///   existing customer: then the purchase provably is not theirs and is refused,
///   whether or not it is linked yet.
///   The server passes no token owner for purely anonymous customers, whose id
///   is lost on reinstall.
pub fn resolve_owner(current: Option<CustomerId>, claim: Claim) -> Ownership {
    match (current, claim) {
        (
            None,
            Claim::Notification {
                token_owner: Some(c),
            },
        ) => Ownership::Assign(c),
        (_, Claim::Notification { .. }) => Ownership::Keep,
        (Some(cur), Claim::Restore { customer, .. }) if cur == customer => Ownership::Keep,
        (
            _,
            Claim::Restore {
                customer,
                token_owner: Some(t),
            },
        ) if t != customer => Ownership::Refuse,
        (None, Claim::Restore { customer, .. }) => Ownership::Assign(customer),
        (Some(from), Claim::Restore { customer, .. }) => Ownership::Transfer { from, to: customer },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(n: u128) -> CustomerId {
        CustomerId(Uuid::from_u128(n))
    }

    fn anon(n: u128) -> KnownCustomer {
        KnownCustomer {
            id: cid(n),
            aliases: vec![format!("{ANONYMOUS_PREFIX}{}", Uuid::from_u128(n))],
        }
    }

    fn identified(n: u128, name: &str) -> KnownCustomer {
        KnownCustomer {
            id: cid(n),
            aliases: vec![
                format!("{ANONYMOUS_PREFIX}{}", Uuid::from_u128(n)),
                name.into(),
            ],
        }
    }

    #[test]
    fn validates_ids() {
        assert_eq!(validate_app_user_id(""), Err(IdentityError::Empty));
        assert_eq!(validate_app_user_id("  "), Err(IdentityError::Empty));
        assert_eq!(
            validate_app_user_id(&"x".repeat(257)),
            Err(IdentityError::TooLong)
        );
        assert_eq!(
            validate_app_user_id("$entitle_anon:nope"),
            Err(IdentityError::BadAnonymousId)
        );
        assert!(validate_app_user_id(&new_anonymous_id()).is_ok());
        assert!(validate_app_user_id("user_42").is_ok());
        assert!(is_anonymous(&new_anonymous_id()));
        assert!(!is_anonymous("user_42"));
    }

    #[test]
    fn anonymous_with_unknown_target_attaches_alias() {
        assert_eq!(
            plan_login(Some(&anon(1)), None),
            LoginPlan::AttachAlias(cid(1))
        );
    }

    #[test]
    fn anonymous_with_existing_target_merges_into_target() {
        assert_eq!(
            plan_login(Some(&anon(1)), Some(cid(2))),
            LoginPlan::Merge {
                from: cid(1),
                into: cid(2)
            }
        );
    }

    #[test]
    fn identified_never_merges() {
        let c = identified(1, "alice");
        assert!(!c.is_anonymous());
        assert_eq!(
            plan_login(Some(&c), Some(cid(2))),
            LoginPlan::Switch {
                target: Some(cid(2))
            }
        );
        assert_eq!(
            plan_login(Some(&c), None),
            LoginPlan::Switch { target: None }
        );
    }

    #[test]
    fn same_customer_is_noop() {
        assert_eq!(
            plan_login(Some(&anon(1)), Some(cid(1))),
            LoginPlan::AlreadyLinked(cid(1))
        );
        let c = identified(3, "bob");
        assert_eq!(
            plan_login(Some(&c), Some(cid(3))),
            LoginPlan::AlreadyLinked(cid(3))
        );
    }

    #[test]
    fn unknown_current_switches() {
        assert_eq!(plan_login(None, None), LoginPlan::Switch { target: None });
        assert_eq!(
            plan_login(None, Some(cid(2))),
            LoginPlan::Switch {
                target: Some(cid(2))
            }
        );
    }

    #[test]
    fn notification_links_only_unowned_purchases() {
        let n = |o| Claim::Notification { token_owner: o };
        assert_eq!(
            resolve_owner(None, n(Some(cid(1)))),
            Ownership::Assign(cid(1))
        );
        assert_eq!(resolve_owner(None, n(None)), Ownership::Keep);
        assert_eq!(
            resolve_owner(Some(cid(2)), n(Some(cid(1)))),
            Ownership::Keep
        );
    }

    #[test]
    fn restore_assigns_or_transfers() {
        let r = |c| Claim::Restore {
            customer: c,
            token_owner: None,
        };
        assert_eq!(resolve_owner(None, r(cid(1))), Ownership::Assign(cid(1)));
        assert_eq!(resolve_owner(Some(cid(1)), r(cid(1))), Ownership::Keep);
        assert_eq!(
            resolve_owner(Some(cid(1)), r(cid(2))),
            Ownership::Transfer {
                from: cid(1),
                to: cid(2)
            }
        );
    }

    #[test]
    fn restore_is_refused_when_the_token_points_to_another_customer() {
        let r = |c, t| Claim::Restore {
            customer: c,
            token_owner: Some(t),
        };
        // Owned by 1, token says 1, 2 restores: refused.
        assert_eq!(
            resolve_owner(Some(cid(1)), r(cid(2), cid(1))),
            Ownership::Refuse
        );
        // Token points to a third customer: still not 2's.
        assert_eq!(
            resolve_owner(Some(cid(1)), r(cid(2), cid(3))),
            Ownership::Refuse
        );
        // Token points to the restoring customer: transfer.
        assert_eq!(
            resolve_owner(Some(cid(1)), r(cid(2), cid(2))),
            Ownership::Transfer {
                from: cid(1),
                to: cid(2)
            }
        );
        // Unowned but the token says 1: refused, not assigned to 2.
        assert_eq!(resolve_owner(None, r(cid(2), cid(1))), Ownership::Refuse);
        assert_eq!(
            resolve_owner(None, r(cid(2), cid(2))),
            Ownership::Assign(cid(2))
        );
        // Already owned by the restoring customer: unchanged.
        assert_eq!(
            resolve_owner(Some(cid(2)), r(cid(2), cid(1))),
            Ownership::Keep
        );
    }
}
