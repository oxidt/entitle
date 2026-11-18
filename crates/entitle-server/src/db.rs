//! Helpers for storing entitle-core enums as text columns.

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Enum → the text stored in the database (its serde name).
pub fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        other => panic!("not a unit enum: {other:?}"),
    }
}

pub fn parse_enum<T: DeserializeOwned>(s: &str) -> anyhow::Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(
        s.to_owned(),
    ))?)
}
