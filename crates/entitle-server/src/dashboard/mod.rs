//! The admin dashboard: server-rendered Topcoat pages under `/dashboard`.
//!
//! Signing in exchanges the admin token for a signed, HttpOnly session cookie,
//! so the token never reaches the page's JavaScript. Pages query the database
//! through the same functions as the admin API. Topcoat's origin policy rejects
//! cross-site form posts.

pub(crate) mod api;
mod customers;
mod flash;
mod format;
mod revenue;
mod session;
mod setup;
mod shell;
mod ui;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use sha2::{Digest, Sha512};
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::cookie::{Key, RouterBuilderCookieExt};
use topcoat::router::{Router as PageRouter, tower::TowerService};
use topcoat::runtime::RouterBuilderRuntimeExt;

use crate::AppState;

const NOT_BUILT: &str = "The dashboard's asset bundle is missing next to this binary. \
Run `topcoat asset bundle --package entitle-server` after building entitle.\n";

/// The pages' URL prefix.
pub const BASE: &str = "/dashboard";

pub fn router(state: AppState) -> axum::Router<AppState> {
    let root = axum::Router::new()
        // The bare domain has nothing else to show. Temporary, so browsers don't cache it.
        .route("/", get(|| async { Redirect::temporary("/dashboard/") }))
        .route(
            "/dashboard",
            get(|| async { Redirect::permanent("/dashboard/") }),
        );
    let Ok(bundle) = AssetBundle::load() else {
        tracing::warn!("{}", NOT_BUILT.trim_end());
        let not_built = || async {
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                NOT_BUILT,
            )
                .into_response()
        };
        return root
            .route("/dashboard/", get(not_built))
            .route("/dashboard/{*rest}", get(not_built));
    };
    let pages = TowerService::new(pages(state, bundle));
    // Topcoat serves its runtime and the hashed assets under `/_topcoat`.
    root.route_service("/dashboard/", pages.clone())
        .route_service("/dashboard/{*rest}", pages.clone())
        .route_service("/_topcoat/{*rest}", pages)
}

fn pages(state: AppState, bundle: AssetBundle) -> PageRouter {
    let key = session_key(&state.admin_token);
    let builder = PageRouter::builder();
    let builder = setup::register(customers::register(revenue::register(builder)));
    session::register(builder)
        .assets(bundle)
        .app_context(state)
        .app_context(key)
        .cookies()
        .runtime()
        .build()
}

/// Derived from the admin token: rotating the token signs everyone out.
fn session_key(admin_token: &str) -> Key {
    let mut h = Sha512::new();
    h.update(b"entitle dashboard session v1\0");
    h.update(admin_token.as_bytes());
    Key::from(&h.finalize())
}
