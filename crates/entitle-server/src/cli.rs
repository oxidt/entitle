//! Command line. `entitle` (or `entitle serve`) runs the server; `entitle admin …` is a
//! client for the admin API of a running server, local or remote.

use std::path::PathBuf;

use anyhow::{Context, bail};
use chrono::{Duration, Utc};
use clap::{Args, Parser, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const STORES: [&str; 3] = ["app_store", "play_store", "creem"];

#[derive(Parser)]
#[command(
    name = "entitle",
    version,
    about = "Self-hosted in-app purchase and entitlement server"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the server (the default without a command).
    Serve,
    /// Manage projects, API keys, products, entitlements and store credentials.
    Admin(AdminArgs),
}

#[derive(Args)]
pub struct AdminArgs {
    /// Base URL of the entitle server.
    #[arg(long, env = "ENTITLE_URL", default_value = "http://localhost:8080")]
    pub url: String,
    /// Admin token. Prefer the environment variable: flags end up in shell history.
    #[arg(long, env = "ENTITLE_ADMIN_TOKEN", hide_env_values = true)]
    pub token: String,
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Projects: one per app (or group of apps sharing entitlements).
    #[command(subcommand)]
    Projects(ProjectsCommand),
    /// API keys: `pk_` for apps, `sk_` for your backend.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Store products, as registered in App Store Connect, Play Console or Creem.
    #[command(subcommand)]
    Products(ProductsCommand),
    /// Entitlements: what the app checks, unlocked by products.
    #[command(subcommand)]
    Entitlements(EntitlementsCommand),
    /// Store credentials (stored encrypted; never shown again).
    #[command(subcommand)]
    Credentials(CredentialsCommand),
    /// Look up customers and grant or revoke promotional access.
    #[command(subcommand)]
    Customers(CustomersCommand),
}

#[derive(Subcommand)]
pub enum CustomersCommand {
    /// Entitlements and purchases of an app user id (never creates the customer).
    Get {
        /// Project id or name.
        project: String,
        app_user_id: String,
    },
    /// Grant an entitlement (promotional), creating the customer if needed.
    Grant {
        /// Project id or name.
        project: String,
        app_user_id: String,
        /// Entitlement identifier.
        entitlement: String,
        /// Grant for this many days instead of for life.
        #[arg(long)]
        days: Option<u32>,
    },
    /// Revoke the customer's promotional grants of an entitlement.
    Revoke {
        /// Project id or name.
        project: String,
        app_user_id: String,
        /// Entitlement identifier.
        entitlement: String,
    },
}

