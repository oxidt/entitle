//! The revenue page: overview tiles, MRR movement and revenue charts, and the
//! purchase activity feed.

use chrono::{Datelike, Duration, NaiveDate, Utc};
use entitle_core::metrics::{Interval, Metrics, Point, Range};
use entitle_core::{Environment, ProjectId};
use serde::Serialize;
use topcoat::{
    Result,
    context::Cx,
    router::{RouterBuilder, page, query_params},
    view::{View, component, view},
};

use super::customers::customer_href;
use super::format::{full_date, int, long_month, money, percent, short_month};
use super::shell::{Projects, Section, app, href, state};
use super::ui::{local_time, status_badge, store_name};
use crate::routes::admin::{ActivityOut, Project, recent_activity};

pub fn register(builder: RouterBuilder) -> RouterBuilder {
    builder.page(revenue).page(activity)
}

// Series colors are theme tokens (styles.css): each theme has its own
// validated steps.
const NEW: &str = "var(--series-new)";
const EXPANSION: &str = "var(--series-expansion)";
const REACTIVATION: &str = "var(--series-reactivation)";
const CONTRACTION: &str = "var(--series-contraction)";
const CHURN: &str = "var(--series-churn)";
const NET: &str = "var(--series-net)";
const COMPARE: &str = "var(--series-compare)";

const ACTIVITY_LIMIT: i64 = 50;

#[derive(Clone, Copy, PartialEq)]
enum RangeKind {
    M3,
    M6,
    M12,
    All,
}

impl RangeKind {
    const ALL: [RangeKind; 4] = [RangeKind::M3, RangeKind::M6, RangeKind::M12, RangeKind::All];

    fn label(self) -> &'static str {
        match self {
            RangeKind::M3 => "3M",
            RangeKind::M6 => "6M",
            RangeKind::M12 => "12M",
            RangeKind::All => "All",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.label() == s)
    }
}

#[query_params]
struct RevenueQuery {
    range: Option<String>,
    compare: Option<String>,
    env: Option<String>,
}

/// The page's state, as it appears in the URL.
#[derive(Clone, Copy)]
struct Params {
    range: RangeKind,
    compare: bool,
    env: Environment,
}

impl Params {
    fn from_query(cx: &Cx) -> Self {
        let q = query_params::<RevenueQuery>(cx).ok();
        let q = q.as_ref();
        Self {
            range: q
                .and_then(|q| q.range.as_deref())
                .and_then(RangeKind::parse)
                .unwrap_or(RangeKind::M12),
            compare: q.is_some_and(|q| q.compare.as_deref() == Some("1")),
            env: match q.and_then(|q| q.env.as_deref()) {
                Some("sandbox") => Environment::Sandbox,
                _ => Environment::Production,
            },
        }
    }

    fn href(self, project: &Project) -> String {
        let mut url = format!(
            "{}&range={}",
            href("/dashboard/", project),
            self.range.label()
        );
        if self.compare {
            url.push_str("&compare=1");
        }
        if self.env == Environment::Sandbox {
            url.push_str("&env=sandbox");
        }
        url
    }
}

fn month_start(d: NaiveDate, add_months: i32) -> NaiveDate {
    let m = d.year() * 12 + d.month0() as i32 + add_months;
    NaiveDate::from_ymd_opt(m.div_euclid(12), m.rem_euclid(12) as u32 + 1, 1).expect("valid month")
}

struct Bounds {
    cur: (NaiveDate, NaiveDate),
    prev: (NaiveDate, NaiveDate),
}

fn range_bounds(range: RangeKind, project: &Project, today: NaiveDate) -> Bounds {
    let to = month_start(today, 1);
    let months = match range {
        RangeKind::M3 => 3,
        RangeKind::M6 => 6,
        RangeKind::M12 => 12,
        RangeKind::All => {
            let created = month_start(project.created_at.date_naive(), 0);
            ((to.year() - created.year()) * 12 + to.month0() as i32 - created.month0() as i32)
                .max(1)
        }
    };
    let from = month_start(to, -months);
    Bounds {
        cur: (from, to),
        prev: (month_start(from, -months), from),
    }
}

async fn load(
    cx: &Cx,
    project: &Project,
    from: NaiveDate,
    to: NaiveDate,
    interval: Interval,
) -> anyhow::Result<Metrics> {
    let range = Range::new(from, to, interval)?;
    crate::metrics::load_and_compute(
        &state(cx).db,
        ProjectId(project.id),
        &project.reporting_currency,
        &range,
    )
    .await
}

