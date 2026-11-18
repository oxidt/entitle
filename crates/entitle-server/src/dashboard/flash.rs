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
    private_cookies(cx).add(flash_cookie(value, super::secure(cx)));
}

fn flash_cookie(value: String, secure: bool) -> Cookie<'static> {
    Cookie::build((COOKIE, value))
        .path(BASE)
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Strict)
        .build()
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

#[cfg(test)]
mod tests {
    use topcoat::cookie::SameSite;

    use super::flash_cookie;

    #[test]
    fn cookie_is_secure_behind_an_https_public_url() {
        let secure = super::super::secure_cookies(Some("https://entitle.example.com"), None);
        let cookie = flash_cookie(String::new(), secure);
        assert_eq!(cookie.secure(), Some(true));
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.same_site(), Some(SameSite::Strict));

        let secure = super::super::secure_cookies(Some("http://localhost:8080"), None);
        assert_eq!(flash_cookie(String::new(), secure).secure(), Some(false));
    }
}
