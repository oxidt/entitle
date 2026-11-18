use std::net::SocketAddr;

use anyhow::{Context, bail};

/// Runtime configuration, read from the environment.
#[derive(Clone)]
pub struct Config {
    /// `DATABASE_URL`
    pub database_url: String,
    /// `ENTITLE_BIND`, default `0.0.0.0:8080`
    pub bind: SocketAddr,
    /// `ENTITLE_ADMIN_TOKEN`, bearer token for `/admin/v1` (at least 32 chars).
    pub admin_token: String,
    /// `ENTITLE_ENCRYPTION_KEY`, base64 of 32 random bytes (`openssl rand -base64 32`).
    pub encryption_key: String,
    /// `ENTITLE_WEBHOOK_ALLOW_PRIVATE=true`: outgoing webhooks may target any
    /// http(s) host, including the private network (e.g. `http://backend:8080`).
    /// Default: public https hosts only.
    pub webhook_allow_private: bool,
    /// `ENTITLE_TRUST_PROXY=true`: a reverse proxy sets `X-Forwarded-For`, so the client IP
    /// for rate limits is its last entry. Default: the TCP peer address.
    pub trust_proxy: bool,
    /// `ENTITLE_PUBLIC_URL`, e.g. `https://entitle.example.com`: where clients reach this
    /// server. Enables the MCP endpoint and its OAuth authorization server.
    pub public_url: Option<String>,
    /// `ENTITLE_SDK_RATE_PER_IP`: public-key requests per minute and client IP to the
    /// SDK routes that create customers or call a store. Default 600.
    pub sdk_rate_per_ip: u32,
    /// `ENTITLE_SDK_RATE_PER_PROJECT`: the same, per project. Default 6000.
    pub sdk_rate_per_project: u32,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let var = |name: &str| std::env::var(name).with_context(|| format!("{name} is not set"));
        let admin_token = var("ENTITLE_ADMIN_TOKEN")?;
        if admin_token.len() < 32 {
            bail!("ENTITLE_ADMIN_TOKEN must be at least 32 characters");
        }
        Ok(Config {
            database_url: var("DATABASE_URL")?,
            bind: std::env::var("ENTITLE_BIND")
                .unwrap_or_else(|_| "0.0.0.0:8080".into())
                .parse()
                .context("ENTITLE_BIND is not a socket address")?,
            admin_token,
            encryption_key: var("ENTITLE_ENCRYPTION_KEY")?,
            webhook_allow_private: flag("ENTITLE_WEBHOOK_ALLOW_PRIVATE")?,
            trust_proxy: flag("ENTITLE_TRUST_PROXY")?,
            public_url: match std::env::var("ENTITLE_PUBLIC_URL") {
                Ok(url) if !url.is_empty() => Some(parse_public_url(&url)?),
                _ => None,
            },
            sdk_rate_per_ip: per_minute("ENTITLE_SDK_RATE_PER_IP", 600)?,
            sdk_rate_per_project: per_minute("ENTITLE_SDK_RATE_PER_PROJECT", 6000)?,
        })
    }
}

/// A boolean env var, false when unset.
fn flag(name: &str) -> anyhow::Result<bool> {
    match std::env::var(name).as_deref() {
        Ok("true" | "1") => Ok(true),
        Ok("false" | "0" | "") | Err(_) => Ok(false),
        Ok(other) => bail!("{name} must be true or false, got `{other}`"),
    }
}

/// A positive request rate, `default` when unset.
fn per_minute(name: &str, default: u32) -> anyhow::Result<u32> {
    match std::env::var(name).as_deref() {
        Err(_) | Ok("") => Ok(default),
        Ok(value) => match value.parse() {
            Ok(n) if n > 0 => Ok(n),
            _ => bail!("{name} must be a positive number of requests per minute, got `{value}`"),
        },
    }
}

/// An http(s) origin without a trailing slash.
fn parse_public_url(url: &str) -> anyhow::Result<String> {
    let parsed = reqwest::Url::parse(url).context("ENTITLE_PUBLIC_URL is not a URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!(
            "ENTITLE_PUBLIC_URL must be an http(s) origin like https://entitle.example.com, got `{url}`"
        );
    }
    Ok(parsed.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::parse_public_url;

    #[test]
    fn public_url_is_an_origin() {
        assert_eq!(
            parse_public_url("https://Entitle.example.com/").unwrap(),
            "https://entitle.example.com"
        );
        assert_eq!(
            parse_public_url("http://localhost:8080").unwrap(),
            "http://localhost:8080"
        );
        for bad in [
            "https://e.example.com/x",
            "https://e.example.com/?a=1",
            "https://e.example.com/#a",
            "https://u:p@e.example.com",
            "ftp://e.example.com",
        ] {
            assert!(parse_public_url(bad).is_err(), "{bad}");
        }
    }
}
