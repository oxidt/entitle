//! Subscription billing periods and normalization of prices to a monthly amount (MRR).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Money;

/// How often a subscription renews. Serialized (and stored) as its ISO 8601 duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BillingPeriod {
    #[serde(rename = "P1W")]
    Week,
    #[serde(rename = "P1M")]
    Month,
    #[serde(rename = "P2M")]
    TwoMonths,
    #[serde(rename = "P3M")]
    ThreeMonths,
    #[serde(rename = "P6M")]
    SixMonths,
    #[serde(rename = "P1Y")]
    Year,
}

/// Average month (365.25 / 12 days), used as the nominal length of month-based periods.
const MONTH_SECS: i64 = 2_629_800;

impl BillingPeriod {
    pub const ALL: [BillingPeriod; 6] = [
        BillingPeriod::Week,
        BillingPeriod::Month,
        BillingPeriod::TwoMonths,
        BillingPeriod::ThreeMonths,
        BillingPeriod::SixMonths,
        BillingPeriod::Year,
    ];

    /// The ISO 8601 duration.
    pub fn as_str(self) -> &'static str {
        match self {
            BillingPeriod::Week => "P1W",
            BillingPeriod::Month => "P1M",
            BillingPeriod::TwoMonths => "P2M",
            BillingPeriod::ThreeMonths => "P3M",
            BillingPeriod::SixMonths => "P6M",
            BillingPeriod::Year => "P1Y",
        }
    }

    pub fn parse(s: &str) -> Option<BillingPeriod> {
        BillingPeriod::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// Length in months as an exact fraction `(numerator, denominator)`.
    /// A week is 12/52 = 3/13 of a month.
    pub fn months(self) -> (i64, i64) {
        match self {
            BillingPeriod::Week => (3, 13),
            BillingPeriod::Month => (1, 1),
            BillingPeriod::TwoMonths => (2, 1),
            BillingPeriod::ThreeMonths => (3, 1),
            BillingPeriod::SixMonths => (6, 1),
            BillingPeriod::Year => (12, 1),
        }
    }

    fn nominal_secs(self) -> i64 {
        match self {
            BillingPeriod::Week => 7 * 86_400,
            _ => {
                let (num, den) = self.months();
                MONTH_SECS * num / den
            }
        }
    }

    /// Infers the period from one billing cycle (`expires_at - purchased_at`): the
    /// supported period whose nominal length is within 10% of it (a 28-day February
    /// is still `P1M`, 89–92 days are `P3M`), else `None`. The tolerances do not
    /// overlap, so at most one period matches.
    pub fn infer(purchased_at: DateTime<Utc>, expires_at: DateTime<Utc>) -> Option<BillingPeriod> {
        let secs = (expires_at - purchased_at).num_seconds();
        BillingPeriod::ALL.into_iter().find(|p| {
            let nominal = p.nominal_secs();
            (secs - nominal).saturating_abs().saturating_mul(10) <= nominal
        })
    }
}

/// The billing period of a purchase: the product's configured period if any, else
/// inferred from the purchase's current cycle ([`BillingPeriod::infer`]).
pub fn billing_period(
    configured: Option<BillingPeriod>,
    purchased_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
) -> Option<BillingPeriod> {
    configured.or_else(|| BillingPeriod::infer(purchased_at, expires_at?))
}

/// The monthly equivalent of `price` charged every `period` (in the same currency):
/// `amount × 1 / months(period)`, computed exactly and rounded once to the nearest
/// minor unit, halves away from zero. `None` if the result does not fit an `i64`.
pub fn monthly_amount(price: &Money, period: BillingPeriod) -> Option<Money> {
    let (num, den) = period.months();
    let amount = div_round(
        i128::from(price.amount_minor) * i128::from(den),
        i128::from(num),
    );
    Some(Money {
        amount_minor: i64::try_from(amount).ok()?,
        currency: price.currency.clone(),
    })
}

