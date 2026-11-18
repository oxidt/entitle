//! Revenue metrics (MRR and its movement, revenue, trials, churn) computed from the
//! append-only purchase state history. Pure: the server loads the inputs.
//!
//! Definitions (also in `docs/api.md`):
//!
//! - **Access** of a subscription at `t`: its latest state with `as_of <= t` is
//!   `active`/`cancelled` and `t < expires_at`, or `grace_period` and
//!   `t < grace_period_expires_at` (else `expires_at`). Everything else (billing
//!   retry, paused, expired, refunded, revoked) has no access.
//! - **Late renewals**: if access lapses at `end` and the purchase's next state,
//!   which grants access again, has `as_of <= end + RENEWAL_TOLERANCE`, access is
//!   continuous (no churn + reactivation). A latest state that lapsed at most
//!   `RENEWAL_TOLERANCE` before `now` and is still `active` + auto-renewing counts
//!   as active (renewal pending). Otherwise access ends at `end`, not when the
//!   expiration was reported.
//! - **MRR** of a purchase with access: its price normalized to one month
//!   ([`monthly_amount`]), for non-trial subscription states (intro prices count,
//!   at the intro price). Trials and one-time purchases have no MRR.
//! - **Billing period** of a state, first match: the product's configured period;
//!   the latest period inferred in this purchase's history for the same product up
//!   to that state; the first one inferred later. A state yields an inference from
//!   `expires_at - previous expires_at` when it extended the previous state's
//!   `expires_at` without a lapse (as_of within the tolerance), else from
//!   `expires_at - purchased_at` — for Play Store only on the first sighting of the
//!   purchase, because Google's `purchased_at` is the subscription's first start.
//!   States that match no supported period contribute no MRR.
//! - **Movement** is per customer: 0 → >0 is new (the customer never had MRR
//!   before) or reactivation, >0 → 0 churn, up/down expansion/contraction. Both
//!   sides of every comparison use the bucket's stock FX rate, so the five fields
//!   sum to the change of customer MRR within the bucket at that rate.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, Datelike, Days, Duration, Months, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::billing::{BillingPeriod, monthly_amount};
use crate::events::EventType;
use crate::fx::FxRates;
use crate::model::{Money, PeriodType, PurchaseKind, PurchaseStatus, Store};

/// How late a renewal may be recorded after the paid period ended and still count
/// as continuous. Stores usually report renewals within minutes to hours.
pub const RENEWAL_TOLERANCE_DAYS: i64 = 3;
pub const MAX_BUCKETS: usize = 1000;

fn tolerance() -> Duration {
    Duration::days(RENEWAL_TOLERANCE_DAYS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interval {
    Day,
    /// Weeks start on Monday.
    Week,
    Month,
}

impl Interval {
    fn floor(self, d: NaiveDate) -> NaiveDate {
        match self {
            Interval::Day => d,
            Interval::Week => d - Days::new(u64::from(d.weekday().num_days_from_monday())),
            Interval::Month => d.with_day(1).expect("day 1 exists"),
        }
    }

    fn next(self, d: NaiveDate) -> Option<NaiveDate> {
        match self {
            Interval::Day => d.checked_add_days(Days::new(1)),
            Interval::Week => d.checked_add_days(Days::new(7)),
            Interval::Month => d.checked_add_months(Months::new(1)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    #[error("`to` must be after `from`")]
    Empty,
    #[error("more than {MAX_BUCKETS} buckets; use a longer interval or a shorter range")]
    TooManyBuckets,
}

/// `[from, to)` split into buckets aligned to interval starts (UTC). The first and
/// last buckets are partial when `from` / `to` are not aligned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    from: NaiveDate,
    to: NaiveDate,
    starts: Vec<NaiveDate>,
}

impl Range {
    pub fn new(from: NaiveDate, to: NaiveDate, interval: Interval) -> Result<Range, RangeError> {
        if to <= from {
            return Err(RangeError::Empty);
        }
        let mut starts = Vec::new();
        let mut start = Some(interval.floor(from));
        while let Some(s) = start.filter(|s| *s < to) {
            if starts.len() == MAX_BUCKETS {
                return Err(RangeError::TooManyBuckets);
            }
            starts.push(s);
            start = interval.next(s);
        }
        Ok(Range { from, to, starts })
    }

    pub fn from(&self) -> NaiveDate {
        self.from
    }

    pub fn to(&self) -> NaiveDate {
        self.to
    }

    /// Aligned bucket start dates.
    pub fn starts(&self) -> &[NaiveDate] {
        &self.starts
    }

    /// `[start, end)` of each bucket, clipped to `[from, to)`.
    fn spans(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let midnight = |d: NaiveDate| d.and_time(chrono::NaiveTime::MIN).and_utc();
        (0..self.starts.len())
            .map(|k| {
                let start = self.starts[k].max(self.from);
                let end = self.starts.get(k + 1).copied().unwrap_or(self.to);
                (midnight(start), midnight(end))
            })
            .collect()
    }
}

/// One `purchase_history` row.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    /// Breaks ties between rows with the same `as_of`.
    pub id: i64,
    pub purchase_id: Uuid,
    pub customer_id: Option<Uuid>,
    pub store: Store,
    pub product_id: String,
    pub kind: PurchaseKind,
    pub status: PurchaseStatus,
    pub period_type: PeriodType,
    pub purchased_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub grace_period_expires_at: Option<DateTime<Utc>>,
    pub auto_renew: bool,
    pub price: Option<Money>,
    pub as_of: DateTime<Utc>,
    pub events: Vec<EventType>,
    pub backfilled: bool,
}

pub struct MetricsInput<'a> {
    /// Every row of the project's production purchases with `as_of` before the end
    /// of the range plus [`RENEWAL_TOLERANCE_DAYS`], in any order.
    pub rows: &'a [HistoryRow],
    /// Configured billing periods by `(store, store_product_id)`.
    pub periods: &'a HashMap<(Store, String), BillingPeriod>,
    pub fx: &'a FxRates,
    /// Creation times of the project's customers (at least those in the range).
    pub customers_created: &'a [DateTime<Utc>],
    /// Reporting currency.
    pub currency: &'a str,
    pub history_start: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub currency: String,
    pub history_start: Option<DateTime<Utc>>,
    pub points: Vec<Point>,
    pub unconverted: Vec<Unconverted>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub start: NaiveDate,
    pub mrr_minor: i64,
    pub active_subscriptions: u64,
    pub active_trials: u64,
    pub new_mrr_minor: i64,
    pub expansion_mrr_minor: i64,
    pub reactivation_mrr_minor: i64,
    pub contraction_mrr_minor: i64,
    pub churned_mrr_minor: i64,
    pub revenue_minor: i64,
    pub refunded_minor: i64,
    pub transactions: u64,
    pub new_customers: u64,
    pub new_trials: u64,
    pub trial_conversions: u64,
    pub churned_subscriptions: u64,
    pub churn_rate: Option<f64>,
}

impl Point {
    fn new(start: NaiveDate) -> Point {
        Point {
            start,
            mrr_minor: 0,
            active_subscriptions: 0,
            active_trials: 0,
            new_mrr_minor: 0,
            expansion_mrr_minor: 0,
            reactivation_mrr_minor: 0,
            contraction_mrr_minor: 0,
            churned_mrr_minor: 0,
            revenue_minor: 0,
            refunded_minor: 0,
            transactions: 0,
            new_customers: 0,
            new_trials: 0,
            trial_conversions: 0,
            churned_subscriptions: 0,
            churn_rate: None,
        }
    }
}

/// Amounts without an FX rate into the reporting currency, in their own currency:
/// revenue over the whole range and MRR at the end of the last bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unconverted {
    pub currency: String,
    pub revenue_minor: i64,
    pub mrr_minor: i64,
}

/// Who MRR movement is attributed to. Purchases never linked to a customer (or
/// unlinked by erasure) are their own pseudo-customer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Owner {
    Customer(Uuid),
    Purchase(Uuid),
}

