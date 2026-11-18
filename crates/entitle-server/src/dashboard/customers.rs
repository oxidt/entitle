//! Customers: the list, a lookup by app user id, and granting or revoking
//! promotional access.

use chrono::{Duration, Utc};
use entitle_core::{
    ActiveEntitlement, CustomerId, Environment, PeriodType, ProjectId, PurchaseKind, Store,
};
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    router::{
        RouterBuilder,
        content::Form,
        error::{SeeOther, not_found, see_other},
        page, query_params, route,
    },
    view::{View, component, view},
};

use super::format::money;
use super::shell::{Projects, Section, app, href, state};
use super::ui::{
    copy_value, error_text, field, local_time, section, status_badge, store_name, url_encode,
};
use crate::customers::CustomerInfo;
use crate::db::enum_str;
use crate::error::ApiError;
use crate::routes::admin::{
    CustomerListItem, CustomerStatus, Project, all_projects, customer_list, entitlements_of,
    recent_activity,
};
use crate::routes::sdk::{Grant, existing, grant_entitlement, info, revoke_entitlement};

pub fn register(builder: RouterBuilder) -> RouterBuilder {
    builder.page(customers).route(grant).route(revoke)
}

const PAGE: i64 = 50;
/// Most "Load more" pages shown at once.
const MAX_PAGES: i64 = 20;
const HISTORY_LIMIT: i64 = 50;

const STATUSES: [(Option<CustomerStatus>, &str, &str); 5] = [
    (None, "", "All"),
    (Some(CustomerStatus::Active), "active", "Active"),
    (Some(CustomerStatus::Trial), "trial", "Trial"),
    (Some(CustomerStatus::Churned), "churned", "Churned"),
    (Some(CustomerStatus::None), "none", "No purchases"),
];

const STORES: [(Store, &str); 3] = [
    (Store::Creem, "creem"),
    (Store::AppStore, "app_store"),
    (Store::PlayStore, "play_store"),
];

const DURATIONS: [(&str, &str); 4] = [
    ("Lifetime", ""),
    ("7 days", "7"),
    ("30 days", "30"),
    ("1 year", "365"),
];

/// The customer page for one app user id.
pub fn customer_href(project: &Project, app_user_id: &str) -> String {
    format!(
        "{}&q={}",
        href("/dashboard/customers", project),
        url_encode(app_user_id)
    )
}

#[query_params]
struct CustomersQuery {
    q: Option<String>,
    status: Option<String>,
    store: Option<String>,
    env: Option<String>,
    pages: Option<i64>,
    error: Option<String>,
}

/// The list's filters, as they appear in the URL.
#[derive(Clone, Copy)]
struct Filter {
    status: usize,
    store: Option<usize>,
    /// `None`: both environments.
    env: Option<Environment>,
    pages: i64,
}

impl Filter {
    fn from_query(q: Option<&CustomersQuery>) -> Self {
        let status = q
            .and_then(|q| q.status.as_deref())
            .and_then(|s| STATUSES.iter().position(|(_, v, _)| *v == s))
            .unwrap_or(0);
        let store = q
            .and_then(|q| q.store.as_deref())
            .and_then(|s| STORES.iter().position(|(_, v)| *v == s));
        let env = match q.and_then(|q| q.env.as_deref()) {
            Some("sandbox") => Some(Environment::Sandbox),
            Some("both") => None,
            _ => Some(Environment::Production),
        };
        let pages = q.and_then(|q| q.pages).unwrap_or(1).clamp(1, MAX_PAGES);
        Self {
            status,
            store,
            env,
            pages,
        }
    }

    /// Store and environment filters need a matching purchase, so they never match "none".
    fn purchase_filters(self) -> bool {
        STATUSES[self.status].0 != Some(CustomerStatus::None)
    }

    /// The list's query, as name/value pairs.
    fn params(self, project: &Project) -> Vec<(&'static str, String)> {
        let mut params = vec![("project", project.id.to_string())];
        let status = STATUSES[self.status].1;
        if !status.is_empty() {
            params.push(("status", status.to_owned()));
        }
        if let Some(store) = self.store {
            params.push(("store", STORES[store].1.to_owned()));
        }
        match self.env {
            Some(Environment::Production) => {}
            Some(Environment::Sandbox) => params.push(("env", "sandbox".to_owned())),
            None => params.push(("env", "both".to_owned())),
        }
        if self.pages > 1 {
            params.push(("pages", self.pages.to_string()));
        }
        params
    }

