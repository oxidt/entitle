//! Setup: everything `entitle admin` does, for one project. Reads and writes
//! go through the admin API in-process (`api::call`); every form posts to
//! `/dashboard/setup/action` and comes back here, a message in the flash.

use std::collections::HashMap;

use axum::http::Method;
use chrono::{DateTime, Utc};
use entitle_core::ProjectId;
use serde::Deserialize;
use serde_json::{Value, json};
use topcoat::{
    Result,
    context::Cx,
    router::{
        RouterBuilder,
        content::Form,
        error::{SeeOther, not_found, see_other},
        page, query_params,
        request::headers,
        route,
    },
    view::{View, component, view},
};

use super::api::{ApiFailure, call};
use super::flash::{self, Flash, Kind};
use super::shell::{Projects, Section, app, href, state};
use super::ui::{
    Glyph, copy_value, error_text, field, local_time, section, store_name, url_encode,
};
use crate::routes::admin::{Project, all_projects, recent_activity};

pub fn register(builder: RouterBuilder) -> RouterBuilder {
    builder
        .page(setup)
        .page(new_project)
        .route(create_project)
        .route(setup_action)
}

// ---------------------------------------------------------------------------
// Admin API shapes

#[derive(Deserialize)]
struct ApiKey {
    id: String,
    kind: String,
    key_prefix: String,
    created_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Product {
    id: String,
    store: String,
    store_product_id: String,
    display_name: Option<String>,
    billing_period: Option<String>,
}

#[derive(Deserialize)]
struct Entitlement {
    id: String,
    identifier: String,
    description: Option<String>,
    product_ids: Vec<String>,
}

#[derive(Deserialize)]
struct ConfiguredStore {
    store: String,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Webhook {
    id: String,
    url: String,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Delivery {
    id: String,
    event_type: String,
    status: String,
    attempts: i32,
    last_error: Option<String>,
    next_attempt_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    payload: Value,
}

#[derive(Deserialize)]
struct Created {
    id: String,
}

#[derive(Deserialize)]
struct NewKey {
    key: String,
}

#[derive(Deserialize)]
struct NewWebhook {
    secret: String,
}

#[derive(Deserialize)]
struct Check {
    ok: bool,
    message: String,
}

const STORES: [&str; 3] = ["app_store", "play_store", "creem"];

const PERIODS: [(&str, &str); 7] = [
    ("", "None (one-time, or infer)"),
    ("P1W", "Weekly"),
    ("P1M", "Monthly"),
    ("P2M", "Every 2 months"),
    ("P3M", "Quarterly"),
    ("P6M", "Every 6 months"),
    ("P1Y", "Yearly"),
];

fn webhook_path(store: &str) -> &'static str {
    match store {
        "app_store" => "apple",
        "play_store" => "google",
        _ => "creem",
    }
}

fn console_hint(store: &str) -> &'static str {
    match store {
        "app_store" => {
            "App Store Connect → App → App Information → App Store Server Notifications (Version 2), production and sandbox."
        }
        "play_store" => {
            "Play Console → Monetization setup → Real-time developer notifications topic; its Pub/Sub push subscription (with authentication) targets this URL."
        }
        _ => "Creem dashboard → Developers → Webhooks.",
    }
}

/// This server's origin as the browser reached it, for the notification URLs.
fn origin(cx: &Cx) -> String {
    let h = headers(cx);
    let get = |name: &str| h.get(name).and_then(|v| v.to_str().ok());
    let proto = get("x-forwarded-proto").unwrap_or("http");
    let host = get("x-forwarded-host")
        .or(get("host"))
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

fn project_path(project: &Project) -> String {
    format!("/projects/{}", project.id)
}

/// The flash for `target`, if the last action left one there.
fn flash_for<'a>(flash: &'a Option<Flash>, target: &str) -> Option<&'a Flash> {
    flash.as_ref().filter(|f| f.target == target)
}

fn flash_error<'a>(flash: &'a Option<Flash>, target: &str) -> Option<&'a str> {
    flash_for(flash, target)
        .filter(|f| f.kind == Kind::Error)
        .map(|f| f.text.as_str())
}

// ---------------------------------------------------------------------------
// Pages

#[query_params]
struct SetupQuery {
    /// Delivery filter: `pending` or `failed`.
    deliveries: Option<String>,
}

#[page("/dashboard/setup")]
async fn setup(cx: &Cx) -> Result<impl View> {
    let projects = Projects::load(cx).await?;
    let flash = flash::take(cx);
    let deliveries = query_params::<SetupQuery>(cx)
        .ok()
        .and_then(|q| q.deliveries.clone())
        .filter(|s| s == "pending" || s == "failed");
    Ok(view! {
        app(
            projects: &projects,
            section: Section::Setup,
            if let Some(project) = projects.current() {
                <div class="setup">
                    checklist(project: project)
                    project_settings(project: project, flash: &flash)
                    api_keys(project: project, flash: &flash)
                    catalog(project: project, flash: &flash)
                    stores(project: project, flash: &flash)
                    webhooks(
                        project: project,
                        flash: &flash,
                        status: deliveries.as_deref()
                    )
                    danger_zone(project: project, flash: &flash)
                </div>
            }
        )
    })
}