/// A purchase's state from `from` until the next segment's `from`.
struct Segment {
    from: DateTime<Utc>,
    /// Access until (exclusive); `None` = until the next segment.
    access_end: Option<DateTime<Utc>>,
    owner: Owner,
    subscription: bool,
    trial: bool,
    /// Monthly amount; `None` for trials or unknown price / period.
    mrr: Option<Money>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct State {
    owner: Option<Owner>,
    /// Set only with access.
    mrr: Option<Money>,
    paid: bool,
    trial: bool,
}

fn state_at(segments: &[Segment], t: DateTime<Utc>) -> State {
    let i = segments.partition_point(|s| s.from <= t);
    let Some(s) = i.checked_sub(1).map(|i| &segments[i]) else {
        return State::default();
    };
    let access = s.subscription && s.access_end.is_none_or(|end| t < end);
    State {
        owner: Some(s.owner),
        mrr: s.mrr.clone().filter(|_| access),
        paid: access && !s.trial,
        trial: access && s.trial,
    }
}

fn has_access_status(status: PurchaseStatus) -> bool {
    matches!(
        status,
        PurchaseStatus::Active | PurchaseStatus::Cancelled | PurchaseStatus::GracePeriod
    )
}

/// End of the access this state grants, if it grants any.
fn paid_through(r: &HistoryRow) -> Option<DateTime<Utc>> {
    if r.kind != PurchaseKind::Subscription {
        return None;
    }
    match r.status {
        PurchaseStatus::Active | PurchaseStatus::Cancelled => r.expires_at,
        PurchaseStatus::GracePeriod => r.grace_period_expires_at.or(r.expires_at),
        _ => None,
    }
}

/// Billing period of each row of one purchase (sorted by `as_of`, `id`). See the module docs.
fn periods(
    rows: &[&HistoryRow],
    configured: &HashMap<(Store, String), BillingPeriod>,
) -> Vec<Option<BillingPeriod>> {
    let inferred: Vec<Option<BillingPeriod>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            if r.kind != PurchaseKind::Subscription || r.period_type == PeriodType::Trial {
                return None;
            }
            let expires = r.expires_at?;
            let step = i
                .checked_sub(1)
                .map(|j| rows[j])
                .filter(|prev| prev.product_id == r.product_id)
                .and_then(|prev| prev.expires_at)
                .filter(|&prev| expires > prev && r.as_of <= prev + tolerance())
                .and_then(|prev| BillingPeriod::infer(prev, expires));
            let own_cycle_reliable = r.store != Store::PlayStore
                || (!r.backfilled && r.events.contains(&EventType::InitialPurchase));
            step.or_else(|| {
                own_cycle_reliable
                    .then(|| BillingPeriod::infer(r.purchased_at, expires))
                    .flatten()
            })
        })
        .collect();
    let mut latest: HashMap<&str, BillingPeriod> = HashMap::new();
    let mut result: Vec<Option<BillingPeriod>> = rows
        .iter()
        .zip(&inferred)
        .map(|(r, inf)| {
            if let Some(p) = inf {
                latest.insert(&r.product_id, *p);
            }
            latest.get(r.product_id.as_str()).copied()
        })
        .collect();
    let mut next: HashMap<&str, BillingPeriod> = HashMap::new();
    for i in (0..rows.len()).rev() {
        if let Some(p) = inferred[i] {
            next.insert(&rows[i].product_id, p);
        }
        result[i] = result[i].or_else(|| next.get(rows[i].product_id.as_str()).copied());
    }
    rows.iter()
        .zip(result)
        .map(|(r, p)| {
            configured
                .get(&(r.store, r.product_id.clone()))
                .copied()
                .or(p)
        })
        .collect()
}