fn net_new(p: &Point) -> i64 {
    p.new_mrr_minor
        + p.expansion_mrr_minor
        + p.reactivation_mrr_minor
        + p.contraction_mrr_minor
        + p.churned_mrr_minor
}

fn mean(xs: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = xs.fold((0.0, 0u32), |(s, n), x| (s + x, n + 1));
    (n > 0).then(|| sum / f64::from(n))
}

/// Every figure in every bucket is zero (or unknown).
fn is_empty(m: &Metrics) -> bool {
    m.points.iter().all(|p| {
        p.mrr_minor == 0
            && p.active_subscriptions == 0
            && p.active_trials == 0
            && net_new(p) == 0
            && [
                p.new_mrr_minor,
                p.expansion_mrr_minor,
                p.reactivation_mrr_minor,
                p.contraction_mrr_minor,
                p.churned_mrr_minor,
            ]
            .iter()
            .all(|v| *v == 0)
            && p.revenue_minor == 0
            && p.refunded_minor == 0
            && p.transactions == 0
            && p.new_customers == 0
            && p.new_trials == 0
            && p.trial_conversions == 0
            && p.churned_subscriptions == 0
            && p.churn_rate.is_none_or(|r| r == 0.0)
    })
}

/// The bucket has not ended yet (the current month).
fn in_progress(p: &Point, today: NaiveDate) -> bool {
    p.start >= month_start(today, 0)
}

fn conv(p: &Point) -> Option<f64> {
    (p.new_trials > 0).then(|| p.trial_conversions as f64 / p.new_trials as f64)
}

fn rate_text(r: Option<f64>) -> String {
    r.map_or_else(|| "—".to_owned(), |r| percent(r, 2))
}

// ---------------------------------------------------------------------------
// Chart configuration for chart.js

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Chart {
    labels: Vec<String>,
    height: u32,
    partial_from: i64,
    axis: Axis,
    series: Vec<Series>,
    tips: Vec<Tip>,
}

#[derive(Serialize)]
struct Axis {
    kind: &'static str,
    currency: String,
}

#[derive(Serialize)]
struct Series {
    label: String,
    color: &'static str,
    kind: &'static str,
    values: Vec<Option<f64>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    compare: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    markers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'static str>,
}

impl Series {
    fn new(
        label: impl Into<String>,
        color: &'static str,
        kind: &'static str,
        values: Vec<Option<f64>>,
    ) -> Self {
        Self {
            label: label.into(),
            color,
            kind,
            values,
            compare: false,
            markers: false,
            key: None,
        }
    }
    fn bar(label: &str, color: &'static str, values: impl Iterator<Item = i64>) -> Self {
        Self::new(
            label,
            color,
            "bar",
            values.map(|v| Some(v as f64)).collect(),
        )
    }
    fn line(label: &str, color: &'static str, values: impl Iterator<Item = Option<f64>>) -> Self {
        Self::new(label, color, "line", values.collect())
    }
    fn key(mut self, key: &'static str) -> Self {
        self.key = Some(key);
        self
    }
}

#[derive(Serialize)]
struct Tip {
    title: String,
    rows: Vec<TipRow>,
}

#[derive(Serialize, Default)]
struct TipRow {
    label: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    line: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    strong: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'static str>,
}

impl TipRow {
    fn plain(label: &str, value: String) -> Self {
        Self {
            label: label.into(),
            value,
            ..Self::default()
        }
    }
    fn colored(label: &str, value: String, color: &'static str) -> Self {
        Self {
            color: Some(color),
            ..Self::plain(label, value)
        }
    }
    fn line(mut self) -> Self {
        self.line = true;
        self
    }
    fn strong(mut self) -> Self {
        self.strong = true;
        self
    }
    fn key(mut self, key: &'static str) -> Self {
        self.key = Some(key);
        self
    }
}

struct Movement {
    key: &'static str,
    label: &'static str,
    color: &'static str,
    field: fn(&Point) -> i64,
}

const MOVEMENT: [Movement; 5] = [
    Movement {
        key: "new",
        label: "New",
        color: NEW,
        field: |p| p.new_mrr_minor,
    },
    Movement {
        key: "expansion",
        label: "Expansion",
        color: EXPANSION,
        field: |p| p.expansion_mrr_minor,
    },
    Movement {
        key: "reactivation",
        label: "Reactivation",
        color: REACTIVATION,
        field: |p| p.reactivation_mrr_minor,
    },
    Movement {
        key: "contraction",
        label: "Contraction",
        color: CONTRACTION,
        field: |p| p.contraction_mrr_minor,
    },
    Movement {
        key: "churn",
        label: "Churn",
        color: CHURN,
        field: |p| p.churned_mrr_minor,
    },
];

