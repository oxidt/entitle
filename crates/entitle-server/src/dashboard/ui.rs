//! Small building blocks shared by the dashboard pages.

use chrono::{DateTime, Utc};
use topcoat::{
    Result,
    context::Cx,
    view::{Child, View, component, view},
};

use super::format::utc_text;

/// A card with a title and an optional intro line.
#[component]
pub async fn section(
    cx: &Cx,
    id: Option<&str>,
    title: &str,
    intro: Option<&str>,
    danger: bool,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    Ok(view! {
        <section
            class=(if danger {
                "card card-elevated bg-base-100 setup-card danger-zone"
            } else {
                "card card-elevated bg-base-100 setup-card"
            })
            id=(id)
        >
            <h2>(title)</h2>
            if let Some(intro) = intro {
                <p class="caption">(intro)</p>
            }
            (child)
        </section>
    })
}

/// A labelled form control, with an optional hint below it.
#[component]
pub async fn field(
    cx: &Cx,
    label: &str,
    hint: Option<&str>,
    #[default] child: Child<'_>,
) -> Result<impl View> {
    Ok(view! {
        <label class="field">
            <span class="field-label">(label)</span>
            (child)
            if let Some(hint) = hint {
                <span class="hint">(hint)</span>
            }
        </label>
    })
}

#[component]
pub async fn error_text(cx: &Cx, error: Option<&str>) -> Result<impl View> {
    Ok(view! {
        if let Some(error) = error {
            <p class="error" role="alert">(error)</p>
        }
    })
}

/// A value with a copy button, e.g. an id or a URL to paste into a store console.
#[component]
pub async fn copy_value(cx: &Cx, value: &str) -> Result<impl View> {
    Ok(view! {
        <span class="copy-value">
            <code>(value)</code>
            <button
                type="button"
                class="btn btn-ghost btn-xs btn-square"
                data-copy=(value)
                aria-label="Copy"
                title="Copy"
            >
                glyph(kind: Glyph::Copy)
                glyph(kind: Glyph::Check)
            </button>
        </span>
    })
}

/// The pictures on icon-only buttons; the button carries the label.
#[derive(Clone, Copy, PartialEq)]
pub enum Glyph {
    Copy,
    Check,
    Trash,
    Ban,
    Refresh,
    Close,
}

#[component]
pub async fn glyph(cx: &Cx, kind: Glyph) -> Result<impl View> {
    Ok(view! {
        <svg
            class="icon"
            viewBox="0 0 24 24"
            aria-hidden="true"
            data-glyph=(match kind {
                Glyph::Check => Some("check"),
                _ => None,
            })
        >
            match kind {
                Glyph::Copy => {
                    <rect x="9" y="9" width="11" height="11" rx="2"></rect>
                    <path d="M5 15V5a2 2 0 0 1 2-2h8"></path>
                }
                Glyph::Check => {
                    <path d="M20 6 9 17l-5-5"></path>
                }
                Glyph::Trash => {
                    <path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"></path>
                }
                Glyph::Ban => {
                    <circle cx="12" cy="12" r="9"></circle>
                    <path d="M5.6 5.6l12.8 12.8"></path>
                }
                Glyph::Refresh => {
                    <path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"></path>
                }
                Glyph::Close => {
                    <path d="M6 6l12 12M18 6 6 18"></path>
                }
            }
        </svg>
    })
}

/// An instant in the viewer's time zone (`format`: a `LOCAL_FORMATS` key in dashboard.js).
#[component]
pub async fn local_time(cx: &Cx, at: DateTime<Utc>, format: &str) -> Result<impl View> {
    Ok(view! {
        <time data-local=(format) datetime=(at.to_rfc3339())>(utc_text(at))</time>
    })
}

#[component]
pub async fn status_badge(cx: &Cx, status: &str) -> Result<impl View> {
    let tone = match status {
        "active" | "trial" => "badge-success",
        "grace_period" | "billing_retry" | "refunded" | "revoked" => "badge-error",
        _ => "",
    };
    Ok(view! {
        <span class=(format!("badge badge-sm badge-soft status {tone}"))>
            (status.replace('_', " "))
        </span>
    })
}

pub fn store_name(store: &str) -> &str {
    match store {
        "creem" => "Creem",
        "app_store" => "App Store",
        "play_store" => "Google Play",
        other => other,
    }
}

/// Percent-encodes everything but unreserved characters, for query values.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