/// State segments of one purchase (rows sorted by `as_of`, `id`).
fn segments(
    rows: &[&HistoryRow],
    configured: &HashMap<(Store, String), BillingPeriod>,
    now: DateTime<Utc>,
) -> Vec<Segment> {
    let purchase = rows[0].purchase_id;
    let first_customer = rows.iter().find_map(|r| r.customer_id);
    let periods = periods(rows, configured);
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let access_end = match (paid_through(r), rows.get(i + 1)) {
                (None, _) => Some(r.as_of),
                (Some(end), Some(next)) => {
                    let regained = paid_through(next).is_some_and(|e| next.as_of < e);
                    let bridged = end < next.as_of && regained && next.as_of - end <= tolerance();
                    (!bridged).then_some(end)
                }
                (Some(end), None) => {
                    let pending = r.status == PurchaseStatus::Active
                        && r.auto_renew
                        && end <= now
                        && now - end <= tolerance();
                    (!pending).then_some(end)
                }
            };
            let trial = r.period_type == PeriodType::Trial;
            let mrr = match (&r.price, periods[i]) {
                (Some(price), Some(period)) if r.kind == PurchaseKind::Subscription && !trial => {
                    monthly_amount(price, period)
                }
                _ => None,
            };
            Segment {
                from: r.as_of,
                access_end,
                owner: r
                    .customer_id
                    .or(first_customer)
                    .map_or(Owner::Purchase(purchase), Owner::Customer),
                subscription: r.kind == PurchaseKind::Subscription,
                trial,
                mrr,
            }
        })
        .collect()
}

/// Conversion into the reporting currency on one date.
struct Conv<'a> {
    fx: &'a FxRates,
    currency: &'a str,
    on: NaiveDate,
}

impl Conv<'_> {
    fn convert(&self, m: &Money) -> Option<i64> {
        self.fx
            .convert(m, self.currency, self.on)
            .map(|m| m.amount_minor)
    }
}

/// Replays purchase state changes in time order.
struct Sweep {
    segments: Vec<Vec<Segment>>,
    current: Vec<State>,
    /// Purchases currently contributing MRR, by owner.
    contributing: HashMap<Owner, HashSet<usize>>,
    /// Owners that have had MRR at some point.
    ever: HashSet<Owner>,
    paid: u64,
    trials: u64,
}

impl Sweep {
    fn total(&self, owner: Owner, conv: &Conv) -> i64 {
        self.contributing.get(&owner).map_or(0, |ps| {
            ps.iter()
                .filter_map(|&p| self.current[p].mrr.as_ref())
                .filter_map(|m| conv.convert(m))
                .sum()
        })
    }

    fn has_mrr(&self, owner: Owner) -> bool {
        self.contributing.get(&owner).is_some_and(|ps| {
            ps.iter().any(|&p| {
                self.current[p]
                    .mrr
                    .as_ref()
                    .is_some_and(|m| m.amount_minor > 0)
            })
        })
    }

    /// Moves `purchases` to their state at `t`, recording movement in `point` if given.
    fn apply(
        &mut self,
        t: DateTime<Utc>,
        purchases: &[usize],
        mut point: Option<(&mut Point, &Conv)>,
    ) {
        let next: Vec<State> = purchases
            .iter()
            .map(|&p| state_at(&self.segments[p], t))
            .collect();
        let mut owners: Vec<Owner> = purchases
            .iter()
            .zip(&next)
            .flat_map(|(&p, n)| [self.current[p].owner, n.owner])
            .flatten()
            .collect();
        owners.sort();
        owners.dedup();
        let before: Vec<(i64, bool)> = owners
            .iter()
            .map(|&o| {
                let total = point.as_ref().map_or(0, |(_, conv)| self.total(o, conv));
                (total, self.ever.contains(&o))
            })
            .collect();

        for (&p, new) in purchases.iter().zip(next) {
            let old = std::mem::replace(&mut self.current[p], new.clone());
            if let Some(o) = old.owner.filter(|_| old.mrr.is_some()) {
                self.contributing.entry(o).or_default().remove(&p);
            }
            if let Some(o) = new.owner.filter(|_| new.mrr.is_some()) {
                self.contributing.entry(o).or_default().insert(p);
            }
            self.paid = self.paid + u64::from(new.paid) - u64::from(old.paid);
            self.trials = self.trials + u64::from(new.trial) - u64::from(old.trial);
            if let Some((pt, _)) = point.as_mut()
                && old.paid
                && !new.paid
            {
                pt.churned_subscriptions += 1;
            }
        }

        for (&o, (before, ever)) in owners.iter().zip(before) {
            if let Some((pt, conv)) = point.as_mut() {
                let after = self.total(o, conv);
                let delta = after - before;
                if before <= 0 && after > 0 {
                    if ever {
                        pt.reactivation_mrr_minor += delta;
                    } else {
                        pt.new_mrr_minor += delta;
                    }
                } else if before > 0 && after <= 0 {
                    pt.churned_mrr_minor += delta;
                } else if delta > 0 {
                    pt.expansion_mrr_minor += delta;
                } else {
                    pt.contraction_mrr_minor += delta;
                }
            }
            if self.has_mrr(o) {
                self.ever.insert(o);
            }
        }
    }
}

