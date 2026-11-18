//! A message for the page a form post redirects to: an error, a notice, or a
//! secret the API returns once. Kept in an encrypted cookie for one render.

use serde::{Deserialize, Serialize};
use topcoat::{
    context::Cx,
    cookie::{Cookie, Cookies, SameSite, private_cookies},
};

use super::BASE;

const COOKIE: &str = "entitle_flash";

#[derive(Serialize, Deserialize, Clone)]
pub struct Flash {
    /// Which part of the page shows it, e.g. `keys` or `store:creem`.
    pub target: String,
    pub kind: Kind,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A failed action, shown as error text.
    Error,
    /// A check that passed.
    Good,
    /// A check that failed (e.g. testing store credentials).
    Bad,
    /// A value the API returns once.
    Secret,
}

pub fn set(cx: &Cx, target: &str, kind: Kind, text: impl Into<String>) {
    let flash = Flash {
        target: target.to_owned(),
        kind,
        text: text.into(),
    };
    let value = serde_json::to_string(&flash).expect("flash serializes");
    private_cookies(cx).add(
        Cookie::build((COOKIE, value))
            .path(BASE)
            .http_only(true)
            .same_site(SameSite::Strict)
            .build(),
    );
}

/// The pending flash, consumed.
pub fn take(cx: &Cx) -> Option<Flash> {
    let jar = private_cookies(cx);
    let flash = jar
        .get(COOKIE)
        .and_then(|c| serde_json::from_str(c.value()).ok());
    if flash.is_some() {
        jar.remove(Cookie::build(COOKIE).path(BASE).build());
    }
    flash
}