#[component]
async fn checklist(cx: &Cx, project: &Project) -> Result<impl View> {
    let state = state(cx);
    let base = project_path(project);
    let creds: Vec<ConfiguredStore> =
        call(state, Method::GET, &format!("{base}/credentials"), None).await?;
    let ents: Vec<Entitlement> =
        call(state, Method::GET, &format!("{base}/entitlements"), None).await?;
    let keys: Vec<ApiKey> = call(state, Method::GET, &format!("{base}/api-keys"), None).await?;
    let activity = recent_activity(&state.db, ProjectId(project.id), None, None, 1).await?;
    let steps = [
        ("Connect a store", !creds.is_empty(), Some("setup-stores")),
        (
            "Add a product and attach it to an entitlement",
            ents.iter().any(|e| !e.product_ids.is_empty()),
            Some("setup-catalog"),
        ),
        (
            "Create a public key for your app",
            keys.iter()
                .any(|k| k.kind == "public" && k.revoked_at.is_none()),
            Some("setup-keys"),
        ),
        (
            "Make a test purchase (sandbox or TestFlight)",
            !activity.is_empty(),
            None,
        ),
    ];
    let left = steps.iter().filter(|(_, done, _)| !done).count();
    let intro = format!("{left} of {} steps left.", steps.len());
    Ok(view! {
        if left > 0 {
            section(
                id: None,
                title: "Get started",
                intro: Some(&intro),
                danger: false,
                <ol class="checklist">
                    for (label, done, target) in steps {
                        <li class=(if done { "done" } else { "" })>
                            <span class="check" aria-hidden="true">
                                (if done { "✓" } else { "" })
                            </span>
                            match target.filter(|_| !done) {
                                Some(target) => {
                                    <a class="link" href=(format!("#{target}"))>(label)</a>
                                }
                                None => {
                                    <span>(label)</span>
                                }
                            }
                            if done {
                                <span class="sr-only">" (done)"</span>
                            }
                        </li>
                    }
                </ol>
            )
        }
    })
}

/// The hidden fields every action form carries.
#[component]
async fn action_fields(cx: &Cx, project: &Project, action: &str) -> Result<impl View> {
    Ok(view! {
        <input type="hidden" name="project" value=(project.id.to_string())>
        <input type="hidden" name="action" value=(action)>
    })
}

/// A destructive button that asks for a second click, then posts `action`.
/// With a `glyph` the first button shows only that icon (table rows).
#[component]
async fn confirm_button(
    cx: &Cx,
    project: &Project,
    label: &str,
    glyph: Option<Glyph>,
    question: &str,
    action: &str,
    fields: Vec<(&str, String)>,
) -> Result<impl View> {
    Ok(view! {
        <span data-confirm="">
            if let Some(g) = glyph {
                <button
                    type="button"
                    class="btn btn-ghost btn-xs btn-square"
                    data-confirm-ask=""
                    aria-label=(label)
                    title=(label)
                >
                    super::ui::glyph(kind: g)
                </button>
            } else {
                <button
                    type="button"
                    class="btn btn-outline btn-xs"
                    data-confirm-ask=""
                >
                    (label)
                </button>
            }
            <span class="confirm" hidden=(true)>
                <span>(question)</span>
                <form class="inline" method="post" action="/dashboard/setup/action">
                    action_fields(project: project, action: action)
                    for (name, value) in &fields {
                        <input type="hidden" name=(*name) value=(value)>
                    }
                    <button type="submit" class="btn btn-error btn-xs">(label)</button>
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
    })
}

#[component]
async fn one_time_secret(cx: &Cx, label: &str, value: &str) -> Result<impl View> {
    Ok(view! {
        <div
            class="notice alert alert-soft alert-warning secret"
            role="status"
            data-dismissable=""
        >
            <strong>(label)</strong>
            " — copy it now, it is not shown again."
            copy_value(value: value)
            <button type="button" class="btn btn-outline btn-xs" data-dismiss="">
                "Done"
            </button>
        </div>
    })
}

#[component]
async fn project_settings(cx: &Cx, project: &Project, flash: &Option<Flash>) -> Result<impl View> {
    let currencies = crate::fx::currencies(&state(cx).db).await?;
    let origins = project.checkout_redirect_origins.join("\n");
    let project_id = project.id.to_string();
    Ok(view! {
        <section class="card card-elevated bg-base-100 setup-card" id="setup-project">
            <h2>"Project"</h2>
            <div class="facts">
                <span class="field-label">"Name"</span>
                <span>(&project.name)</span>
                <span class="field-label">"Project id"</span>
                copy_value(value: &project_id)
            </div>
            <form
                class="stack"
                method="post"
                action="/dashboard/setup/action"
                data-dirty=""
            >
                action_fields(project: project, action: "update_project")
                field(
                    label: "Reporting currency",
                    hint: Some(
                        "Revenue is converted to this currency (ISO 4217, ECB rates).",
                    ),
                    <select
                        class="select select-sm w-24 uppercase"
                        name="currency"
                        required=(true)
                    >
                        if !currencies.contains(&project.reporting_currency) {
                            <option selected=(true)>
                                (&project.reporting_currency)
                            </option>
                        }
                        for c in &currencies {
                            <option selected=(&project.reporting_currency == c)>
                                (c)
                            </option>
                        }
                    </select>
                )
                field(
                    label: "Checkout redirect origins",
                    hint: Some(
                        "One per line, e.g. https://example.com. Creem checkouts may only return to these.",
                    ),
                    <textarea
                        class="textarea font-mono text-xs w-full"
                        rows="3"
                        name="origins"
                    >
                        (origins)
                    </textarea>
                )
                error_text(error: flash_error(flash, "project"))
                <div class="row">
                    <button type="submit" class="btn btn-primary btn-sm">"Save"</button>
                </div>
            </form>
        </section>
    })
}

