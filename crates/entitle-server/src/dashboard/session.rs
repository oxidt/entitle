//! Signing in and out. The session is a signed cookie holding its expiry; the
//! signing key derives from the admin token (see `session_key`).

use chrono::Utc;
use serde::Deserialize;
use topcoat::{
    Result,
    context::{Cx, context},
    cookie::{Cookie, Cookies, SameSite, signed_cookies},
    router::{
        Body, Layer, LayerFuture, Next, Path, RouterBuilder,
        content::Form,
        error::{SeeOther, redirect, see_other},
        page, query_params,
        request::uri,
        route,
    },
    view::{View, view},
};

use super::BASE;
use crate::AppState;
use crate::auth::is_admin_token;

const COOKIE: &str = "entitle_session";
const LOGIN: &str = "/dashboard/login";
/// Signed-in time before the cookie stops verifying.
const LIFETIME_HOURS: i64 = 12;

pub fn register(builder: RouterBuilder) -> RouterBuilder {
    builder
        .layer(Guard)
        .page(login)
        .route(sign_in)
        .route(sign_out)
}

fn state(cx: &Cx) -> &AppState {
    context::<AppState>(cx)
}

/// Whether the request carries an unexpired session cookie.
pub fn signed_in(cx: &Cx) -> bool {
    signed_cookies(cx)
        .get(COOKIE)
        .and_then(|c| c.value().parse::<i64>().ok())
        .is_some_and(|expires| expires > Utc::now().timestamp())
}

/// Sends every dashboard request without a session to the login page.
struct Guard;

impl Layer for Guard {
    fn path(&self) -> Option<&Path> {
        Some(Path::new(BASE))
    }

    fn handle<'a>(&'a self, cx: &'a Cx, body: Body, next: Next<'a>) -> LayerFuture<'a> {
        Box::pin(async move {
            if uri(cx).path() == LOGIN || signed_in(cx) {
                next.run(cx, body).await
            } else {
                Err(redirect(LOGIN).into())
            }
        })
    }
}

#[derive(Deserialize)]
struct SignIn {
    token: String,
}

/// `?error=1` after a rejected token.
#[query_params]
struct LoginQuery {
    error: Option<String>,
}

#[page("/dashboard/login")]
async fn login(cx: &Cx) -> Result<impl View> {
    if signed_in(cx) {
        return Err(redirect("/dashboard/").into());
    }
    let rejected = query_params::<LoginQuery>(cx).is_ok_and(|q| q.error.is_some());
    let error = rejected.then_some("That token was rejected.");
    Ok(view! {
        super::shell::document(
            title: "Sign in",
            <main class="login auth-bg">
                <form class="login-card auth-card" method="post" action=(LOGIN)>
                    <div class="brand">
                        <span class="brand-mark" aria-hidden="true"></span>
                        "entitle"
                    </div>
                    <h1>"Sign in"</h1>
                    <label for="token">"Admin token"</label>
                    <input
                        class="input w-full"
                        id="token"
                        name="token"
                        type="password"
                        autocomplete="current-password"
                        placeholder="ENTITLE_ADMIN_TOKEN"
                        required=(true)
                        autofocus=(true)
                    >
                    if let Some(error) = error {
                        <p class="error" role="alert">(error)</p>
                    }
                    <button type="submit" class="btn btn-primary w-full">
                        "Sign in"
                    </button>
                    <p class="hint">"The token stays on the server."</p>
                </form>
            </main>
        )
    })
}

#[route(POST "/dashboard/login")]
async fn sign_in(cx: &Cx, Form(form): Form<SignIn>) -> Result<SeeOther> {
    if !is_admin_token(state(cx), form.token.trim()) {
        return Ok(see_other(format!("{LOGIN}?error=1")));
    }
    let expires = Utc::now().timestamp() + LIFETIME_HOURS * 3600;
    signed_cookies(cx).add(session_cookie(expires, super::secure(cx)));
    Ok(see_other("/dashboard/"))
}

fn session_cookie(expires: i64, secure: bool) -> Cookie<'static> {
    Cookie::build((COOKIE, expires.to_string()))
        .path(BASE)
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .build()
}

#[route(POST "/dashboard/logout")]
async fn sign_out(cx: &Cx) -> Result<SeeOther> {
    signed_cookies(cx).remove(Cookie::build(COOKIE).path(BASE).build());
    Ok(see_other(LOGIN))
}

#[cfg(test)]
mod tests {
    use topcoat::cookie::SameSite;

    use super::session_cookie;

    #[test]
    fn cookie_is_secure_behind_an_https_public_url() {
        let secure = super::super::secure_cookies(Some("https://entitle.example.com"), None);
        let cookie = session_cookie(0, secure);
        assert_eq!(cookie.secure(), Some(true));
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));

        let secure = super::super::secure_cookies(Some("http://localhost:8080"), None);
        assert_eq!(session_cookie(0, secure).secure(), Some(false));
    }
}