/// The charts' shared inputs.
struct Plot<'a> {
    pts: &'a [Point],
    prev: Option<&'a [Point]>,
    ccy: &'a str,
    today: NaiveDate,
}

impl Plot<'_> {
    fn labels(&self) -> Vec<String> {
        self.pts.iter().map(|p| short_month(p.start)).collect()
    }

    fn title(&self, i: usize) -> String {
        let p = &self.pts[i];
        long_month(p.start)
            + if in_progress(p, self.today) {
                " (to date)"
            } else {
                ""
            }
    }

    fn periods(&self) -> Vec<String> {
        (0..self.pts.len()).map(|i| self.title(i)).collect()
    }

    fn partial_from(&self) -> i64 {
        self.pts
            .iter()
            .position(|p| in_progress(p, self.today))
            .map_or(-1, |i| i as i64)
    }

    /// The previous-period overlay series, when comparing.
    fn compare(&self, label: &str, f: impl Fn(&Point) -> Option<f64>) -> Option<Series> {
        self.prev.map(|prev| {
            let mut s = Series::line(label, COMPARE, prev.iter().map(f));
            s.compare = true;
            s
        })
    }

    /// A month-to-date rate is not comparable to a full month's: leave it off the line.
    fn ended(&self, f: impl Fn(&Point) -> Option<f64>) -> impl Iterator<Item = Option<f64>> {
        self.pts.iter().map(move |p| {
            if in_progress(p, self.today) {
                None
            } else {
                f(p)
            }
        })
    }

    fn prev_row(&self, i: usize, f: impl Fn(&Point) -> String) -> Option<TipRow> {
        self.prev
            .map(|prev| TipRow::colored(&long_month(prev[i].start), f(&prev[i]), COMPARE).line())
    }

    fn chart(
        &self,
        height: u32,
        axis: &'static str,
        series: Vec<Series>,
        tip: impl Fn(usize) -> Vec<TipRow>,
    ) -> String {
        let chart = Chart {
            labels: self.labels(),
            height,
            partial_from: self.partial_from(),
            axis: Axis {
                kind: axis,
                currency: self.ccy.to_owned(),
            },
            series,
            tips: (0..self.pts.len())
                .map(|i| Tip {
                    title: self.title(i),
                    rows: tip(i),
                })
                .collect(),
        };
        serde_json::to_string(&chart).expect("chart config serializes")
    }
}

// ---------------------------------------------------------------------------
// Pages

#[page("/dashboard/")]
async fn revenue(cx: &Cx) -> Result<impl View> {
    let projects = Projects::load(cx).await?;
    let v = Params::from_query(cx);
    Ok(view! {
        app(
            projects: &projects,
            section: Section::Revenue,
            if let Some(project) = projects.current() {
                overview(project: project)
                charts(project: project, view: v)
                activity_feed(project: project, env: v.env)
            }
        )
    })
}

#[component]
async fn delta(cx: &Cx, value: Option<f64>, pp: bool, good_when_up: bool) -> Result<impl View> {
    let value = value.filter(|v| v.is_finite());
    Ok(view! {
        match value {
            None => {
                <span class="delta flat">"—"</span>
            }
            Some(value) => {
                let shown = if pp {
                    format!("{:.2} pp", (value * 100.0).abs())
                } else {
                    format!("{:.1}%", (value * 100.0).abs())
                };
                if value.abs() < 0.0005 {
                    <span class="delta flat">
                        "±"
                        (shown)
                    </span>
                } else {
                    let up = value > 0.0;
                    let good = up == good_when_up;
                    <span class=(if good { "delta good" } else { "delta bad" })>
                        <span aria-hidden="true">(if up { "▲" } else { "▼" })</span>
                        " "
                        (shown)
                        <span class="sr-only">
                            (if good { " (better)" } else { " (worse)" })
                        </span>
                    </span>
                }
            }
        }
    })
}

#[component]
async fn tile(cx: &Cx, label: &str, value: String, sub: Option<&str>) -> Result<impl View> {
    Ok(view! {
        <div class="tile card-elevated bg-base-100">
            <div class="tile-label">(label)</div>
            <div class="tile-value">(value)</div>
            if let Some(sub) = sub {
                <div class="tile-sub">(sub)</div>
            }
        </div>
    })
}

