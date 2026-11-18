//! The MCP server (rmcp, Streamable HTTP). Every tool is a call to the admin API
//! in-process, so tools share its validation and errors; results are its JSON. Write
//! tools need the `mcp:write` scope.

use std::sync::Arc;

use axum::Router;
use axum::http::Method;
use axum::http::request::Parts;
use reqwest::Url;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Access;
use crate::AppState;
use crate::dashboard::api;

pub fn router(state: AppState, public_url: &str) -> Router<AppState> {
    // rmcp checks the Host header against this list (DNS rebinding protection).
    let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"].map(Into::into).into();
    if let Some(host) = Url::parse(public_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
    {
        hosts.push(host);
    }
    let service = StreamableHttpService::new(
        move || Ok(Tools::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        // Stateless JSON: no sessions or open streams to keep alive or cut at shutdown.
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(hosts)
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().route_service("/mcp", service)
}

#[derive(Clone)]
struct Tools {
    state: AppState,
}

#[derive(Deserialize, JsonSchema)]
struct ProjectInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct MetricsInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// First day, `YYYY-MM-DD`.
    from: String,
    /// Last day (inclusive), `YYYY-MM-DD`.
    to: String,
    /// Bucket size: `day`, `week` (starting Monday) or `month`.
    interval: String,
}

#[derive(Deserialize, JsonSchema)]
struct CustomersInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// Only customers in this state: `active` (paid access now), `trial`, `churned`
    /// (purchases, none granting access) or `none` (no purchases).
    status: Option<String>,
    /// Only customers with a purchase in this store: `creem`, `app_store` or `play_store`.
    store: Option<String>,
    /// Only customers with a purchase in this environment: `production` or `sandbox`.
    environment: Option<String>,
    /// 1 to 200, default 50.
    limit: Option<i64>,
    /// For paging, default 0.
    offset: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct CustomerInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// The customer's app user id (any of their aliases).
    app_user_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct ActivityInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// `production` or `sandbox`; both when absent.
    environment: Option<String>,
    /// Only this customer's purchases.
    app_user_id: Option<String>,
    /// 1 to 200, default 50.
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct GrantInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// The customer's app user id (any of their aliases). Created if unknown.
    app_user_id: String,
    /// Entitlement identifier from `list_entitlements`, e.g. `pro`.
    identifier: String,
    /// When the grant ends, RFC 3339 (e.g. `2026-12-31T00:00:00Z`). Omit for lifetime.
    expires_at: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct RevokeInput {
    /// Project id (UUID) from `list_projects`.
    project_id: String,
    /// The customer's app user id (any of their aliases).
    app_user_id: String,
    /// Entitlement identifier from `list_entitlements`, e.g. `pro`.
    identifier: String,
}

/// `/projects/{project_id}/{rest…}?{query}`, with each part percent-encoded.
fn project_path(
    project_id: &str,
    rest: &[&str],
    query: &[(&str, Option<String>)],
) -> Result<String, ErrorData> {
    let project_id = uuid::Uuid::parse_str(project_id)
        .map_err(|_| ErrorData::invalid_params("project_id must be a UUID", None))?;
    let mut path = format!("/projects/{project_id}");
    for segment in rest {
        path.push('/');
        path.push_str(&encode(segment));
    }
    let pairs: Vec<String> = query
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={}", encode(v))))
        .collect();
    if !pairs.is_empty() {
        path.push('?');
        path.push_str(&pairs.join("&"));
    }
    Ok(path)
}

/// Percent-encodes all but `[A-Za-z0-9_~-]`. `.` too, so `.` and `..` stay ids, not
/// dot segments.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Tools {
    fn new(state: AppState) -> Self {
        Tools { state }
    }

    async fn get(&self, path: &str) -> Result<CallToolResult, ErrorData> {
        self.call(Method::GET, path, None).await
    }

    /// The admin API's JSON as the tool result; its errors as tool errors the model sees.
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<CallToolResult, ErrorData> {
        Ok(
            match api::call::<Value>(&self.state, method, path, body).await {
                Ok(json) => CallToolResult::success(vec![ContentBlock::text(json.to_string())]),
                Err(e) => CallToolResult::error(vec![ContentBlock::text(e.0)]),
            },
        )
    }
}

/// Write tools refuse tokens granted without `mcp:write`.
fn require_write(parts: &Parts) -> Result<(), ErrorData> {
    match parts.extensions.get::<Access>() {
        Some(Access { write: true }) => Ok(()),
        _ => Err(ErrorData::invalid_request(
            "This connection is read-only. Reconnect to entitle and check \"Allow changes\" \
             on the consent page to grant or revoke entitlements.",
            None,
        )),
    }
}

#[tool_router]
impl Tools {
    #[tool(
        description = "List every project: id, name, reporting_currency and settings. Start here; the other tools take a project_id.",
        annotations(read_only_hint = true)
    )]
    async fn list_projects(&self) -> Result<CallToolResult, ErrorData> {
        self.get("/projects").await
    }

    #[tool(
        description = "List a project's products: id, store, store_product_id, display_name and billing_period.",
        annotations(read_only_hint = true)
    )]
    async fn list_products(
        &self,
        Parameters(p): Parameters<ProjectInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.get(&project_path(&p.project_id, &["products"], &[])?)
            .await
    }

    #[tool(
        description = "List a project's entitlements (access levels such as `pro`) with the ids of the products that unlock each.",
        annotations(read_only_hint = true)
    )]
    async fn list_entitlements(
        &self,
        Parameters(p): Parameters<ProjectInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.get(&project_path(&p.project_id, &["entitlements"], &[])?)
            .await
    }

    #[tool(
        description = "Revenue metrics per day, week or month in the project's reporting currency: MRR and its movements (new, expansion, contraction, churn), revenue, refunds, active subscriptions and trials, new customers, trial conversions and churn rate. Amounts are in minor units (cents); `unconverted` lists amounts without an exchange rate.",
        annotations(read_only_hint = true)
    )]
    async fn get_metrics(
        &self,
        Parameters(p): Parameters<MetricsInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = [
            ("from", Some(p.from)),
            ("to", Some(p.to)),
            ("interval", Some(p.interval)),
        ];
        self.get(&project_path(&p.project_id, &["metrics"], &query)?)
            .await
    }

    #[tool(
        description = "List a project's customers, most recently active first, with their status (active, trial, churned, none) and stores.",
        annotations(read_only_hint = true)
    )]
    async fn list_customers(
        &self,
        Parameters(p): Parameters<CustomersInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = [
            ("status", p.status),
            ("store", p.store),
            ("environment", p.environment),
            ("limit", p.limit.map(|l| l.to_string())),
            ("offset", p.offset.map(|o| o.to_string())),
        ];
        self.get(&project_path(&p.project_id, &["customers"], &query)?)
            .await
    }

    #[tool(
        description = "Look up one customer: active entitlements with expiry, purchases and subscriptions across stores, and aliases.",
        annotations(read_only_hint = true)
    )]
    async fn get_customer(
        &self,
        Parameters(p): Parameters<CustomerInput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.get(&project_path(
            &p.project_id,
            &["customers", &p.app_user_id],
            &[],
        )?)
        .await
    }

    #[tool(
        description = "Newest purchase events (purchases, renewals, cancellations, refunds, expirations) recorded from the stores, optionally for one customer or environment.",
        annotations(read_only_hint = true)
    )]
    async fn list_activity(
        &self,
        Parameters(p): Parameters<ActivityInput>,
    ) -> Result<CallToolResult, ErrorData> {
        let query = [
            ("environment", p.environment),
            ("app_user_id", p.app_user_id),
            ("limit", p.limit.map(|l| l.to_string())),
        ];
        self.get(&project_path(&p.project_id, &["activity"], &query)?)
            .await
    }

    #[tool(
        description = "Grant an entitlement to a customer by hand (promotional access), for life or until expires_at (must be in the future). Creates the customer if unknown. Returns the customer's updated state. Needs write access.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn grant_entitlement(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GrantInput>,
    ) -> Result<CallToolResult, ErrorData> {
        require_write(&parts)?;
        let path = project_path(
            &p.project_id,
            &[
                "customers",
                &p.app_user_id,
                "entitlements",
                &p.identifier,
                "grant",
            ],
            &[],
        )?;
        self.call(
            Method::POST,
            &path,
            Some(json!({"expires_at": p.expires_at})),
        )
        .await
    }

    #[tool(
        description = "Revoke all manual (promotional) grants of an entitlement from a customer. Store purchases are not affected. Returns the customer's updated state. Needs write access.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn revoke_entitlement(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RevokeInput>,
    ) -> Result<CallToolResult, ErrorData> {
        require_write(&parts)?;
        let path = project_path(
            &p.project_id,
            &[
                "customers",
                &p.app_user_id,
                "entitlements",
                &p.identifier,
                "revoke",
            ],
            &[],
        )?;
        self.call(Method::POST, &path, None).await
    }
}

#[tool_handler]
impl ServerHandler for Tools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("entitle", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Access to entitle, a subscription and entitlement backend for apps: \
                 projects, products, entitlements, customers, purchase activity and revenue \
                 metrics. Call list_projects first; the other tools take its ids. \
                 grant_entitlement and revoke_entitlement need write access.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::project_path;

    #[test]
    fn dot_ids_stay_path_segments() {
        let id = "00000000-0000-0000-0000-000000000001";
        assert_eq!(
            project_path(id, &["customers", ".."], &[("q", Some("a b&c".into()))]).unwrap(),
            format!("/projects/{id}/customers/%2E%2E?q=a%20b%26c")
        );
    }
}