/// Applies `changes[*next..]` before `end`, grouped by time.
fn replay(
    sweep: &mut Sweep,
    changes: &[(DateTime<Utc>, usize)],
    next: &mut usize,
    end: DateTime<Utc>,
    mut point: Option<(&mut Point, &Conv)>,
) {
    while let Some(&(t, _)) = changes.get(*next).filter(|(t, _)| *t < end) {
        let group: Vec<usize> = changes[*next..]
            .iter()
            .take_while(|(c, _)| *c == t)
            .map(|(_, p)| *p)
            .collect();
        *next += group.len();
        sweep.apply(
            t,
            &group,
            point.as_mut().map(|(pt, conv)| (&mut **pt, *conv)),
        );
    }
}

pub fn compute(input: &MetricsInput, range: &Range) -> Metrics {
    let now = input.now;
    let spans = range.spans();
    let (first, last) = (spans[0].0, spans[spans.len() - 1].1);
    let bucket_of = |t: DateTime<Utc>| -> Option<usize> {
        (first <= t && t < last).then(|| spans.partition_point(|(s, _)| *s <= t) - 1)
    };
    let mut points: Vec<Point> = range.starts().iter().map(|&s| Point::new(s)).collect();

    // Group rows by purchase, oldest first.
    let mut by_purchase: HashMap<Uuid, Vec<&HistoryRow>> = HashMap::new();
    for r in input.rows {
        by_purchase.entry(r.purchase_id).or_default().push(r);
    }
    let mut purchases: Vec<Vec<&HistoryRow>> = by_purchase.into_values().collect();
    for rows in &mut purchases {
        rows.sort_by_key(|r| (r.as_of, r.id));
    }
    purchases.sort_by_key(|rows| rows[0].purchase_id);

    // Flows recorded per history row.
    let mut unconverted_revenue: HashMap<String, i64> = HashMap::new();
    for rows in &purchases {
        for (i, r) in rows.iter().enumerate() {
            let Some(k) = bucket_of(r.as_of).filter(|_| !r.backfilled) else {
                continue;
            };
            let pt = &mut points[k];
            let prev = i.checked_sub(1).map(|j| rows[j]);
            let trial = r.period_type == PeriodType::Trial;
            let has = |e| r.events.contains(&e);
            let on = r.as_of.date_naive();
            if !trial && (has(EventType::InitialPurchase) || has(EventType::Renewal)) {
                pt.transactions += 1;
                if let Some(price) = &r.price {
                    match input.fx.convert(price, input.currency, on) {
                        Some(m) => pt.revenue_minor += m.amount_minor,
                        None => {
                            *unconverted_revenue
                                .entry(price.currency.to_ascii_uppercase())
                                .or_default() += price.amount_minor;
                        }
                    }
                }
            }
            if !trial && has(EventType::Refund) {
                // Refunds in currencies without a rate are not reported.
                if let Some(m) = r
                    .price
                    .as_ref()
                    .and_then(|p| input.fx.convert(p, input.currency, on))
                {
                    pt.refunded_minor += m.amount_minor;
                }
            }
            let prev_trial = prev.is_some_and(|p| p.period_type == PeriodType::Trial);
            if trial && !prev_trial && (has(EventType::InitialPurchase) || has(EventType::Renewal))
            {
                pt.new_trials += 1;
            }
            if !trial
                && prev_trial
                && r.kind == PurchaseKind::Subscription
                && has_access_status(r.status)
            {
                pt.trial_conversions += 1;
            }
        }
    }
    for &created in input.customers_created {
        if let Some(k) = bucket_of(created) {
            points[k].new_customers += 1;
        }
    }

    // MRR stock and movement: replay every change of access in time order.
    let segments: Vec<Vec<Segment>> = purchases
        .iter()
        .map(|rows| segments(rows, input.periods, now))
        .collect();
    let mut changes: Vec<(DateTime<Utc>, usize)> = Vec::new();
    for (p, segs) in segments.iter().enumerate() {
        for (i, s) in segs.iter().enumerate() {
            changes.push((s.from, p));
            let until = segs.get(i + 1).map(|n| n.from);
            if let Some(end) = s.access_end
                && end > s.from
                && until.is_none_or(|u| end < u)
            {
                changes.push((end, p));
            }
        }
    }
    changes.retain(|(t, _)| *t <= now);
    changes.sort();
    changes.dedup();

    let mut sweep = Sweep {
        current: vec![State::default(); segments.len()],
        segments,
        contributing: HashMap::new(),
        ever: HashSet::new(),
        paid: 0,
        trials: 0,
    };
    let mut next_change = 0;
    replay(&mut sweep, &changes, &mut next_change, first, None);
    let mut last_conv_on = now.date_naive();
    for (k, &(_, end)) in spans.iter().enumerate() {
        let stock_at = (end - Duration::microseconds(1)).min(now);
        let conv = Conv {
            fx: input.fx,
            currency: input.currency,
            on: stock_at.date_naive(),
        };
        last_conv_on = conv.on;
        let start_paid = sweep.paid;
        let pt = &mut points[k];
        replay(
            &mut sweep,
            &changes,
            &mut next_change,
            end,
            Some((pt, &conv)),
        );
        pt.mrr_minor = sweep
            .current
            .iter()
            .filter_map(|s| s.mrr.as_ref())
            .filter_map(|m| conv.convert(m))
            .sum();
        pt.active_subscriptions = sweep.paid;
        pt.active_trials = sweep.trials;
        pt.churn_rate =
            (start_paid > 0).then(|| pt.churned_subscriptions as f64 / start_paid as f64);
    }

    // MRR without a rate, at the end of the last bucket.
    let conv = Conv {
        fx: input.fx,
        currency: input.currency,
        on: last_conv_on,
    };
    let mut totals: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for m in sweep.current.iter().filter_map(|s| s.mrr.as_ref()) {
        if conv.convert(m).is_none() {
            totals.entry(m.currency.to_ascii_uppercase()).or_default().1 += m.amount_minor;
        }
    }
    for (currency, amount) in unconverted_revenue {
        totals.entry(currency).or_default().0 += amount;
    }
    let unconverted = totals
        .into_iter()
        .map(|(currency, (revenue_minor, mrr_minor))| Unconverted {
            currency,
            revenue_minor,
            mrr_minor,
        })
        .collect();

    Metrics {
        currency: input.currency.to_owned(),
        history_start: input.history_start,
        points,
        unconverted,
    }
}