#[derive(Subcommand)]
pub enum ProjectsCommand {
    List,
    Create {
        name: String,
        /// Reporting currency for revenue metrics (ISO 4217, default USD).
        #[arg(long)]
        currency: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum KeysCommand {
    List {
        /// Project id or name.
        project: String,
    },
    /// Create a key. It is printed once; only its hash is stored.
    Create {
        /// Project id or name.
        project: String,
        /// A secret (`sk_`) key instead of a public (`pk_`) one.
        #[arg(long)]
        secret: bool,
    },
}

#[derive(Subcommand)]
pub enum ProductsCommand {
    List {
        /// Project id or name.
        project: String,
    },
    Create {
        /// Project id or name.
        project: String,
        /// The product id in the store, e.g. `com.example.pro.monthly`.
        store_product_id: String,
        #[arg(long, value_parser = STORES)]
        store: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        /// Renewal period of a subscription: P1W, P1M, P2M, P3M, P6M or P1Y.
        #[arg(long)]
        billing_period: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum EntitlementsCommand {
    List {
        /// Project id or name.
        project: String,
    },
    Create {
        /// Project id or name.
        project: String,
        /// Identifier the app checks, e.g. `pro`.
        identifier: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Let a product unlock an entitlement.
    Attach {
        /// Project id or name.
        project: String,
        /// Entitlement identifier or id.
        entitlement: String,
        /// Store product id or product id.
        product: String,
        /// Store of the product, when the same store product id exists in several stores.
        #[arg(long, value_parser = STORES)]
        store: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum CredentialsCommand {
    /// Stores with credentials (secrets are never returned).
    List {
        /// Project id or name.
        project: String,
    },
    /// Set credentials from a JSON file with the body documented in docs/api.md.
    Set {
        /// Project id or name.
        project: String,
        #[arg(value_parser = STORES)]
        store: String,
        file: PathBuf,
    },
    /// Make one authenticated call to the store with the saved credentials.
    Test {
        /// Project id or name.
        project: String,
        #[arg(value_parser = STORES)]
        store: String,
    },
    /// Set App Store credentials from an App Store Connect In-App Purchase key.
    SetApple {
        /// Project id or name.
        project: String,
        #[arg(long)]
        issuer_id: String,
        #[arg(long)]
        key_id: String,
        /// The downloaded `.p8` private key.
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long)]
        bundle_id: String,
        /// Apple ID of the app; required once the app is live.
        #[arg(long)]
        app_apple_id: Option<u64>,
    },
}

struct Client {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Client {
    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let url = format!("{}/admin/v1{path}", self.base.trim_end_matches('/'));
        let mut request = self.http.request(method, &url).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
                .unwrap_or(text);
            bail!("{status}: {message}");
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("response is not JSON")
    }

    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.send(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.send(Method::POST, path, Some(body)).await
    }

    /// `/projects/{id}` for a project id or an exact project name.
    async fn project(&self, project: &str) -> anyhow::Result<String> {
        if let Ok(id) = project.parse::<Uuid>() {
            return Ok(format!("/projects/{id}"));
        }
        let projects = self.get("/projects").await?;
        let id = unique(&projects, "project", project, |p| p["name"] == project)?;
        Ok(format!("/projects/{id}"))
    }
}

/// Percent-encodes a path segment (app user ids may contain `$`, `:` or `/`).
fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The `id` of the only item matching `pred`.
fn unique(
    items: &Value,
    what: &str,
    name: &str,
    pred: impl Fn(&Value) -> bool,
) -> anyhow::Result<String> {
    let found: Vec<&Value> = items
        .as_array()
        .map(|a| a.iter().filter(|i| pred(i)).collect())
        .unwrap_or_default();
    match found.as_slice() {
        [one] => Ok(one["id"].as_str().context("item without id")?.to_owned()),
        [] => bail!("no {what} `{name}`"),
        _ => bail!("several {what}s match `{name}`; use its id"),
    }
}

/// Runs an admin command. Returns the response to print: JSON, or a message string.
pub async fn run_admin(args: AdminArgs) -> anyhow::Result<Value> {
    let c = Client {
        http: reqwest::Client::new(),
        base: args.url,
        token: args.token,
    };
    match args.command {
        AdminCommand::Projects(ProjectsCommand::List) => c.get("/projects").await,
        AdminCommand::Projects(ProjectsCommand::Create { name, currency }) => {
            let project = c.post("/projects", json!({ "name": name })).await?;
            match currency {
                Some(currency) => {
                    let id = project["id"].as_str().context("project without id")?;
                    c.send(
                        Method::PATCH,
                        &format!("/projects/{id}"),
                        Some(json!({ "reporting_currency": currency })),
                    )
                    .await
                }
                None => Ok(project),
            }
        }
        AdminCommand::Keys(KeysCommand::List { project }) => {
            c.get(&format!("{}/api-keys", c.project(&project).await?))
                .await
        }
        AdminCommand::Keys(KeysCommand::Create { project, secret }) => {
            let kind = if secret { "secret" } else { "public" };
            let key = c
                .post(
                    &format!("{}/api-keys", c.project(&project).await?),
                    json!({ "kind": kind }),
                )
                .await?;
            eprintln!("Copy the key now: it is not shown again.");
            Ok(key)
        }
        AdminCommand::Products(ProductsCommand::List { project }) => {
            c.get(&format!("{}/products", c.project(&project).await?))
                .await
        }
        AdminCommand::Products(ProductsCommand::Create {
            project,
            store_product_id,
            store,
            name,
            billing_period,
        }) => {
            c.post(
                &format!("{}/products", c.project(&project).await?),
                json!({
                    "store": store,
                    "store_product_id": store_product_id,
                    "display_name": name,
                    "billing_period": billing_period,
                }),
            )
            .await
        }
        AdminCommand::Entitlements(EntitlementsCommand::List { project }) => {
            c.get(&format!("{}/entitlements", c.project(&project).await?))
                .await
        }
        AdminCommand::Entitlements(EntitlementsCommand::Create {
            project,
            identifier,
            description,
        }) => {
            c.post(
                &format!("{}/entitlements", c.project(&project).await?),
                json!({ "identifier": identifier, "description": description }),
            )
            .await
        }
        AdminCommand::Entitlements(EntitlementsCommand::Attach {
            project,
            entitlement,
            product,
            store,
        }) => {
            let base = c.project(&project).await?;
            let entitlements = c.get(&format!("{base}/entitlements")).await?;
            let ent_id = unique(&entitlements, "entitlement", &entitlement, |e| {
                e["identifier"] == entitlement.as_str() || e["id"] == entitlement.as_str()
            })?;
            let products = c.get(&format!("{base}/products")).await?;
            let product_id = unique(&products, "product", &product, |p| {
                (p["store_product_id"] == product.as_str() || p["id"] == product.as_str())
                    && store.as_deref().is_none_or(|s| p["store"] == s)
            })?;
            c.send(
                Method::PUT,
                &format!("{base}/entitlements/{ent_id}/products/{product_id}"),
                None,
            )
            .await?;
            Ok(json!(format!("{product} now unlocks {entitlement}")))
        }
        AdminCommand::Customers(CustomersCommand::Get {
            project,
            app_user_id,
        }) => {
            let base = c.project(&project).await?;
            c.get(&format!("{base}/customers/{}", segment(&app_user_id)))
                .await
        }
        AdminCommand::Customers(CustomersCommand::Grant {
            project,
            app_user_id,
            entitlement,
            days,
        }) => {
            let base = c.project(&project).await?;
            let expires_at = days.map(|d| Utc::now() + Duration::days(d.into()));
            c.post(
                &format!(
                    "{base}/customers/{}/entitlements/{}/grant",
                    segment(&app_user_id),
                    segment(&entitlement)
                ),
                json!({ "expires_at": expires_at }),
            )
            .await
        }
        AdminCommand::Customers(CustomersCommand::Revoke {
            project,
            app_user_id,
            entitlement,
        }) => {
            let base = c.project(&project).await?;
            c.send(
                Method::POST,
                &format!(
                    "{base}/customers/{}/entitlements/{}/revoke",
                    segment(&app_user_id),
                    segment(&entitlement)
                ),
                None,
            )
            .await
        }
        AdminCommand::Credentials(CredentialsCommand::List { project }) => {
            c.get(&format!("{}/credentials", c.project(&project).await?))
                .await
        }
        AdminCommand::Credentials(CredentialsCommand::Set {
            project,
            store,
            file,
        }) => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let body: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", file.display()))?;
            let base = c.project(&project).await?;
            c.send(
                Method::PUT,
                &format!("{base}/credentials/{store}"),
                Some(body),
            )
            .await?;
            Ok(json!(format!("{store} credentials saved")))
        }
        AdminCommand::Credentials(CredentialsCommand::Test { project, store }) => {
            let base = c.project(&project).await?;
            let check = c
                .send(
                    Method::POST,
                    &format!("{base}/credentials/{store}/test"),
                    None,
                )
                .await?;
            let message = check["message"].as_str().unwrap_or_default();
            if check["ok"] != true {
                bail!("{store}: {message}");
            }
            Ok(json!(message))
        }
        AdminCommand::Credentials(CredentialsCommand::SetApple {
            project,
            issuer_id,
            key_id,
            key_file,
            bundle_id,
            app_apple_id,
        }) => {
            let private_key = std::fs::read_to_string(&key_file)
                .with_context(|| format!("reading {}", key_file.display()))?;
            let base = c.project(&project).await?;
            c.send(
                Method::PUT,
                &format!("{base}/credentials/app_store"),
                Some(json!({
                    "issuer_id": issuer_id,
                    "key_id": key_id,
                    "private_key": private_key,
                    "bundle_id": bundle_id,
                    "app_apple_id": app_apple_id,
                })),
            )
            .await?;
            Ok(json!("app_store credentials saved"))
        }
    }
}