#[component]
async fn overview(cx: &Cx, project: &Project) -> Result<impl View> {
    let today = Utc::now().date_naive();
    let m = load(
        cx,
        project,
        today - Duration::days(27),
        today + Duration::days(1),
        Interval::Day,
    )
    .await;
    Ok(view! {
        match m {
            Err(e) => {
                <p class="notice alert alert-soft alert-error">
                    "Could not load overview: "
                    (e.to_string())
                </p>
            }
            Ok(m) => {
                let last = m.points.last();
                let dash = || "—".to_owned();
                let revenue_28d: i64 = m.points.iter().map(|p| p.revenue_minor).sum();
                let new_customers: u64 = m.points.iter().map(|p| p.new_customers).sum();
                <section class="tiles" aria-label="Overview">
                    tile(
                        label: "MRR",
                        value: last.map_or_else(
                            dash,
                            |l| money(l.mrr_minor, &m.currency),
                        ),
                        sub: None
                    )
                    tile(
                        label: "Active subscriptions",
                        value: last.map_or_else(
                            dash,
                            |l| int(l.active_subscriptions as i64),
                        ),
                        sub: None
                    )
                    tile(
                        label: "Active trials",
                        value: last.map_or_else(dash, |l| int(l.active_trials as i64)),
                        sub: None
                    )
                    tile(
                        label: "Revenue",
                        value: money(revenue_28d, &m.currency),
                        sub: Some("Last 28 days")
                    )
                    tile(
                        label: "New customers",
                        value: int(new_customers as i64),
                        sub: Some("Last 28 days")
                    )
                </section>
            }
        }
    })
}

#[component]
async fn charts(cx: &Cx, project: &Project, view: Params) -> Result<impl View> {
    let today = Utc::now().date_naive();
    let bounds = range_bounds(view.range, project, today);
    // "All" starts at project creation, so its previous period is always empty.
    let can_compare = view.range != RangeKind::All;
    let loaded = async {
        let cur = load(cx, project, bounds.cur.0, bounds.cur.1, Interval::Month).await?;
        let prev = load(cx, project, bounds.prev.0, bounds.prev.1, Interval::Month).await?;
        anyhow::Ok((cur, prev))
    }
    .await;
    let toggled = Params {
        compare: !view.compare,
        ..view
    };
    Ok(view! {
        <div class="toolbar">
            <div class="tabs tabs-box tabs-sm" aria-label="Range">
                for r in RangeKind::ALL {
                    <a
                        class="tab"
                        href=(Params { range: r, ..view }.href(project))
                        aria-current=((r == view.range).then_some("page"))
                    >
                        (r.label())
                    </a>
                }
            </div>
            <label class="switch">
                <input
                    class="toggle toggle-sm"
                    type="checkbox"
                    checked=(can_compare && view.compare)
                    disabled=(!can_compare)
                    data-navigate=(toggled.href(project))
                >
                "Compare to previous period"
            </label>
        </div>
        match loaded {
            Err(e) => {
                <p class="notice alert alert-soft alert-error">
                    "Could not load metrics: "
                    (e.to_string())
                </p>
            }
            Ok((cur, prev)) => {
                <div>
                    notices(m: &cur, from: bounds.cur.0)
                    if is_empty(&cur) {
                        <p class="notice alert">"No purchases in this range yet."</p>
                    } else {
                        chart_cards(
                            cur: &cur,
                            prev: &prev,
                            compare: can_compare && view.compare
                        )
                    }
                </div>
            }
        }
    })
}