#[component]
async fn api_keys(cx: &Cx, project: &Project, flash: &Option<Flash>) -> Result<impl View> {
    let keys: std::result::Result<Vec<ApiKey>, ApiFailure> = call(
        state(cx),
        Method::GET,
        &format!("{}/api-keys", project_path(project)),
        None,
    )
    .await;
    let (active, revoked): (Vec<ApiKey>, Vec<ApiKey>) = match keys {
        Ok(keys) => keys.into_iter().partition(|k| k.revoked_at.is_none()),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let secret = flash_for(flash, "keys").filter(|f| f.kind == Kind::Secret);
    Ok(view! {
        <section class="card card-elevated bg-base-100 setup-card" id="setup-keys">
            <h2>"API keys"</h2>
            <p class="caption">
                <code>"pk_"</code>
                " keys go into your app; "
                <code>"sk_"</code>
                " keys are for your backend only."
            </p>
            if let Some(secret) = secret {
                one_time_secret(label: "New API key", value: &secret.text)
            }
            error_text(error: flash_error(flash, "keys"))
            if !active.is_empty() {
                <table class="table table-sm table-polished">
                    <thead>
                        <tr>
                            <th>"Key"</th>
                            <th>"Kind"</th>
                            <th>"Created"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>
                        for k in &active {
                            <tr>
                                <td>
                                    <code>
                                        (&k.key_prefix)
                                        "…"
                                    </code>
                                </td>
                                <td>(&k.kind)</td>
                                <td>local_time(at: k.created_at, format: "date")</td>
                                <td class="actions">
                                    confirm_button(
                                        project: project,
                                        label: "Revoke",
                                        glyph: Some(Glyph::Ban),
                                        question: "Apps using it stop working.",
                                        action: "revoke_key",
                                        fields: vec![("key_id", k.id.clone())]
                                    )
                                </td>
                            </tr>
                        }
                    </tbody>
                </table>
            } else {
                <p class="hint">"No active keys."</p>
            }
            if !revoked.is_empty() {
                <p class="hint">
                    (revoked.len().to_string())
                    " revoked."
                </p>
            }
            <div class="row">
                <form class="inline" method="post" action="/dashboard/setup/action">
                    action_fields(project: project, action: "create_key")
                    <input type="hidden" name="kind" value="public">
                    <button type="submit" class="btn btn-primary btn-sm">
                        "New public key"
                    </button>
                </form>
                <form class="inline" method="post" action="/dashboard/setup/action">
                    action_fields(project: project, action: "create_key")
                    <input type="hidden" name="kind" value="secret">
                    <button type="submit" class="btn btn-outline btn-sm">
                        "New secret key"
                    </button>
                </form>
            </div>
        </section>
    })
}

#[component]
async fn catalog(cx: &Cx, project: &Project, flash: &Option<Flash>) -> Result<impl View> {
    let base = project_path(project);
    let products: Vec<Product> =
        call(state(cx), Method::GET, &format!("{base}/products"), None).await?;
    let ents: Vec<Entitlement> = call(
        state(cx),
        Method::GET,
        &format!("{base}/entitlements"),
        None,
    )
    .await?;
    Ok(view! {
        <section class="card card-elevated bg-base-100 setup-card" id="setup-catalog">
            <h2>"Products & entitlements"</h2>
            <p class="caption">
                "Your app checks entitlements (e.g. full_game); store products unlock them. A product can unlock several entitlements."
            </p>
            error_text(error: flash_error(flash, "catalog"))
            <h3>"Entitlements"</h3>
            if ents.is_empty() {
                <p class="hint">"None yet."</p>
            }
            for e in &ents {
                entitlement_row(project: project, ent: e, products: &products)
            }
            <form class="inline-form" method="post" action="/dashboard/setup/action">
                action_fields(project: project, action: "create_entitlement")
                field(
                    label: "Identifier",
                    hint: None,
                    <input
                        class="input input-sm"
                        name="identifier"
                        required=(true)
                        placeholder="full_game"
                    >
                )
                field(
                    label: "Description (optional)",
                    hint: None,
                    <input
                        class="input input-sm"
                        name="description"
                        placeholder="Full game unlock"
                    >
                )
                <button type="submit" class="btn btn-primary btn-sm">
                    "Add entitlement"
                </button>
            </form>
            <h3>"Products"</h3>
            if products.is_empty() {
                <p class="hint">"None yet."</p>
            } else {
                <div class="table-scroll">
                    <table class="table table-sm table-polished">
                        <thead>
                            <tr>
                                <th>"Store product id"</th>
                                <th>"Store"</th>
                                <th>"Name"</th>
                                <th>"Renews"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            for p in &products {
                                <tr>
                                    <td><code>(&p.store_product_id)</code></td>
                                    <td>(store_name(&p.store))</td>
                                    <td>
                                        if let Some(name) = &p.display_name {
                                            (name)
                                        } else {
                                            <span class="muted">"—"</span>
                                        }
                                    </td>
                                    <td>
                                        <form
                                            class="inline"
                                            method="post"
                                            action="/dashboard/setup/action"
                                        >
                                            action_fields(project: project, action: "product_period")
                                            <input type="hidden" name="product_id" value=(&p.id)>
                                            <select
                                                class="select select-sm"
                                                name="billing_period"
                                                aria-label=(format!(
                                                    "Billing period of {}",
                                                    p.store_product_id,
                                                ))
                                                data-autosubmit=""
                                            >
                                                for (value, label) in PERIODS {
                                                    <option
                                                        value=(value)
                                                        selected=(p.billing_period.as_deref().unwrap_or("")
                                                            == value)
                                                    >
                                                        (label)
                                                    </option>
                                                }
                                            </select>
                                        </form>
                                    </td>
                                    <td class="actions">
                                        confirm_button(
                                            project: project,
                                            label: "Delete",
                                            glyph: Some(Glyph::Trash),
                                            question: "Delete product?",
                                            action: "delete_product",
                                            fields: vec![("product_id", p.id.clone())]
                                        )
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
            <form class="inline-form" method="post" action="/dashboard/setup/action">
                action_fields(project: project, action: "create_product")
                field(
                    label: "Store",
                    hint: None,
                    <select class="select select-sm" name="store">
                        for s in STORES {
                            <option value=(s)>(store_name(s))</option>
                        }
                    </select>
                )
                field(
                    label: "Store product id",
                    hint: None,
                    <input
                        class="input input-sm"
                        name="store_product_id"
                        required=(true)
                        placeholder="com.example.game.full"
                    >
                )
                field(
                    label: "Name (optional)",
                    hint: None,
                    <input
                        class="input input-sm"
                        name="display_name"
                        placeholder="Full game"
                    >
                )
                field(
                    label: "Renews",
                    hint: None,
                    <select class="select select-sm" name="billing_period">
                        for (value, label) in PERIODS {
                            <option value=(value)>(label)</option>
                        }
                    </select>
                )
                <button type="submit" class="btn btn-primary btn-sm">
                    "Add product"
                </button>
            </form>
        </section>
    })
}

#[component]
async fn entitlement_row(
    cx: &Cx,
    project: &Project,
    ent: &Entitlement,
    products: &[Product],
) -> Result<impl View> {
    let attached: Vec<&Product> = products
        .iter()
        .filter(|p| ent.product_ids.contains(&p.id))
        .collect();
    let available: Vec<&Product> = products
        .iter()
        .filter(|p| !ent.product_ids.contains(&p.id))
        .collect();
    Ok(view! {
        <div class="entitlement">
            <div class="entitlement-head">
                <code class="strong">(&ent.identifier)</code>
                if let Some(d) = &ent.description {
                    <span class="muted">(d)</span>
                }
                <span class="spacer"></span>
                confirm_button(
                    project: project,
                    label: "Delete",
                    glyph: Some(Glyph::Trash),
                    question: "Delete entitlement?",
                    action: "delete_entitlement",
                    fields: vec![("entitlement_id", ent.id.clone())]
                )
            </div>
            <div class="chips">
                if attached.is_empty() {
                    <span class="hint">"Not unlocked by any product yet."</span>
                }
                for p in &attached {
                    <span class="chip">
                        <span class="muted">(store_name(&p.store))</span>
                        " "
                        (&p.store_product_id)
                        <form
                            class="inline"
                            method="post"
                            action="/dashboard/setup/action"
                        >
                            action_fields(project: project, action: "detach")
                            <input type="hidden" name="entitlement_id" value=(&ent.id)>
                            <input type="hidden" name="product_id" value=(&p.id)>
                            <button
                                type="submit"
                                aria-label=(format!("Detach {}", p.store_product_id))
                                title="Detach"
                            >
                                super::ui::glyph(kind: Glyph::Close)
                            </button>
                        </form>
                    </span>
                }
                if !available.is_empty() {
                    <span class="attach">
                        <form
                            class="inline"
                            method="post"
                            action="/dashboard/setup/action"
                        >
                            action_fields(project: project, action: "attach")
                            <input type="hidden" name="entitlement_id" value=(&ent.id)>
                            <select
                                class="select select-sm w-auto"
                                name="product_id"
                                required=(true)
                                aria-label=(format!(
                                    "Product to attach to {}",
                                    ent.identifier,
                                ))
                            >
                                <option value="">"Attach product…"</option>
                                for p in &available {
                                    <option value=(&p.id)>
                                        (store_name(&p.store))
                                        " · "
                                        (&p.store_product_id)
                                    </option>
                                }
                            </select>
                            <button type="submit" class="btn btn-outline btn-xs">
                                "Attach"
                            </button>
                        </form>
                    </span>
                }
            </div>
        </div>
    })
}

#[component]
async fn stores(cx: &Cx, project: &Project, flash: &Option<Flash>) -> Result<impl View> {
    let creds: std::result::Result<Vec<ConfiguredStore>, ApiFailure> = call(
        state(cx),
        Method::GET,
        &format!("{}/credentials", project_path(project)),
        None,
    )
    .await;
    let origin = origin(cx);
    let (creds, error) = match creds {
        Ok(c) => (c, None),
        Err(e) => (Vec::new(), Some(e.0)),
    };
    Ok(view! {
        <section class="card card-elevated bg-base-100 setup-card" id="setup-stores">
            <h2>"Stores"</h2>
            <p class="caption">
                "Credentials are stored encrypted and never shown again; replacing them overwrites all fields."
            </p>
            error_text(error: error.as_deref())
            for store in STORES {
                store_block(
                    project: project,
                    store: store,
                    url: format!(
                        "{origin}/webhooks/{}/{}",
                        webhook_path(store),
                        project.id,
                    ),
                    configured: creds.iter().find(|c| c.store == store),
                    flash: flash
                )
            }
        </section>
    })
}

#[component]
async fn store_block(
    cx: &Cx,
    project: &Project,
    store: &str,
    url: String,
    configured: Option<&ConfiguredStore>,
    flash: &Option<Flash>,
) -> Result<impl View> {
    let target = format!("store:{store}");
    let check = flash_for(flash, &target).filter(|f| matches!(f.kind, Kind::Good | Kind::Bad));
    let error = flash_error(flash, &target);
    Ok(view! {
        <div class="store" data-edit="">
            <div class="store-head">
                <h3>(store_name(store))</h3>
                if let Some(c) = configured {
                    <span class="badge badge-sm badge-soft badge-success">
                        "Configured "
                        local_time(at: c.updated_at, format: "date")
                    </span>
                } else {
                    <span class="badge badge-sm badge-soft">"Not configured"</span>
                }
                <span class="spacer"></span>
                <button
                    type="button"
                    class="btn btn-outline btn-xs"
                    data-edit-open=""
                    data-edit-idle=""
                >
                    (if configured.is_some() { "Replace" } else { "Set up" })
                </button>
                if configured.is_some() {
                    <form
                        class="inline"
                        method="post"
                        action="/dashboard/setup/action"
                        data-edit-idle=""
                    >
                        action_fields(project: project, action: "test_credentials")
                        <input type="hidden" name="store" value=(store)>
                        <button type="submit" class="btn btn-outline btn-xs">
                            "Test connection"
                        </button>
                    </form>
                    <span data-edit-idle="">
                        confirm_button(
                            project: project,
                            label: "Remove",
                            glyph: None,
                            question: "Remove credentials?",
                            action: "delete_credentials",
                            fields: vec![("store", store.to_owned())]
                        )
                    </span>
                }
            </div>
            <div class="store-url">
                <span class="field-label">"Notification URL"</span>
                copy_value(value: &url)
                <span class="hint">(console_hint(store))</span>
            </div>
            if let Some(check) = check {
                <p
                    class=(if check.kind == Kind::Good {
                        "notice alert alert-soft alert-success"
                    } else {
                        "notice alert alert-soft alert-error"
                    })
                    role="status"
                >
                    (&check.text)
                </p>
            }
            error_text(error: error)
            <div data-edit-form="" hidden=(true)>
                match store {
                    "app_store" => {
                        apple_form(project: project)
                    }
                    "play_store" => {
                        google_form(project: project, url: &url)
                    }
                    _ => {
                        creem_form(project: project)
                    }
                }
            </div>
        </div>
    })
}

#[component]
async fn form_buttons(cx: &Cx) -> Result<impl View> {
    Ok(view! {
        <div class="row">
            <button type="submit" class="btn btn-primary btn-sm">"Save"</button>
            <button type="button" class="btn btn-outline btn-sm" data-edit-cancel="">
                "Cancel"
            </button>
        </div>
    })
}

#[component]
async fn apple_form(cx: &Cx, project: &Project) -> Result<impl View> {
    Ok(view! {
        <form class="store-form" method="post" action="/dashboard/setup/action">
            action_fields(project: project, action: "set_credentials")
            <input type="hidden" name="store" value="app_store">
            <textarea
                class="textarea font-mono text-xs w-full"
                name="private_key"
                hidden=(true)
            ></textarea>
            <p class="hint">
                "App Store Connect → Users and Access → Integrations → In-App Purchase: generate a key and download the "
                <code>".p8"</code>
                " file."
            </p>
            field(
                label: "In-App Purchase key (.p8)",
                hint: Some("Read in the browser and sent only to your entitle server."),
                <input
                    class="file-input file-input-sm"
                    type="file"
                    accept=".p8"
                    required=(true)
                    data-read-into="private_key"
                    data-expect="p8"
                >
            )
            <p class="error" role="alert" data-file-error="" hidden=(true)>
                "That does not look like a .p8 private key."
            </p>
            field(
                label: "Key ID",
                hint: None,
                <input
                    class="input input-sm"
                    name="key_id"
                    required=(true)
                    placeholder="ABC123DEFG"
                >
            )
            field(
                label: "Issuer ID",
                hint: None,
                <input
                    class="input input-sm"
                    name="issuer_id"
                    required=(true)
                    placeholder="57246542-96fe-1a63-e053-0824d011072a"
                >
            )
            field(
                label: "Bundle ID",
                hint: None,
                <input
                    class="input input-sm"
                    name="bundle_id"
                    required=(true)
                    placeholder="com.example.app"
                >
            )
            field(
                label: "Apple ID (optional)",
                hint: Some(
                    "App Information → Apple ID. Required once the app is live.",
                ),
                <input
                    class="input input-sm"
                    name="app_apple_id"
                    inputmode="numeric"
                    pattern="[0-9]*"
                >
            )
            form_buttons()
        </form>
    })
}

#[component]
async fn google_form(cx: &Cx, project: &Project, url: &str) -> Result<impl View> {
    Ok(view! {
        <form class="store-form" method="post" action="/dashboard/setup/action">
            action_fields(project: project, action: "set_credentials")
            <input type="hidden" name="store" value="play_store">
            <textarea
                class="textarea font-mono text-xs w-full"
                name="service_account_json"
                hidden=(true)
            ></textarea>
            field(
                label: "Service account key (JSON)",
                hint: Some(
                    "A Google Cloud service account with access in Play Console → Users and permissions.",
                ),
                <input
                    class="file-input file-input-sm"
                    type="file"
                    accept=".json,application/json"
                    required=(true)
                    data-read-into="service_account_json"
                    data-expect="service_account"
                >
            )
            <p class="error" role="alert" data-file-error="" hidden=(true)>
                "That is not a service account JSON key."
            </p>
            field(
                label: "Package name",
                hint: None,
                <input
                    class="input input-sm"
                    name="package_name"
                    required=(true)
                    placeholder="com.example.app"
                >
            )
            field(
                label: "Pub/Sub push audience",
                hint: Some("As configured on the push subscription's authentication."),
                <input
                    class="input input-sm"
                    name="pubsub_audience"
                    required=(true)
                    value=(url)
                >
            )
            field(
                label: "Pub/Sub push service account",
                hint: None,
                <input
                    class="input input-sm"
                    name="pubsub_service_account_email"
                    required=(true)
                    type="email"
                    placeholder="pubsub-push@project.iam.gserviceaccount.com"
                >
            )
            form_buttons()
        </form>
    })
}

#[component]
async fn creem_form(cx: &Cx, project: &Project) -> Result<impl View> {
    Ok(view! {
        <form class="store-form" method="post" action="/dashboard/setup/action">
            action_fields(project: project, action: "set_credentials")
            <input type="hidden" name="store" value="creem">
            field(
                label: "API key",
                hint: None,
                <input
                    class="input input-sm"
                    name="api_key"
                    required=(true)
                    type="password"
                    autocomplete="off"
                >
            )
            field(
                label: "Webhook secret",
                hint: None,
                <input
                    class="input input-sm"
                    name="webhook_secret"
                    required=(true)
                    type="password"
                    autocomplete="off"
                >
            )
            <label class="switch">
                <input
                    class="toggle toggle-sm"
                    type="checkbox"
                    name="test_mode"
                    value="1"
                    checked=(true)
                >
                "Test mode"
            </label>
            form_buttons()
        </form>
    })
}

#[component]
async fn webhooks(
    cx: &Cx,
    project: &Project,
    flash: &Option<Flash>,
    status: Option<&str>,
) -> Result<impl View> {
    let hooks: Vec<Webhook> = call(
        state(cx),
        Method::GET,
        &format!("{}/webhooks", project_path(project)),
        None,
    )
    .await?;
    let secret = flash_for(flash, "webhooks").filter(|f| f.kind == Kind::Secret);
    Ok(view! {
        <section class="card card-elevated bg-base-100 setup-card" id="setup-webhooks">
            <h2>"Outgoing webhooks"</h2>
            <p class="caption">
                "entitle POSTs purchase and entitlement events to these URLs, signed with the "
                <code>"Entitle-Signature"</code>
                " header."
            </p>
            if let Some(secret) = secret {
                one_time_secret(label: "Signing secret", value: &secret.text)
            }
            error_text(error: flash_error(flash, "webhooks"))
            if !hooks.is_empty() {
                <table class="table table-sm table-polished">
                    <tbody>
                        for h in &hooks {
                            <tr>
                                <td class="wrap">(&h.url)</td>
                                <td>local_time(at: h.created_at, format: "date")</td>
                                <td class="actions">
                                    confirm_button(
                                        project: project,
                                        label: "Delete",
                                        glyph: Some(Glyph::Trash),
                                        question: "Stop sending events?",
                                        action: "delete_webhook",
                                        fields: vec![("webhook_id", h.id.clone())]
                                    )
                                </td>
                            </tr>
                        }
                    </tbody>
                </table>
            }
            <form class="row" method="post" action="/dashboard/setup/action">
                action_fields(project: project, action: "create_webhook")
                <input
                    class="input input-sm"
                    type="url"
                    name="url"
                    required=(true)
                    placeholder="https://api.example.com/entitle"
                    aria-label="Webhook URL"
                >
                <button type="submit" class="btn btn-primary btn-sm">
                    "Add endpoint"
                </button>
            </form>
            if !hooks.is_empty() {
                delivery_list(project: project, status: status, flash: flash)
            }
        </section>
    })
}

fn deliveries_href(project: &Project, status: Option<&str>) -> String {
    let mut url = href("/dashboard/setup", project);
    if let Some(s) = status {
        url.push_str(&format!("&deliveries={s}"));
    }
    url + "#setup-webhooks"
}

#[component]
async fn delivery_list(
    cx: &Cx,
    project: &Project,
    status: Option<&str>,
    flash: &Option<Flash>,
) -> Result<impl View> {
    let mut path = format!("{}/webhooks/deliveries", project_path(project));
    if let Some(s) = status {
        path.push_str(&format!("?status={s}"));
    }
    let list: std::result::Result<Vec<Delivery>, ApiFailure> =
        call(state(cx), Method::GET, &path, None).await;
    let (list, error) = match list {
        Ok(l) => (l, None),
        Err(e) => (Vec::new(), Some(e.0)),
    };
    Ok(view! {
        <div class="deliveries">
            <div class="card-head">
                <h3>"Recent deliveries"</h3>
                <div class="tabs tabs-box tabs-sm" aria-label="Delivery status">
                    for (s, label) in [
                        (None, "All"),
                        (Some("pending"), "Retrying"),
                        (Some("failed"), "Failed"),
                    ] {
                        <a
                            class="tab"
                            href=(deliveries_href(project, s))
                            aria-current=((s == status).then_some("page"))
                        >
                            (label)
                        </a>
                    }
                </div>
                <button
                    type="button"
                    class="btn btn-ghost btn-sm btn-square"
                    data-href=(deliveries_href(project, status))
                    aria-label="Refresh"
                    title="Refresh"
                >
                    super::ui::glyph(kind: Glyph::Refresh)
                </button>
            </div>
            error_text(error: error.as_deref().or(flash_error(flash, "deliveries")))
            if list.is_empty() && error.is_none() {
                <p class="hint">
                    "No deliveries"
                    if let Some(s) = status {
                        " with status "
                        (s)
                    }
                    "."
                </p>
            }
            if !list.is_empty() {
                <div class="table-scroll">
                    <table class="table table-sm table-polished">
                        <thead>
                            <tr>
                                <th>"Created"</th>
                                <th>"Event"</th>
                                <th>"Status"</th>
                                <th>"Last error"</th>
                                <th>"Payload"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            for d in &list {
                                delivery_row(project: project, d: d)
                            }
                        </tbody>
                    </table>
                </div>
            }
        </div>
    })
}

#[component]
async fn delivery_row(cx: &Cx, project: &Project, d: &Delivery) -> Result<impl View> {
    let tone = match d.status.as_str() {
        "delivered" => "badge badge-sm badge-soft badge-success",
        "failed" => "badge badge-sm badge-soft badge-error",
        _ => "badge badge-sm badge-soft",
    };
    let payload = serde_json::to_string_pretty(&d.payload).unwrap_or_default();
    Ok(view! {
        <tr>
            <td>local_time(at: d.created_at, format: "activity")</td>
            <td><code>(&d.event_type)</code></td>
            <td>
                <span class=(tone)>
                    (if d.status == "pending" { "retrying" } else { d.status.as_str() })
                </span>
                <span class="muted">
                    " "
                    (d.attempts.to_string())
                    "×"
                </span>
                if let Some(next) = d.next_attempt_at {
                    <div class="hint">
                        "next "
                        local_time(at: next, format: "activity")
                    </div>
                }
            </td>
            <td class="wrap">
                if let Some(e) = &d.last_error {
                    (e)
                } else {
                    <span class="muted">"—"</span>
                }
            </td>
            <td>
                <details>
                    <summary>"View"</summary>
                    <pre class="payload">(payload)</pre>
                </details>
            </td>
            <td class="actions">
                <form class="inline" method="post" action="/dashboard/setup/action">
                    action_fields(project: project, action: "retry_delivery")
                    <input type="hidden" name="delivery_id" value=(&d.id)>
                    <button type="submit" class="btn btn-outline btn-xs">
                        (if d.status == "delivered" { "Resend" } else { "Retry now" })
                    </button>
                </form>
            </td>
        </tr>
    })
}

#[component]
async fn danger_zone(cx: &Cx, project: &Project, flash: &Option<Flash>) -> Result<impl View> {
    Ok(view! {
        section(
            id: Some("setup-delete"),
            title: "Delete project",
            intro: Some(
                "Deletes the project with all its customers, purchases, keys and credentials. This cannot be undone.",
            ),
            danger: true,
            <div class="row">
                <input
                    class="input input-sm"
                    aria-label="Type the project name to confirm"
                    placeholder=(format!("Type “{}” to confirm", project.name))
                    data-reveal-when=(&project.name)
                    data-reveals="delete-project"
                >
                <span id="delete-project" hidden=(true)>
                    confirm_button(
                        project: project,
                        label: "Delete project",
                        glyph: None,
                        question: "Really delete?",
                        action: "delete_project",
                        fields: vec![("confirm_name", project.name.clone())]
                    )
                </span>
            </div>
            error_text(error: flash_error(flash, "delete"))
        )
    })
}

/// Creates a project; also shown when there are none yet.
#[component]
pub async fn new_project_form(
    cx: &Cx,
    cancel: Option<String>,
    error: Option<&str>,
) -> Result<impl View> {
    Ok(view! {
        <form
            class="card card-elevated bg-base-100 new-project"
            method="post"
            action="/dashboard/projects"
        >
            <h2>"New project"</h2>
            <p class="caption">
                "One project per app, or per group of apps that share entitlements."
            </p>
            <div class="row">
                <input
                    class="input input-sm"
                    autofocus=(true)
                    required=(true)
                    name="name"
                    placeholder="My App"
                    aria-label="Project name"
                >
                <button type="submit" class="btn btn-primary btn-sm">"Create"</button>
                if let Some(cancel) = cancel {
                    <button
                        type="button"
                        class="btn btn-outline btn-sm"
                        data-href=(cancel)
                    >
                        "Cancel"
                    </button>
                }
            </div>
            error_text(error: error)
        </form>
    })
}

#[page("/dashboard/projects/new")]
async fn new_project(cx: &Cx) -> Result<impl View> {
    let projects = Projects::load(cx).await?;
    let error = flash::take(cx)
        .filter(|f| f.target == "new_project")
        .map(|f| f.text);
    let cancel = projects.current().map(|p| href("/dashboard/setup", p));
    Ok(view! {
        app(
            projects: &projects,
            section: Section::Setup,
            new_project_form(cancel: cancel, error: error.as_deref())
        )
    })
}

#[derive(Deserialize)]
struct NewProjectForm {
    name: String,
}

#[route(POST "/dashboard/projects")]
async fn create_project(cx: &Cx, Form(form): Form<NewProjectForm>) -> Result<SeeOther> {
    let created: std::result::Result<Created, ApiFailure> = call(
        state(cx),
        Method::POST,
        "/projects",
        Some(json!({ "name": form.name.trim() })),
    )
    .await;
    Ok(match created {
        Ok(p) => see_other(format!("/dashboard/setup?project={}", url_encode(&p.id))),
        Err(e) => {
            flash::set(cx, "new_project", Kind::Error, e.0);
            see_other("/dashboard/projects/new")
        }
    })
}

// ---------------------------------------------------------------------------
// Actions

type Fields = HashMap<String, String>;

/// What an action leaves for the page: nothing, or a message of some kind.
type Outcome = std::result::Result<Option<(Kind, String)>, ApiFailure>;

fn get<'a>(f: &'a Fields, name: &str) -> &'a str {
    f.get(name).map_or("", |v| v.trim())
}

/// `None` for an empty field.
fn opt(f: &Fields, name: &str) -> Value {
    match get(f, name) {
        "" => Value::Null,
        v => Value::String(v.to_owned()),
    }
}

/// One form post: runs `action` against the admin API, then returns to the
/// section it belongs to with its outcome in the flash.
#[route(POST "/dashboard/setup/action")]
async fn setup_action(cx: &Cx, Form(f): Form<Fields>) -> Result<SeeOther> {
    let state = state(cx);
    let project = all_projects(&state.db)
        .await?
        .into_iter()
        .find(|p| p.id.to_string() == get(&f, "project"))
        .ok_or_else(not_found)?;
    let base = project_path(&project);
    let back = |anchor: &str| see_other(format!("{}#{anchor}", href("/dashboard/setup", &project)));
    let store = get(&f, "store");
    let store = STORES.into_iter().find(|s| *s == store).unwrap_or("creem");
    let store_target = format!("store:{store}");

    // (flash target, anchor to return to, result: an optional message to show)
    let (target, anchor, result): (&str, &str, Outcome) = match get(&f, "action") {
        "update_project" => {
            let origins: Vec<&str> = get(&f, "origins")
                .lines()
                .map(str::trim)
                .filter(|o| !o.is_empty())
                .collect();
            let body = json!({ "reporting_currency": get(&f, "currency"), "checkout_redirect_origins": origins });
            let r = call::<Value>(state, Method::PATCH, &base, Some(body)).await;
            ("project", "setup-project", r.map(|_| None))
        }
        "delete_project" => {
            if get(&f, "confirm_name") != project.name {
                let r = Err(ApiFailure("Type the project name to confirm.".into()));
                ("delete", "setup-delete", r)
            } else {
                return Ok(
                    match call::<Value>(state, Method::DELETE, &base, None).await {
                        Ok(_) => see_other("/dashboard/setup"),
                        Err(e) => {
                            flash::set(cx, "delete", Kind::Error, e.0);
                            back("setup-delete")
                        }
                    },
                );
            }
        }
        "create_key" => {
            let body = json!({ "kind": get(&f, "kind") });
            let r =
                call::<NewKey>(state, Method::POST, &format!("{base}/api-keys"), Some(body)).await;
            ("keys", "setup-keys", r.map(|k| Some((Kind::Secret, k.key))))
        }
        "revoke_key" => {
            let path = format!("{base}/api-keys/{}", url_encode(get(&f, "key_id")));
            (
                "keys",
                "setup-keys",
                call::<Value>(state, Method::DELETE, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "create_entitlement" => {
            let body = json!({ "identifier": get(&f, "identifier"), "description": opt(&f, "description") });
            let r = call::<Value>(
                state,
                Method::POST,
                &format!("{base}/entitlements"),
                Some(body),
            )
            .await;
            ("catalog", "setup-catalog", r.map(|_| None))
        }
        "delete_entitlement" => {
            let path = format!(
                "{base}/entitlements/{}",
                url_encode(get(&f, "entitlement_id"))
            );
            (
                "catalog",
                "setup-catalog",
                call::<Value>(state, Method::DELETE, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "attach" | "detach" => {
            let path = format!(
                "{base}/entitlements/{}/products/{}",
                url_encode(get(&f, "entitlement_id")),
                url_encode(get(&f, "product_id"))
            );
            let method = if get(&f, "action") == "attach" {
                Method::PUT
            } else {
                Method::DELETE
            };
            (
                "catalog",
                "setup-catalog",
                call::<Value>(state, method, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "create_product" => {
            let body = json!({
                "store": get(&f, "store"),
                "store_product_id": get(&f, "store_product_id"),
                "display_name": opt(&f, "display_name"),
                "billing_period": opt(&f, "billing_period"),
            });
            let r =
                call::<Value>(state, Method::POST, &format!("{base}/products"), Some(body)).await;
            ("catalog", "setup-catalog", r.map(|_| None))
        }
        "product_period" => {
            let path = format!("{base}/products/{}", url_encode(get(&f, "product_id")));
            let body = json!({ "billing_period": opt(&f, "billing_period") });
            (
                "catalog",
                "setup-catalog",
                call::<Value>(state, Method::PATCH, &path, Some(body))
                    .await
                    .map(|_| None),
            )
        }
        "delete_product" => {
            let path = format!("{base}/products/{}", url_encode(get(&f, "product_id")));
            (
                "catalog",
                "setup-catalog",
                call::<Value>(state, Method::DELETE, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "set_credentials" => {
            let result = match credentials_body(store, &f) {
                Ok(body) => {
                    let path = format!("{base}/credentials/{store}");
                    match call::<Value>(state, Method::PUT, &path, Some(body)).await {
                        // Saved: check them right away, as `Test connection` would.
                        Ok(_) => test_credentials(state, &base, store).await,
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            };
            (store_target.as_str(), "setup-stores", result)
        }
        "test_credentials" => (
            store_target.as_str(),
            "setup-stores",
            test_credentials(state, &base, store).await,
        ),
        "delete_credentials" => {
            let path = format!("{base}/credentials/{store}");
            (
                store_target.as_str(),
                "setup-stores",
                call::<Value>(state, Method::DELETE, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "create_webhook" => {
            let body = json!({ "url": get(&f, "url") });
            let r =
                call::<NewWebhook>(state, Method::POST, &format!("{base}/webhooks"), Some(body))
                    .await;
            (
                "webhooks",
                "setup-webhooks",
                r.map(|w| Some((Kind::Secret, w.secret))),
            )
        }
        "delete_webhook" => {
            let path = format!("{base}/webhooks/{}", url_encode(get(&f, "webhook_id")));
            (
                "webhooks",
                "setup-webhooks",
                call::<Value>(state, Method::DELETE, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        "retry_delivery" => {
            let path = format!(
                "{base}/webhooks/deliveries/{}/retry",
                url_encode(get(&f, "delivery_id"))
            );
            (
                "deliveries",
                "setup-webhooks",
                call::<Value>(state, Method::POST, &path, None)
                    .await
                    .map(|_| None),
            )
        }
        _ => return Err(not_found().into()),
    };
    match result {
        Ok(Some((kind, text))) => flash::set(cx, target, kind, text),
        Ok(None) => {}
        Err(e) => flash::set(cx, target, Kind::Error, e.0),
    }
    Ok(back(anchor))
}

async fn test_credentials(state: &crate::AppState, base: &str, store: &str) -> Outcome {
    let check: Check = call(
        state,
        Method::POST,
        &format!("{base}/credentials/{store}/test"),
        None,
    )
    .await?;
    Ok(Some(if check.ok {
        (Kind::Good, format!("✓ {}", check.message))
    } else {
        (Kind::Bad, format!("✗ {}", check.message))
    }))
}

/// The credentials JSON the admin API expects for `store`, from the form.
fn credentials_body(store: &str, f: &Fields) -> std::result::Result<Value, ApiFailure> {
    Ok(match store {
        "app_store" => {
            let apple_id = match get(f, "app_apple_id") {
                "" => Value::Null,
                id => Value::from(
                    id.parse::<u64>()
                        .map_err(|_| ApiFailure("Apple ID must be a number.".into()))?,
                ),
            };
            json!({
                "issuer_id": get(f, "issuer_id"),
                "key_id": get(f, "key_id"),
                "private_key": f.get("private_key").map_or("", String::as_str),
                "bundle_id": get(f, "bundle_id"),
                "app_apple_id": apple_id,
            })
        }
        "play_store" => {
            let account: Value = serde_json::from_str(get(f, "service_account_json"))
                .map_err(|_| ApiFailure("That is not a service account JSON key.".into()))?;
            json!({
                "service_account_json": account,
                "package_name": get(f, "package_name"),
                "pubsub_audience": get(f, "pubsub_audience"),
                "pubsub_service_account_email": get(f, "pubsub_service_account_email"),
            })
        }
        _ => json!({
            "api_key": get(f, "api_key"),
            "webhook_secret": get(f, "webhook_secret"),
            "test_mode": f.contains_key("test_mode"),
        }),
    })
}