#[cfg(test)]
mod tests {
    use EventType::*;

    use super::*;
    use crate::fx::PerEur;
    use crate::testing::at;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn money(amount_minor: i64, currency: &str) -> Money {
        Money {
            amount_minor,
            currency: currency.into(),
        }
    }

    /// An active, auto-renewing $10 App Store subscription state of `purchase`.
    fn row(
        purchase: u128,
        customer: Option<u128>,
        as_of: DateTime<Utc>,
        expires: DateTime<Utc>,
        events: &[EventType],
    ) -> HistoryRow {
        HistoryRow {
            id: as_of.timestamp(),
            purchase_id: Uuid::from_u128(purchase),
            customer_id: customer.map(Uuid::from_u128),
            store: Store::AppStore,
            product_id: "monthly".into(),
            kind: PurchaseKind::Subscription,
            status: PurchaseStatus::Active,
            period_type: PeriodType::Normal,
            purchased_at: as_of,
            expires_at: Some(expires),
            grace_period_expires_at: None,
            auto_renew: true,
            price: Some(money(1000, "USD")),
            as_of,
            events: events.to_vec(),
            backfilled: false,
        }
    }

    fn ended(r: &HistoryRow, as_of: DateTime<Utc>, status: PurchaseStatus) -> HistoryRow {
        HistoryRow {
            id: as_of.timestamp(),
            status,
            auto_renew: false,
            as_of,
            events: vec![if status.is_revoked() {
                Refund
            } else {
                Expiration
            }],
            ..r.clone()
        }
    }

    struct Case {
        rows: Vec<HistoryRow>,
        periods: HashMap<(Store, String), BillingPeriod>,
        customers: Vec<DateTime<Utc>>,
        now: DateTime<Utc>,
    }

    impl Case {
        fn new(rows: Vec<HistoryRow>, now: DateTime<Utc>) -> Case {
            Case {
                rows,
                periods: HashMap::new(),
                customers: Vec::new(),
                now,
            }
        }

        fn run(&self, from: &str, to: &str, interval: Interval) -> Metrics {
            let mut fx = FxRates::default();
            fx.insert(date("2025-01-01"), "USD", PerEur::parse("1.25").unwrap());
            let range = Range::new(date(from), date(to), interval).unwrap();
            compute(
                &MetricsInput {
                    rows: &self.rows,
                    periods: &self.periods,
                    fx: &fx,
                    customers_created: &self.customers,
                    currency: "USD",
                    history_start: None,
                    now: self.now,
                },
                &range,
            )
        }
    }

    /// `[new, expansion, reactivation, contraction, churned]`.
    fn movement(p: &Point) -> [i64; 5] {
        [
            p.new_mrr_minor,
            p.expansion_mrr_minor,
            p.reactivation_mrr_minor,
            p.contraction_mrr_minor,
            p.churned_mrr_minor,
        ]
    }

    /// Single currency: each bucket's movement is exactly the change of the stock.
    fn assert_reconciles(m: &Metrics) {
        let mut prev = 0;
        for p in &m.points {
            assert_eq!(p.mrr_minor - prev, movement(p).iter().sum::<i64>(), "{p:?}");
            prev = p.mrr_minor;
        }
    }