#[component]
async fn notices(cx: &Cx, m: &Metrics, from: NaiveDate) -> Result<impl View> {
    let history_start = m
        .history_start
        .map(|t| t.date_naive())
        .filter(|start| from < *start);
    let unconverted = m
        .unconverted
        .iter()
        .map(|u| {
            format!(
                "{} revenue, {} MRR",
                money(u.revenue_minor, &u.currency),
                money(u.mrr_minor, &u.currency)
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let currencies = m
        .unconverted
        .iter()
        .map(|u| u.currency.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(view! {
        if let Some(start) = history_start {
            <p class="notice alert">
                <span aria-hidden="true">"ⓘ "</span>
                "History starts "
                (full_date(start))
                ". Values before that date are incomplete."
            </p>
        }
        if !m.unconverted.is_empty() {
            <p class="notice alert alert-soft alert-warning">
                <span aria-hidden="true">"⚠ "</span>
                "No exchange rate for "
                (currencies)
                "; excluded from the figures below: "
                (unconverted)
                "."
            </p>
        }
    })
}

#[derive(Clone, Copy)]
struct LegendItem<'a> {
    label: &'a str,
    color: &'static str,
    line: bool,
    /// A toggle button for this key.
    toggle: Option<&'static str>,
    /// Hidden together with this key's series.
    follows: Option<&'static str>,
}

impl<'a> LegendItem<'a> {
    fn new(label: &'a str, color: &'static str) -> Self {
        Self {
            label,
            color,
            line: false,
            toggle: None,
            follows: None,
        }
    }
    fn line(mut self) -> Self {
        self.line = true;
        self
    }
}

#[component]
async fn legend(cx: &Cx, items: Vec<LegendItem<'_>>) -> Result<impl View> {
    Ok(view! {
        <div class="legend">
            for it in items {
                let key_class = if it.line { "key line" } else { "key" };
                let swatch = format!("background: {}", it.color);
                if let Some(key) = it.toggle {
                    <button
                        type="button"
                        class="legend-item"
                        aria-pressed="true"
                        data-legend-key=(key)
                    >
                        <span class=(key_class) style=(&swatch)></span>
                        (it.label)
                    </button>
                } else {
                    <span class="legend-item static" data-legend-follows=(it.follows)>
                        <span class=(key_class) style=(&swatch)></span>
                        (it.label)
                    </span>
                }
            }
        </div>
    })
}

#[component]
async fn data_table(
    cx: &Cx,
    labels: Vec<String>,
    columns: Vec<(&str, Vec<String>)>,
) -> Result<impl View> {
    Ok(view! {
        <details class="table-view">
            <summary>"Show data table"</summary>
            <div class="table-scroll">
                <table>
                    <thead>
                        <tr>
                            <th>"Period"</th>
                            for (label, _) in &columns {
                                <th>(*label)</th>
                            }
                        </tr>
                    </thead>
                    <tbody>
                        for (i, l) in labels.iter().enumerate() {
                            <tr>
                                <td>(l)</td>
                                for (_, values) in &columns {
                                    <td>(&values[i])</td>
                                }
                            </tr>
                        }
                    </tbody>
                </table>
            </div>
        </details>
    })
}

#[component]
async fn kpi(
    cx: &Cx,
    label: &str,
    value: String,
    delta_value: Option<f64>,
    pp: bool,
    good_when_up: bool,
    sub: &str,
) -> Result<impl View> {
    Ok(view! {
        <div class="kpi">
            <div class="kpi-label">(label)</div>
            <div class="kpi-value">(value)</div>
            <div class="kpi-sub">
                delta(value: delta_value, pp: pp, good_when_up: good_when_up)
                (sub)
            </div>
        </div>
    })
}

#[component]
async fn chart_cards(cx: &Cx, cur: &Metrics, prev: &Metrics, compare: bool) -> Result<impl View> {
    let today = Utc::now().date_naive();
    let ccy = cur.currency.as_str();
    let pts = cur.points.as_slice();
    // The previous period is aligned bucket by bucket with the current one.
    let prev_pts = (prev.points.len() == pts.len()).then_some(prev.points.as_slice());
    let plot = Plot {
        pts,
        prev: prev_pts.filter(|_| compare),
        ccy,
        today,
    };
    let show_prev = plot.prev.is_some();

    // Header KPIs.
    let mrr_now = pts.last().map_or(0, |p| p.mrr_minor);
    let mrr_start = prev
        .points
        .last()
        .or(pts.first())
        .map_or(0, |p| p.mrr_minor);
    let done: Vec<&Point> = pts.iter().filter(|p| !in_progress(p, today)).collect();
    let net_avg = mean(done.iter().map(|p| net_new(p) as f64));
    let net_prev = mean(prev.points.iter().map(|p| net_new(p) as f64));
    let churn_avg = mean(done.iter().filter_map(|p| p.churn_rate));
    let churn_prev = mean(prev.points.iter().filter_map(|p| p.churn_rate));
    let rel = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) if b != 0.0 => Some((a - b) / b.abs()),
        _ => None,
    };

    let money_of = |f: fn(&Point) -> i64| move |p: &Point| money(f(p), ccy);
    let periods = plot.periods();

    // MRR movement.
    let mut movement: Vec<Series> = MOVEMENT
        .iter()
        .map(|s| Series::bar(s.label, s.color, pts.iter().map(s.field)).key(s.key))
        .collect();
    movement.extend(
        plot.compare("Net new, previous period", |p| Some(net_new(p) as f64))
            .map(|s| s.key("net")),
    );
    let mut net =
        Series::line("Net new", NET, pts.iter().map(|p| Some(net_new(p) as f64))).key("net");
    net.markers = true;
    movement.push(net);
    let movement_chart = plot.chart(300, "money", movement, |i| {
        let p = &pts[i];
        let mut rows = vec![
            TipRow::colored("Net new", money(net_new(p), ccy), NET)
                .line()
                .strong(),
        ];
        rows.extend(
            MOVEMENT
                .iter()
                .map(|s| TipRow::colored(s.label, money((s.field)(p), ccy), s.color).key(s.key)),
        );
        rows.extend(plot.prev_row(i, |q| money(net_new(q), ccy)));
        rows
    });
    let mut movement_legend: Vec<LegendItem> = MOVEMENT
        .iter()
        .map(|s| LegendItem {
            toggle: Some(s.key),
            ..LegendItem::new(s.label, s.color)
        })
        .collect();
    movement_legend.push(LegendItem {
        toggle: Some("net"),
        ..LegendItem::new("Net new", NET).line()
    });
    if show_prev {
        movement_legend.push(LegendItem {
            follows: Some("net"),
            ..LegendItem::new("Net new, previous period", COMPARE).line()
        });
    }
    let mut movement_columns: Vec<(&str, Vec<String>)> = MOVEMENT
        .iter()
        .map(|s| {
            (
                s.label,
                pts.iter().map(|p| money((s.field)(p), ccy)).collect(),
            )
        })
        .collect();
    movement_columns.push((
        "Net new",
        pts.iter().map(|p| money(net_new(p), ccy)).collect(),
    ));
    movement_columns.push(("MRR", pts.iter().map(money_of(|p| p.mrr_minor)).collect()));

    // Revenue.
    let mut revenue_series = vec![
        Series::bar("Revenue", NEW, pts.iter().map(|p| p.revenue_minor)),
        Series::bar("Refunds", CHURN, pts.iter().map(|p| -p.refunded_minor)),
    ];
    revenue_series
        .extend(plot.compare("Revenue, previous period", |p| Some(p.revenue_minor as f64)));
    let revenue_chart = plot.chart(300, "money", revenue_series, |i| {
        let p = &pts[i];
        let mut rows = vec![
            TipRow::colored("Revenue", money(p.revenue_minor, ccy), NEW).strong(),
            TipRow::colored("Refunds", money(p.refunded_minor, ccy), CHURN),
            TipRow::plain("Transactions", int(p.transactions as i64)),
        ];
        rows.extend(plot.prev_row(i, |q| money(q.revenue_minor, ccy)));
        rows
    });
    let mut revenue_legend = vec![
        LegendItem::new("Revenue", NEW),
        LegendItem::new("Refunds", CHURN),
    ];
    if show_prev {
        revenue_legend.push(LegendItem::new("Revenue, previous period", COMPARE).line());
    }

    // Active subscriptions and trials.
    let count_chart =
        |height: u32, label: &'static str, color: &'static str, f: fn(&Point) -> u64| {
            let mut series: Vec<Series> = plot
                .compare("Previous period", |p| Some(f(p) as f64))
                .into_iter()
                .collect();
            series.push(Series::line(
                label,
                color,
                pts.iter().map(|p| Some(f(p) as f64)),
            ));
            plot.chart(height, "count", series, |i| {
                let mut rows = vec![TipRow::colored(label, int(f(&pts[i]) as i64), color).line()];
                rows.extend(plot.prev_row(i, |q| int(f(q) as i64)));
                rows
            })
        };
    let subs_chart = count_chart(160, "Active subscriptions", NEW, |p| p.active_subscriptions);
    let trials_chart = count_chart(120, "Active trials", EXPANSION, |p| p.active_trials);

    // Churn rate.
    let mut churn_series: Vec<Series> = plot
        .compare("Previous period", |p| p.churn_rate)
        .into_iter()
        .collect();
    churn_series.push(Series::line(
        "Churn rate",
        CHURN,
        plot.ended(|p| p.churn_rate),
    ));
    let churn_chart = plot.chart(240, "pct", churn_series, |i| {
        let p = &pts[i];
        let mut rows = vec![
            TipRow::colored("Churn rate", rate_text(p.churn_rate), CHURN)
                .line()
                .strong(),
            TipRow::plain("Churned subscriptions", int(p.churned_subscriptions as i64)),
        ];
        rows.extend(plot.prev_row(i, |q| rate_text(q.churn_rate)));
        rows
    });

    // Trial conversion.
    let mut conv_series: Vec<Series> = plot.compare("Previous period", conv).into_iter().collect();
    conv_series.push(Series::line(
        "Trial conversion",
        REACTIVATION,
        plot.ended(conv),
    ));
    let conv_chart = plot.chart(240, "pct", conv_series, |i| {
        let p = &pts[i];
        let mut rows = vec![
            TipRow::colored("Conversion", rate_text(conv(p)), REACTIVATION)
                .line()
                .strong(),
            TipRow::plain("New trials", int(p.new_trials as i64)),
            TipRow::plain("Conversions", int(p.trial_conversions as i64)),
        ];
        rows.extend(plot.prev_row(i, |q| rate_text(conv(q))));
        rows
    });

    let column = |f: &dyn Fn(&Point) -> String| pts.iter().map(f).collect::<Vec<_>>();
    Ok(view! {
        <section class="card card-elevated bg-base-100">
            <h2>"Recurring revenue"</h2>
            <div class="kpis">
                kpi(
                    label: "MRR",
                    value: money(mrr_now, ccy),
                    delta_value: rel(Some(mrr_now as f64), Some(mrr_start as f64)),
                    pp: false,
                    good_when_up: true,
                    sub: " since range start"
                )
                kpi(
                    label: "Net new MRR / month",
                    value: net_avg.map_or_else(
                        || "—".to_owned(),
                        |n| money(n.round() as i64, ccy),
                    ),
                    delta_value: rel(net_avg, net_prev),
                    pp: false,
                    good_when_up: true,
                    sub: " vs previous period"
                )
                kpi(
                    label: "Churn rate / month",
                    value: rate_text(churn_avg),
                    delta_value: churn_avg.zip(churn_prev).map(|(a, b)| a - b),
                    pp: true,
                    good_when_up: false,
                    sub: " vs previous period"
                )
            </div>
            <p class="caption">
                "Monthly averages over completed months; the current month is shaded. MRR movement by month:"
            </p>
            legend(items: movement_legend)
            <div class="chart" data-chart=(movement_chart)></div>
            data_table(labels: periods.clone(), columns: movement_columns)
        </section>
        <div class="grid">
            <section class="card card-elevated bg-base-100">
                <h2>"Revenue"</h2>
                legend(items: revenue_legend)
                <div class="chart" data-chart=(revenue_chart)></div>
                data_table(
                    labels: periods.clone(),
                    columns: vec![
                        ("Revenue", column(&|p| money(p.revenue_minor, ccy))),
                        ("Refunds", column(&|p| money(p.refunded_minor, ccy))),
                        ("Transactions", column(&|p| int(p.transactions as i64))),
                    ]
                )
            </section>
            <section class="card card-elevated bg-base-100">
                <h2>"Active subscriptions & trials"</h2>
                <h3>"Active subscriptions"</h3>
                <div class="chart" data-chart=(subs_chart)></div>
                <h3>"Active trials"</h3>
                <div class="chart" data-chart=(trials_chart)></div>
                data_table(
                    labels: periods.clone(),
                    columns: vec![
                        (
                            "Active subscriptions",
                            column(&|p| int(p.active_subscriptions as i64)),
                        ),
                        ("Active trials", column(&|p| int(p.active_trials as i64))),
                    ]
                )
            </section>
            <section class="card card-elevated bg-base-100">
                <h2>"Churn rate"</h2>
                <p class="caption">
                    "Churned subscriptions ÷ active at the start of the month."
                </p>
                <div class="chart" data-chart=(churn_chart)></div>
                data_table(
                    labels: periods.clone(),
                    columns: vec![
                        ("Churn rate", column(&|p| rate_text(p.churn_rate))),
                        ("Churned", column(&|p| int(p.churned_subscriptions as i64))),
                    ]
                )
            </section>
            <section class="card card-elevated bg-base-100">
                <h2>"Trial conversion"</h2>
                <p class="caption">"Trial conversions ÷ new trials in the month."</p>
                <div class="chart" data-chart=(conv_chart)></div>
                data_table(
                    labels: periods,
                    columns: vec![
                        ("Conversion", column(&|p| rate_text(conv(p)))),
                        ("New trials", column(&|p| int(p.new_trials as i64))),
                        ("Conversions", column(&|p| int(p.trial_conversions as i64))),
                    ]
                )
            </section>
        </div>
    })
}

// ---------------------------------------------------------------------------
// Activity

fn activity_href(project: &Project, env: Environment) -> String {
    format!(
        "{}&env={}",
        href("/dashboard/activity", project),
        env.as_str()
    )
}

/// The feed alone, for its periodic refresh.
#[page("/dashboard/activity")]
async fn activity(cx: &Cx) -> Result<impl View> {
    let projects = Projects::load(cx).await?;
    let env = Params::from_query(cx).env;
    Ok(view! {
        if let Some(project) = projects.current() {
            activity_feed(project: project, env: env)
        }
    })
}

#[component]
async fn activity_feed(cx: &Cx, project: &Project, env: Environment) -> Result<impl View> {
    let rows = recent_activity(
        &state(cx).db,
        ProjectId(project.id),
        Some(env),
        None,
        ACTIVITY_LIMIT,
    )
    .await;
    let env_name = env.as_str();
    Ok(view! {
        <section
            class="card card-elevated bg-base-100 activity"
            data-refresh=(activity_href(project, env))
            data-refresh-auto=""
        >
            <div class="card-head">
                <h2>"Purchase activity"</h2>
                <div class="tabs tabs-box tabs-sm" aria-label="Environment">
                    for (e, label) in [
                        (Environment::Sandbox, "Sandbox"),
                        (Environment::Production, "Production"),
                    ] {
                        <a
                            class="tab"
                            href=(format!(
                                "{}&env={}",
                                href("/dashboard/", project),
                                e.as_str(),
                            ))
                            aria-current=((e == env).then_some("page"))
                        >
                            (label)
                        </a>
                    }
                </div>
                <button
                    type="button"
                    class="btn btn-ghost btn-sm btn-square"
                    data-refresh-now=""
                    aria-label="Refresh"
                    title="Refresh"
                >
                    super::ui::glyph(kind: super::ui::Glyph::Refresh)
                </button>
            </div>
            <p class="caption">
                "Latest purchase state changes from app syncs and store notifications. Refreshes every 10 s."
            </p>
            match rows {
                Err(e) => {
                    <p class="notice alert alert-soft alert-error">
                        "Could not load activity: "
                        (e.to_string())
                    </p>
                }
                Ok(rows) => {
                    if rows.is_empty() {
                        <p class="notice alert">
                            "No "
                            (env_name)
                            " purchases recorded yet."
                            if env == Environment::Sandbox {
                                " Buy with a sandbox tester or TestFlight; purchases show up once the app syncs them or the store notifies entitle."
                            }
                        </p>
                    } else {
                        <div class="table-scroll">
                            <table>
                                <thead>
                                    <tr>
                                        <th>"Recorded"</th>
                                        <th>"Events"</th>
                                        <th>"Product"</th>
                                        <th>"User"</th>
                                        <th>"Store"</th>
                                        <th>"Status"</th>
                                        <th>"Expires"</th>
                                        <th class="num">"Price"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    for r in &rows {
                                        activity_row(project: project, r: r)
                                    }
                                </tbody>
                            </table>
                        </div>
                    }
                }
            }
        </section>
    })
}

#[component]
async fn activity_row(cx: &Cx, project: &Project, r: &ActivityOut) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td>local_time(at: r.recorded_at, format: "activity")</td>
            <td>
                if r.events.is_empty() {
                    <span class="muted">"state update"</span>
                } else {
                    (r.events.join(", ").replace('_', " "))
                }
            </td>
            <td><code>(&r.product_id)</code></td>
            <td>
                if let Some(user) = &r.app_user_id {
                    <a class="link" href=(customer_href(project, user))>(user)</a>
                } else {
                    <span class="muted">"unlinked"</span>
                }
            </td>
            <td>(store_name(&r.store))</td>
            <td>
                status_badge(status: &r.status)
                if r.period_type != "normal" {
                    <span class="muted">
                        " ("
                        (&r.period_type)
                        ")"
                    </span>
                }
            </td>
            <td>
                if let Some(expires) = r.expires_at {
                    local_time(at: expires, format: "activity")
                } else {
                    "—"
                }
            </td>
            <td class="num">
                (r
                    .price
                    .as_ref()
                    .map_or_else(
                        || "—".to_owned(),
                        |p| money(p.amount_minor, &p.currency),
                    ))
            </td>
        </tr>
    })
}