    fn href(self, project: &Project) -> String {
        let query: Vec<String> = self
            .params(project)
            .into_iter()
            .map(|(k, v)| format!("{k}={}", url_encode(&v)))
            .collect();
        format!("/dashboard/customers?{}", query.join("&"))
    }
}

#[page("/dashboard/customers")]
async fn customers(cx: &Cx) -> Result<impl View> {
    let projects = Projects::load(cx).await?;
    let q = query_params::<CustomersQuery>(cx).ok();
    let query = q
        .and_then(|q| q.q.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let filter = Filter::from_query(q);
    let error = q.and_then(|q| q.error.clone());
    Ok(view! {
        app(
            projects: &projects,
            section: Section::Customers,
            if let Some(project) = projects.current() {
                <div class="setup">
                    <form
                        class="card card-elevated bg-base-100 row search flex-row"
                        method="get"
                        action="/dashboard/customers"
                    >
                        <input
                            type="hidden"
                            name="project"
                            value=(project.id.to_string())
                        >
                        <input
                            class="input input-sm"
                            aria-label="App user id"
                            name="q"
                            required=(true)
                            placeholder="App user id, e.g. user_42 or $entitle_anon:…"
                            value=(query.as_deref().unwrap_or(""))
                        >
                        <button type="submit" class="btn btn-primary btn-sm">
                            "Look up"
                        </button>
                    </form>
                    if let Some(query) = &query {
                        customer_page(
                            project: project,
                            app_user_id: query,
                            error: error.as_deref()
                        )
                    } else {
                        customer_list_section(project: project, filter: filter)
                    }
                </div>
            }
        )
    })
}

#[component]
async fn customer_list_section(cx: &Cx, project: &Project, filter: Filter) -> Result<impl View> {
    let purchase_filters = filter.purchase_filters();
    let limit = PAGE * filter.pages;
    // One row more than shown tells whether "Load more" has anything to load.
    let rows = customer_list(
        &state(cx).db,
        ProjectId(project.id),
        STATUSES[filter.status].0,
        filter
            .store
            .filter(|_| purchase_filters)
            .map(|i| STORES[i].0),
        filter.env.filter(|_| purchase_filters),
        limit + 1,
        0,
    )
    .await
    .map_err(|e| e.to_string())
    .map(|mut rows| {
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        (rows, has_more)
    });
    let reset = Filter { pages: 1, ..filter };
    let next_page = Filter {
        pages: filter.pages + 1,
        ..filter
    }
    .params(project);
    Ok(view! {
        section(
            id: None,
            title: "All customers",
            intro: Some(
                "Most recently active first. Status counts store purchases only.",
            ),
            danger: false,
            <div class="toolbar">
                <div class="tabs tabs-box tabs-sm" aria-label="Status">
                    for (i, (_, _, label)) in STATUSES.iter().enumerate() {
                        <a
                            class="tab"
                            href=(Filter { status: i, ..reset }.href(project))
                            aria-current=((i == filter.status).then_some("page"))
                        >
                            (*label)
                        </a>
                    }
                </div>
                <select
                    class="select select-sm w-auto"
                    aria-label="Store"
                    disabled=(!purchase_filters)
                    data-navigate=""
                >
                    <option
                        value=(Filter { store: None, ..reset }.href(project))
                        selected=(filter.store.is_none())
                    >
                        "All stores"
                    </option>
                    for (i, (_, slug)) in STORES.iter().enumerate() {
                        <option
                            value=(Filter { store: Some(i), ..reset }.href(project))
                            selected=(filter.store == Some(i))
                        >
                            (store_name(slug))
                        </option>
                    }
                </select>
                <select
                    class="select select-sm w-auto"
                    aria-label="Environment"
                    disabled=(!purchase_filters)
                    data-navigate=""
                >
                    for (env, label) in [
                        (Some(Environment::Production), "Production"),
                        (Some(Environment::Sandbox), "Sandbox"),
                        (None, "Both environments"),
                    ] {
                        <option
                            value=(Filter { env, ..reset }.href(project))
                            selected=(filter.env == env)
                        >
                            (label)
                        </option>
                    }
                </select>
            </div>
            match rows {
                Err(e) => {
                    error_text(error: Some(e.as_str()))
                }
                Ok((rows, has_more)) => {
                    if rows.is_empty() {
                        <p class="hint">"No customers match."</p>
                    } else {
                        <div class="table-scroll">
                            <table class="table table-sm table-polished">
                                <thead>
                                    <tr>
                                        <th>"User"</th>
                                        <th>"Status"</th>
                                        <th>"Stores"</th>
                                        <th>"Last activity"</th>
                                        <th>"First seen"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    for c in &rows {
                                        customer_row(project: project, c: c)
                                    }
                                </tbody>
                            </table>
                        </div>
                    }
                    if has_more && filter.pages < MAX_PAGES {
                        <form method="get" action="/dashboard/customers">
                            for (name, value) in next_page {
                                <input type="hidden" name=(name) value=(value)>
                            }
                            <button type="submit" class="btn btn-outline btn-sm">
                                "Load more"
                            </button>
                        </form>
                    }
                }
            }
        )
    })
}

#[component]
async fn customer_row(project: &Project, c: &CustomerListItem) -> Result<impl View> {
    let stores = c
        .stores
        .iter()
        .map(|s| store_name(s))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(view! {
        <tr>
            <td>
                if let Some(user) = &c.app_user_id {
                    <a class="link" href=(customer_href(project, user))>(user)</a>
                } else {
                    <span class="muted">"no app user id"</span>
                }
            </td>
            <td>status_badge(status: &c.status)</td>
            <td>
                if stores.is_empty() {
                    <span class="muted">"—"</span>
                } else {
                    (stores)
                }
            </td>
            <td>local_time(at: c.last_activity_at, format: "customer")</td>
            <td>local_time(at: c.first_seen_at, format: "customer")</td>
        </tr>
    })
}

/// One customer, or the grant form for an app user id not seen yet.
#[component]
async fn customer_page(
    cx: &Cx,
    project: &Project,
    app_user_id: &str,
    error: Option<&str>,
) -> Result<impl View> {
    let state = state(cx);
    let project_id = ProjectId(project.id);
    let found = match existing(state, project_id, app_user_id).await {
        Ok(id) => Some(
            info(state, project_id, id, app_user_id)
                .await
                .map(|json| (id, json.0)),
        ),
        Err(ApiError::NotFound(_)) => None,
        Err(e) => Some(Err(e)),
    };
    Ok(view! {
        match found {
            None => {
                <p class="notice alert">
                    "No customer with app user id “"
                    (app_user_id)
                    "” in "
                    (&project.name)
                    "."
                </p>
                grant_form(project: project, app_user_id: app_user_id, error: error)
            }
            Some(Err(e)) => {
                <p class="notice alert alert-soft alert-error">(e.to_string())</p>
            }
            Some(Ok((id, info))) => {
                customer_view(project: project, customer: id, info: &info, error: error)
            }
        }
    })
}

fn source(s: &str) -> &str {
    if s == "promotional" {
        "Promotional"
    } else {
        store_name(s)
    }
}

#[component]
async fn customer_view(
    cx: &Cx,
    project: &Project,
    customer: CustomerId,
    info: &CustomerInfo,
    error: Option<&str>,
) -> Result<impl View> {
    let ents: Vec<&ActiveEntitlement> = info.entitlements.values().collect();
    let history = recent_activity(
        &state(cx).db,
        ProjectId(project.id),
        None,
        Some(customer.0),
        HISTORY_LIMIT,
    )
    .await;
    let aliases = info.aliases.as_deref().unwrap_or_default().join(", ");
    let customer_id = info.customer_id.to_string();
    let history = history.map_err(|e| e.to_string());
    Ok(view! {
        section(
            id: None,
            title: &info.app_user_id,
            intro: None,
            danger: false,
            <div class="facts">
                <span class="field-label">"Customer id"</span>
                copy_value(value: &customer_id)
                <span class="field-label">"Aliases"</span>
                <span>(aliases)</span>
                <span class="field-label">"First seen"</span>
                <span>local_time(at: info.first_seen_at, format: "customer")</span>
                if let Some(url) = &info.management_url {
                    <span class="field-label">"Manage subscription"</span>
                    <a href=(url) target="_blank" rel="noreferrer">(url)</a>
                }
            </div>
        )
        section(
            id: None,
            title: "Active entitlements",
            intro: None,
            danger: false,
            error_text(error: error)
            if ents.is_empty() {
                <p class="hint">"No active entitlements."</p>
            } else {
                <div class="table-scroll">
                    <table class="table table-sm table-polished">
                        <thead>
                            <tr>
                                <th>"Entitlement"</th>
                                <th>"Source"</th>
                                <th>"Product"</th>
                                <th>"Expires"</th>
                                <th>"Renews"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            for e in &ents {
                                entitlement_row(
                                    project: project,
                                    app_user_id: &info.app_user_id,
                                    e: e
                                )
                            }
                        </tbody>
                    </table>
                </div>
            }
            grant_form(project: project, app_user_id: &info.app_user_id, error: None)
        )
        section(
            id: None,
            title: "Purchases",
            intro: None,
            danger: false,
            if info.purchases.is_empty() {
                <p class="hint">"No purchases."</p>
            } else {
                <div class="table-scroll">
                    <table class="table table-sm table-polished">
                        <thead>
                            <tr>
                                <th>"Product"</th>
                                <th>"Store"</th>
                                <th>"Status"</th>
                                <th>"Purchased"</th>
                                <th>"Expires"</th>
                                <th>"Price"</th>
                                <th>"Original transaction"</th>
                            </tr>
                        </thead>
                        <tbody>
                            for p in &info.purchases {
                                <tr>
                                    <td><code>(&p.product_id)</code></td>
                                    <td>
                                        (store_name(p.store.as_str()))
                                        if p.environment == Environment::Sandbox {
                                            <span class="badge badge-sm badge-soft">"sandbox"</span>
                                        }
                                    </td>
                                    <td>
                                        status_badge(status: p.status.as_str())
                                        if p.period_type != PeriodType::Normal {
                                            <span class="muted">
                                                " ("
                                                (enum_str(&p.period_type))
                                                ")"
                                            </span>
                                        }
                                        if p.kind == PurchaseKind::Subscription && !p.auto_renew {
                                            <span class="muted">" · won't renew"</span>
                                        }
                                    </td>
                                    <td>local_time(at: p.purchased_at, format: "customer")</td>
                                    <td>
                                        if let Some(expires) = p.expires_at {
                                            local_time(at: expires, format: "customer")
                                        } else {
                                            "—"
                                        }
                                    </td>
                                    <td>
                                        (p
                                            .price
                                            .as_ref()
                                            .map_or_else(
                                                || "—".to_owned(),
                                                |m| money(m.amount_minor, &m.currency),
                                            ))
                                    </td>
                                    <td>
                                        <code class="muted">
                                            (p.original_transaction_id.as_deref().unwrap_or_default())
                                        </code>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
        )
        section(
            id: None,
            title: "History",
            intro: Some(
                "Purchase state changes recorded for this customer, newest first.",
            ),
            danger: false,
            match history {
                Err(e) => {
                    error_text(error: Some(e.as_str()))
                }
                Ok(rows) => {
                    if rows.is_empty() {
                        <p class="hint">"Nothing recorded."</p>
                    } else {
                        <div class="table-scroll">
                            <table class="table table-sm table-polished">
                                <thead>
                                    <tr>
                                        <th>"Recorded"</th>
                                        <th>"Events"</th>
                                        <th>"Product"</th>
                                        <th>"Status"</th>
                                        <th>"Expires"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    for r in &rows {
                                        <tr>
                                            <td>
                                                local_time(at: r.recorded_at, format: "customer")
                                                if r.environment == "sandbox" {
                                                    <span class="badge badge-sm badge-soft">"sandbox"</span>
                                                }
                                            </td>
                                            <td>
                                                if r.events.is_empty() {
                                                    <span class="muted">"state update"</span>
                                                } else {
                                                    (r.events.join(", ").replace('_', " "))
                                                }
                                            </td>
                                            <td><code>(&r.product_id)</code></td>
                                            <td>status_badge(status: &r.status)</td>
                                            <td>
                                                if let Some(expires) = r.expires_at {
                                                    local_time(at: expires, format: "customer")
                                                } else {
                                                    "—"
                                                }
                                            </td>
                                        </tr>
                                    }
                                </tbody>
                            </table>
                        </div>
                    }
                }
            }
        )
    })
}

#[component]
async fn entitlement_row(
    project: &Project,
    app_user_id: &str,
    e: &ActiveEntitlement,
) -> Result<impl View> {
    let source_name = enum_str(&e.source);
    Ok(view! {
        <tr>
            <td><code class="strong">(&e.identifier)</code></td>
            <td>
                (source(&source_name))
                if e.environment == Some(Environment::Sandbox) {
                    <span class="badge badge-sm badge-soft">"sandbox"</span>
                }
                if e.billing_issue {
                    <span class="badge badge-sm badge-soft badge-error">
                        "billing issue"
                    </span>
                }
            </td>
            <td>
                if let Some(product) = &e.product_id {
                    <code>(product)</code>
                } else {
                    <span class="muted">"—"</span>
                }
            </td>
            <td>
                if let Some(expires) = e.expires_at {
                    local_time(at: expires, format: "customer")
                } else {
                    "Never"
                }
            </td>
            <td>(if e.will_renew { "Yes" } else { "No" })</td>
            <td class="actions">
                if source_name == "promotional" {
                    <span data-confirm="">
                        <button
                            type="button"
                            class="btn btn-ghost btn-xs btn-square"
                            data-confirm-ask=""
                            aria-label="Revoke"
                            title="Revoke"
                        >
                            super::ui::glyph(kind: super::ui::Glyph::Ban)
                        </button>
                        <span class="confirm" hidden=(true)>
                            <span>"Revoke promotional access?"</span>
                            <form
                                class="inline"
                                method="post"
                                action="/dashboard/customers/revoke"
                            >
                                <input
                                    type="hidden"
                                    name="project"
                                    value=(project.id.to_string())
                                >
                                <input
                                    type="hidden"
                                    name="app_user_id"
                                    value=(app_user_id)
                                >
                                <input
                                    type="hidden"
                                    name="identifier"
                                    value=(&e.identifier)
                                >
                                <button type="submit" class="btn btn-error btn-xs">
                                    "Revoke"
                                </button>
                            </form>
                            <button
                                type="button"
                                class="btn btn-outline btn-xs"
                                data-confirm-cancel=""
                            >
                                "Cancel"
                            </button>
                        </span>
                    </span>
                }
            </td>
        </tr>
    })
}

#[component]
async fn grant_form(
    cx: &Cx,
    project: &Project,
    app_user_id: &str,
    error: Option<&str>,
) -> Result<impl View> {
    let ents = entitlements_of(&state(cx).db, ProjectId(project.id)).await?;
    Ok(view! {
        if ents.is_empty() {
            <p class="hint">"Create an entitlement in Setup to grant access."</p>
        } else {
            <form class="inline-form" method="post" action="/dashboard/customers/grant">
                <input type="hidden" name="project" value=(project.id.to_string())>
                <input type="hidden" name="app_user_id" value=(app_user_id)>
                field(
                    label: "Grant entitlement",
                    hint: None,
                    <select class="select select-sm" name="identifier" required=(true)>
                        <option value="">"Choose…"</option>
                        for e in &ents {
                            <option value=(&e.identifier)>(&e.identifier)</option>
                        }
                    </select>
                )
                field(
                    label: "For",
                    hint: None,
                    <select class="select select-sm" name="days">
                        for (label, days) in DURATIONS {
                            <option value=(days)>(label)</option>
                        }
                    </select>
                )
                <button type="submit" class="btn btn-primary btn-sm">"Grant"</button>
                error_text(error: error)
            </form>
        }
    })
}

#[derive(Deserialize)]
struct GrantForm {
    project: String,
    app_user_id: String,
    identifier: String,
    days: Option<i64>,
}

#[derive(Deserialize)]
struct RevokeForm {
    project: String,
    app_user_id: String,
    identifier: String,
}

/// The project a form names, if it exists.
async fn form_project(cx: &Cx, id: &str) -> Result<Project> {
    all_projects(&state(cx).db)
        .await?
        .into_iter()
        .find(|p| p.id.to_string() == id)
        .ok_or_else(|| not_found().into())
}

/// Back to the customer, with the error to show, if any.
fn back(project: &Project, app_user_id: &str, result: Result<(), ApiError>) -> SeeOther {
    let mut url = customer_href(project, app_user_id);
    if let Err(e) = result {
        url.push_str(&format!("&error={}", url_encode(&e.to_string())));
    }
    see_other(url)
}

#[route(POST "/dashboard/customers/grant")]
async fn grant(cx: &Cx, Form(form): Form<GrantForm>) -> Result<SeeOther> {
    let project = form_project(cx, &form.project).await?;
    let expires_at = form
        .days
        .filter(|d| (1..=36_500).contains(d))
        .map(|d| Utc::now() + Duration::days(d));
    let result = grant_entitlement(
        state(cx),
        ProjectId(project.id),
        &form.app_user_id,
        &form.identifier,
        Grant { expires_at },
    )
    .await;
    Ok(back(&project, &form.app_user_id, result.map(|_| ())))
}

#[route(POST "/dashboard/customers/revoke")]
async fn revoke(cx: &Cx, Form(form): Form<RevokeForm>) -> Result<SeeOther> {
    let project = form_project(cx, &form.project).await?;
    let result = revoke_entitlement(
        state(cx),
        ProjectId(project.id),
        &form.app_user_id,
        &form.identifier,
    )
    .await;
    Ok(back(&project, &form.app_user_id, result.map(|_| ())))
}