    #[test]
    fn buckets_align_to_interval_starts() {
        let starts = |from, to, i| Range::new(date(from), date(to), i).unwrap().starts;
        // 2026-01-07 is a Wednesday.
        assert_eq!(
            starts("2026-01-07", "2026-01-20", Interval::Week),
            [date("2026-01-05"), date("2026-01-12"), date("2026-01-19")]
        );
        assert_eq!(
            starts("2026-01-15", "2026-03-02", Interval::Month),
            [date("2026-01-01"), date("2026-02-01"), date("2026-03-01")]
        );
        assert_eq!(starts("2026-01-31", "2026-02-02", Interval::Day).len(), 2);
        assert_eq!(
            Range::new(date("2026-01-01"), date("2026-01-01"), Interval::Day),
            Err(RangeError::Empty)
        );
        assert_eq!(
            Range::new(date("2026-01-01"), date("2023-01-01"), Interval::Day),
            Err(RangeError::Empty)
        );
        assert_eq!(
            starts("2026-01-01", "2028-09-27", Interval::Day).len(),
            1000
        );
        assert_eq!(
            Range::new(date("2026-01-01"), date("2028-09-28"), Interval::Day),
            Err(RangeError::TooManyBuckets)
        );
    }

    #[test]
    fn partial_first_bucket_counts_flows_from_from_only() {
        // Bought Tuesday 2026-01-06, the range starts on Wednesday.
        let m = Case::new(
            vec![row(1, Some(1), at(5), at(36), &[InitialPurchase])],
            at(20),
        )
        .run("2026-01-07", "2026-01-12", Interval::Week);
        assert_eq!(m.points[0].start, date("2026-01-05"));
        assert_eq!(m.points[0].mrr_minor, 1000);
        assert_eq!(m.points[0].new_mrr_minor, 0);
        assert_eq!(m.points[0].revenue_minor, 0);
    }

    #[test]
    fn mrr_renewal_and_churn() {
        let first = row(1, Some(1), at(4), at(35), &[InitialPurchase]);
        let renewal = row(1, Some(1), at(35), at(63), &[Renewal]);
        let m = Case::new(vec![first, renewal], at(365)).run(
            "2026-01-01",
            "2026-05-01",
            Interval::Month,
        );
        let [jan, feb, mar, apr] = &m.points[..] else {
            panic!("{m:?}")
        };
        assert_eq!(jan.start, date("2026-01-01"));
        assert_eq!((jan.mrr_minor, jan.active_subscriptions), (1000, 1));
        assert_eq!(movement(jan), [1000, 0, 0, 0, 0]);
        assert_eq!((jan.revenue_minor, jan.transactions), (1000, 1));
        assert_eq!(jan.churn_rate, None);
        assert_eq!(feb.mrr_minor, 1000);
        assert_eq!(movement(feb), [0; 5]);
        assert_eq!((feb.revenue_minor, feb.transactions), (1000, 1));
        assert_eq!(feb.churn_rate, Some(0.0));
        // Expired 2026-03-05 without a renewal.
        assert_eq!(mar.mrr_minor, 0);
        assert_eq!(movement(mar), [0, 0, 0, 0, -1000]);
        assert_eq!(mar.churned_subscriptions, 1);
        assert_eq!(mar.churn_rate, Some(1.0));
        assert_eq!(apr.churn_rate, None);
        assert_reconciles(&m);
    }

    #[test]
    fn new_versus_reactivation() {
        let first = row(1, Some(1), at(0), at(31), &[InitialPurchase]);
        let rows = vec![
            ended(&first, at(31), PurchaseStatus::Expired),
            first,
            // The same customer comes back with a new purchase.
            row(2, Some(1), at(40), at(71), &[InitialPurchase]),
            // Another customer's first subscription.
            row(3, Some(2), at(45), at(76), &[InitialPurchase]),
        ];
        let m = Case::new(rows, at(50)).run("2026-01-01", "2026-03-01", Interval::Month);
        assert_eq!(movement(&m.points[0]), [1000, 0, 0, 0, 0]);
        assert_eq!(movement(&m.points[1]), [1000, 0, 1000, 0, -1000]);
        assert_eq!(m.points[1].mrr_minor, 2000);
        assert_reconciles(&m);
    }

    #[test]
    fn expansion_and_contraction_via_product_change() {
        let monthly = row(1, Some(1), at(0), at(31), &[InitialPurchase]);
        let premium = HistoryRow {
            product_id: "premium".into(),
            price: Some(money(2000, "USD")),
            ..row(1, Some(1), at(10), at(41), &[ProductChange])
        };
        let yearly = HistoryRow {
            product_id: "yearly".into(),
            price: Some(money(6000, "USD")),
            ..row(1, Some(1), at(20), at(385), &[ProductChange])
        };
        let m = Case::new(vec![monthly, premium, yearly], at(25)).run(
            "2026-01-01",
            "2026-01-31",
            Interval::Day,
        );
        assert_eq!(movement(&m.points[0]), [1000, 0, 0, 0, 0]);
        assert_eq!(movement(&m.points[10]), [0, 1000, 0, 0, 0]);
        assert_eq!(movement(&m.points[20]), [0, 0, 0, -1500, 0]);
        assert_eq!(m.points[24].mrr_minor, 500);
        // Product changes are not charges.
        assert_eq!(m.points.iter().map(|p| p.transactions).sum::<u64>(), 1);
        assert_reconciles(&m);
    }