/// `n / d` rounded to the nearest integer, halves away from zero. `d > 0`.
pub(crate) fn div_round(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    if 2 * r.abs() >= d { q + n.signum() } else { q }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    fn money(amount_minor: i64, currency: &str) -> Money {
        Money {
            amount_minor,
            currency: currency.into(),
        }
    }

    fn monthly(amount: i64, currency: &str, period: &str) -> i64 {
        monthly_amount(
            &money(amount, currency),
            BillingPeriod::parse(period).unwrap(),
        )
        .unwrap()
        .amount_minor
    }

    #[test]
    fn serde_names_are_iso_durations() {
        for p in BillingPeriod::ALL {
            assert_eq!(serde_json::to_value(p).unwrap(), p.as_str());
        }
        assert_eq!(BillingPeriod::parse("P1Y"), Some(BillingPeriod::Year));
        assert_eq!(BillingPeriod::parse("P12M"), None);
        assert_eq!(BillingPeriod::parse("p1y"), None);
    }

    #[test]
    fn monthly_normalization() {
        assert_eq!(monthly(999, "USD", "P1M"), 999);
        // Yearly ÷ 12: 59.99 → 4.999… → 5.00
        assert_eq!(monthly(5999, "USD", "P1Y"), 500);
        assert_eq!(monthly(4999, "EUR", "P1Y"), 417); // 416.58
        // Weekly × 52/12: 2.99 → 12.956… → 12.96
        assert_eq!(monthly(299, "USD", "P1W"), 1296);
        assert_eq!(monthly(1500, "USD", "P3M"), 500);
        assert_eq!(monthly(1000, "USD", "P6M"), 167); // 166.67
        assert_eq!(monthly(1001, "USD", "P2M"), 501); // 500.5 rounds away from zero
        // JPY has no minor unit: whole yen.
        assert_eq!(monthly(9800, "JPY", "P1Y"), 817); // 816.67
        assert_eq!(monthly(480, "JPY", "P1W"), 2080);
        // Refunds / negative amounts round symmetrically.
        assert_eq!(monthly(-1001, "USD", "P2M"), -501);
        assert_eq!(
            monthly_amount(&money(i64::MAX, "USD"), BillingPeriod::Week),
            None
        );
        assert_eq!(
            monthly_amount(&money(i64::MAX, "USD"), BillingPeriod::Month),
            Some(money(i64::MAX, "USD"))
        );
    }

    #[test]
    fn inference_snaps_to_supported_periods() {
        let start = Utc.with_ymd_and_hms(2026, 2, 1, 12, 0, 0).unwrap();
        let infer = |days: i64| BillingPeriod::infer(start, start + Duration::days(days));
        assert_eq!(infer(7), Some(BillingPeriod::Week));
        assert_eq!(infer(28), Some(BillingPeriod::Month)); // February
        assert_eq!(infer(31), Some(BillingPeriod::Month));
        assert_eq!(infer(59), Some(BillingPeriod::TwoMonths));
        assert_eq!(infer(89), Some(BillingPeriod::ThreeMonths));
        assert_eq!(infer(92), Some(BillingPeriod::ThreeMonths));
        assert_eq!(infer(181), Some(BillingPeriod::SixMonths));
        assert_eq!(infer(365), Some(BillingPeriod::Year));
        assert_eq!(infer(366), Some(BillingPeriod::Year));
        // Unsnappable: trials of odd length, gaps between periods, nonsense.
        for days in [0, 3, 14, 45, 120, 250, 730, -30] {
            assert_eq!(infer(days), None, "{days} days");
        }
    }

    #[test]
    fn configured_period_wins() {
        let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let month_later = Some(start + Duration::days(31));
        assert_eq!(
            billing_period(Some(BillingPeriod::Year), start, month_later),
            Some(BillingPeriod::Year)
        );
        assert_eq!(
            billing_period(None, start, month_later),
            Some(BillingPeriod::Month)
        );
        assert_eq!(billing_period(None, start, None), None);
        assert_eq!(
            billing_period(None, start, Some(start + Duration::days(45))),
            None
        );
    }
}
