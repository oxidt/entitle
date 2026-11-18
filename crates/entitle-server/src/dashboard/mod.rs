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

use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use sha2::{Digest, Sha512};
use topcoat::asset::{AssetBundle, RouterBuilderAssetExt};
use topcoat::context::{Cx, context};
use topcoat::cookie::{CookieLayer, Key};
use topcoat::router::{Router as PageRouter, request::headers, tower::TowerService};
use topcoat::runtime::RuntimeLayer;

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
        let dashboard = axum::Router::new()
            .route("/dashboard/", get(not_built))
            .route("/dashboard/{*rest}", get(not_built));
        return root.merge(no_store(dashboard));
    };
    let pages = TowerService::new(pages(state, bundle));
    let dashboard = axum::Router::new()
        .route_service("/dashboard/", pages.clone())
        .route_service("/dashboard/{*rest}", pages.clone());
    // Topcoat serves its runtime and the hashed assets under `/_topcoat`.
    root.merge(no_store(dashboard))
        .route_service("/_topcoat/{*rest}", pages)
}

/// Pages can show secrets (a new API key, store setup), so nothing may cache them.
fn no_store(dashboard: axum::Router<AppState>) -> axum::Router<AppState> {
    dashboard.layer(map_response(|mut res: Response| async move {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res
    }))
}

/// Whether dashboard cookies get `Secure`: always behind an https
/// `ENTITLE_PUBLIC_URL`, otherwise when a proxy reports https, so plain http
/// stays usable for local development.
fn secure_cookies(public_url: Option<&str>, forwarded_proto: Option<&[u8]>) -> bool {
    public_url.is_some_and(|url| url.starts_with("https://")) || forwarded_proto == Some(b"https")
}

/// [`secure_cookies`] for the current request.
fn secure(cx: &Cx) -> bool {
    secure_cookies(
        context::<AppState>(cx).public_url.as_deref(),
        headers(cx).get("x-forwarded-proto").map(|p| p.as_bytes()),
    )
}

fn pages(state: AppState, bundle: AssetBundle) -> PageRouter {
    let key = session_key(&state.admin_token);
    let builder = PageRouter::builder();
    let builder = setup::register(customers::register(revenue::register(builder)));
    session::register(builder)
        .assets(bundle)
        .with(state)
        .with(key)
        .layer(CookieLayer::new())
        .layer(RuntimeLayer::new())
        .build()
}

/// Derived from the admin token: rotating the token signs everyone out.
fn session_key(admin_token: &str) -> Key {
    let mut h = Sha512::new();
    h.update(b"entitle dashboard session v1\0");
    h.update(admin_token.as_bytes());
    Key::from(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::secure_cookies;

    #[test]
    fn cookies_are_secure_behind_an_https_public_url() {
        assert!(secure_cookies(Some("https://entitle.example.com"), None));
        assert!(secure_cookies(
            Some("https://entitle.example.com"),
            Some(b"http")
        ));
        assert!(secure_cookies(None, Some(b"https")));
        assert!(secure_cookies(
            Some("http://localhost:8080"),
            Some(b"https")
        ));
        assert!(!secure_cookies(Some("http://localhost:8080"), None));
        assert!(!secure_cookies(None, None));
    }
}