    #[test]
    fn late_renewal_within_tolerance_is_continuous() {
        let first = row(1, Some(1), at(0), at(31), &[InitialPurchase]);
        let renewed_after = |delay: Duration| {
            let mut r = row(1, Some(1), at(31) + delay, at(59), &[Renewal]);
            r.purchased_at = at(31);
            vec![first.clone(), r]
        };
        let run = |rows| Case::new(rows, at(58)).run("2026-01-01", "2026-03-01", Interval::Month);

        let m = run(renewed_after(Duration::days(2)));
        assert_eq!(movement(&m.points[1]), [0; 5]);
        assert_eq!(m.points[1].churned_subscriptions, 0);
        assert_eq!(m.points[1].mrr_minor, 1000);

        let m = run(renewed_after(Duration::days(5)));
        assert_eq!(movement(&m.points[1]), [0, 0, 1000, 0, -1000]);
        assert_eq!(m.points[1].churned_subscriptions, 1);
        assert_eq!(m.points[1].mrr_minor, 1000);
    }

    #[test]
    fn renewal_pending_at_now_is_not_churn_yet() {
        let rows = vec![row(1, Some(1), at(0), at(31), &[InitialPurchase])];
        let feb = |rows: Vec<HistoryRow>, now| {
            Case::new(rows, now)
                .run("2026-02-01", "2026-03-01", Interval::Month)
                .points[0]
                .clone()
        };
        let pending = feb(rows.clone(), at(33));
        assert_eq!((pending.mrr_minor, pending.churned_mrr_minor), (1000, 0));
        // Past the tolerance the churn is dated at the expiry.
        let lapsed = feb(rows.clone(), at(35));
        assert_eq!((lapsed.mrr_minor, lapsed.churned_mrr_minor), (0, -1000));
        // Cancelled subscriptions are not expected to renew.
        let mut cancelled = rows[0].clone();
        cancelled.auto_renew = false;
        cancelled.status = PurchaseStatus::Cancelled;
        let p = feb(vec![cancelled], at(32));
        assert_eq!((p.mrr_minor, p.churned_mrr_minor), (0, -1000));
    }

    #[test]
    fn grace_period_counts_until_grace_end() {
        let first = row(1, Some(1), at(0), at(31), &[InitialPurchase]);
        let grace = HistoryRow {
            status: PurchaseStatus::GracePeriod,
            grace_period_expires_at: Some(at(47)),
            ..row(1, Some(1), at(31), at(31), &[BillingIssue])
        };
        let expired = ended(&grace, at(50), PurchaseStatus::Expired);
        let m = Case::new(vec![first, grace, expired], at(100)).run(
            "2026-02-01",
            "2026-02-21",
            Interval::Day,
        );
        assert_eq!(m.points[0].churned_mrr_minor, 0);
        assert_eq!(m.points[15].mrr_minor, 1000); // Feb 16
        assert_eq!(m.points[16].mrr_minor, 0); // Feb 17: grace ended
        assert_eq!(m.points[16].churned_mrr_minor, -1000);
        assert_eq!(m.points[16].churned_subscriptions, 1);
        assert_eq!(m.points[19].churned_mrr_minor, 0); // expiration reported later
    }

    /// A Play Store monthly subscription with a 7-day trial: Google reports the
    /// subscription's first start as `purchased_at` in every state.
    fn play_trial() -> Vec<HistoryRow> {
        let play = |as_of, expires, period_type, events: &[EventType]| HistoryRow {
            store: Store::PlayStore,
            period_type,
            purchased_at: at(0),
            ..row(1, Some(1), as_of, expires, events)
        };
        vec![
            play(at(0), at(7), PeriodType::Trial, &[InitialPurchase]),
            play(at(7), at(38), PeriodType::Normal, &[Renewal]),
            play(at(38), at(66), PeriodType::Normal, &[Renewal]),
        ]
    }

    #[test]
    fn trial_is_excluded_and_conversion_counted() {
        let m = Case::new(play_trial(), at(20)).run("2026-01-01", "2026-01-10", Interval::Day);
        let day0 = &m.points[0];
        assert_eq!(
            (
                day0.mrr_minor,
                day0.active_trials,
                day0.active_subscriptions
            ),
            (0, 1, 0)
        );
        assert_eq!(
            (day0.new_trials, day0.transactions, day0.revenue_minor),
            (1, 0, 0)
        );
        assert_eq!(movement(day0), [0; 5]);
        let day7 = &m.points[7];
        assert_eq!(
            (
                day7.mrr_minor,
                day7.active_trials,
                day7.active_subscriptions
            ),
            (1000, 0, 1)
        );
        assert_eq!(day7.trial_conversions, 1);
        assert_eq!(movement(day7), [1000, 0, 0, 0, 0]);
        assert_eq!((day7.transactions, day7.revenue_minor), (1, 1000));
        assert_reconciles(&m);
    }

    #[test]
    fn period_inferred_from_history_for_play_renewals() {
        // `expires_at - purchased_at` of the last state is 66 days; consecutive
        // expiries (38 → 66) are one month.
        let mut case = Case::new(play_trial(), at(60));
        let feb = case.run("2026-02-01", "2026-03-01", Interval::Month);
        assert_eq!(feb.points[0].mrr_minor, 1000);
        // The configured period always wins: annual → monthly.
        case.periods
            .insert((Store::PlayStore, "monthly".into()), BillingPeriod::Year);
        let feb = case.run("2026-02-01", "2026-03-01", Interval::Month);
        assert_eq!(feb.points[0].mrr_minor, 83);
    }

    #[test]
    fn annual_price_fx_and_unconverted_currencies() {
        let yearly = |p, c, price| HistoryRow {
            product_id: "yearly".into(),
            price: Some(price),
            ..row(p, Some(c), at(0), at(365), &[InitialPurchase])
        };
        let rows = vec![
            yearly(1, 1, money(12000, "EUR")),
            yearly(2, 2, money(6000, "AED")),
        ];
        let m = Case::new(rows, at(10)).run("2026-01-01", "2026-02-01", Interval::Month);
        let jan = &m.points[0];
        // 120 EUR/yr = 10 EUR/mo = 12.50 USD.
        assert_eq!(jan.mrr_minor, 1250);
        assert_eq!(jan.new_mrr_minor, 1250);
        assert_eq!(jan.revenue_minor, 15000);
        assert_eq!((jan.transactions, jan.active_subscriptions), (2, 2));
        assert_eq!(
            m.unconverted,
            [Unconverted {
                currency: "AED".into(),
                revenue_minor: 6000,
                mrr_minor: 500,
            }]
        );
    }

    #[test]
    fn revenue_refunds_and_customers() {
        let one_time = HistoryRow {
            kind: PurchaseKind::OneTime,
            expires_at: None,
            product_id: "lifetime".into(),
            price: Some(money(5000, "USD")),
            ..row(1, Some(1), at(2), at(2), &[InitialPurchase])
        };
        let refunded = ended(&one_time, at(3), PurchaseStatus::Refunded);
        let backfilled = HistoryRow {
            backfilled: true,
            events: vec![],
            ..row(2, Some(2), at(1), at(32), &[])
        };
        let mut case = Case::new(vec![one_time, refunded, backfilled], at(10));
        case.customers = vec![at(0), at(2), at(2), at(40)];
        let m = case.run("2026-01-01", "2026-01-05", Interval::Day);
        let per_day = |f: fn(&Point) -> i64| m.points.iter().map(f).collect::<Vec<_>>();
        assert_eq!(per_day(|p| p.revenue_minor), [0, 0, 5000, 0]);
        assert_eq!(per_day(|p| p.transactions as i64), [0, 0, 1, 0]);
        assert_eq!(per_day(|p| p.refunded_minor), [0, 0, 0, 5000]);
        assert_eq!(per_day(|p| p.new_customers as i64), [1, 0, 2, 0]);
        // One-time purchases have no MRR; the backfilled subscription does.
        assert_eq!(per_day(|p| p.mrr_minor), [0, 1000, 1000, 1000]);
        assert_eq!(per_day(|p| p.new_mrr_minor), [0, 1000, 0, 0]);
    }

    #[test]
    fn linking_an_unlinked_purchase_is_not_movement() {
        let rows = vec![
            row(1, None, at(0), at(31), &[InitialPurchase]),
            row(1, Some(7), at(5), at(31), &[]),
            // Never linked (or erased): its own pseudo-customer.
            row(2, None, at(3), at(34), &[InitialPurchase]),
        ];
        let m = Case::new(rows, at(10)).run("2026-01-01", "2026-01-08", Interval::Day);
        assert_eq!(movement(&m.points[0]), [1000, 0, 0, 0, 0]);
        assert_eq!(movement(&m.points[3]), [1000, 0, 0, 0, 0]);
        assert_eq!(movement(&m.points[5]), [0; 5]);
        assert_eq!(m.points[6].mrr_minor, 2000);
    }

    #[test]
    fn movement_reconciles_across_intervals() {
        let mut rows = play_trial();
        let first = row(10, Some(10), at(0), at(31), &[InitialPurchase]);
        rows.push(ended(&first, at(31), PurchaseStatus::Expired));
        rows.push(first);
        rows.push(row(11, Some(10), at(40), at(71), &[InitialPurchase]));
        rows.push(row(12, Some(10), at(45), at(76), &[InitialPurchase]));
        rows.push(HistoryRow {
            product_id: "premium".into(),
            price: Some(money(2500, "USD")),
            ..row(12, Some(10), at(50), at(81), &[ProductChange])
        });
        rows.push(row(13, None, at(20), at(51), &[InitialPurchase]));
        rows.push(row(13, Some(11), at(25), at(51), &[]));
        let refunded = row(14, Some(12), at(3), at(34), &[InitialPurchase]);
        rows.push(ended(&refunded, at(9), PurchaseStatus::Refunded));
        rows.push(refunded);
        for interval in [Interval::Day, Interval::Week, Interval::Month] {
            let m = Case::new(rows.clone(), at(80)).run("2025-12-01", "2026-04-01", interval);
            assert_reconciles(&m);
            assert!(m.points.iter().any(|p| p.churned_mrr_minor < 0));
            assert!(m.points.iter().any(|p| p.reactivation_mrr_minor > 0));
            assert!(m.points.iter().any(|p| p.expansion_mrr_minor > 0));
        }
    }
}
