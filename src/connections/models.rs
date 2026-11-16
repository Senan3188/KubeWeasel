use crate::error::{Result, invalid};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) const KIND: &str = "kubeweasel.io/connection";
pub(super) const META: &str = "kubeweasel.io/connection-config";
pub(super) const SELECTOR: &str =
    "app.kubernetes.io/managed-by=kubeweasel,kubeweasel.io/connection=true";
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Description {
    pub name: String,
    pub kind: String,
    pub project_id: String,
}
#[derive(Deserialize)]
pub(super) struct Input {
    #[serde(flatten)]
    pub description: Description,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
    #[serde(default)]
    pub remove_keys: Vec<String>,
    pub version: Option<String>,
}
#[derive(Clone, Serialize)]
pub(super) struct Consumer {
    pub name: String,
    pub kind: String,
    pub status: String,
    pub restart_required: bool,
}
#[derive(Clone, Serialize)]
pub(super) struct View {
    pub id: String,
    #[serde(flatten)]
    pub description: Description,
    pub keys: Vec<String>,
    pub version: String,
    pub mappings: BTreeMap<String, String>,
    pub workload_version: String,
    pub consumers: Vec<Consumer>,
    pub usage_error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attachment {
    pub mappings: BTreeMap<String, String>,
    pub version: String,
    pub workload_version: String,
}
pub(super) fn valid_key(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
}
pub(super) fn valid_env(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}
pub(super) fn validate(input: &Input) -> Result<()> {
    let d = &input.description;
    if d.name.trim().is_empty()
        || d.name.chars().count() > 80
        || d.name.chars().any(char::is_control)
        || !["postgresql", "mysql", "redis", "kafka", "custom"].contains(&d.kind.as_str())
        || d.project_id.len() != 32
        || !d.project_id.bytes().all(|c| c.is_ascii_hexdigit())
        || input.values.len() > 100
        || input.remove_keys.len() > 100
        || input
            .values
            .iter()
            .any(|(k, v)| !valid_key(k) || v.len() > 16384)
        || input
            .remove_keys
            .iter()
            .any(|k| !valid_key(k) || input.values.contains_key(k))
        || input.values.values().map(String::len).sum::<usize>() > 65536
    {
        return Err(invalid(
            "Use a connection name, supported type, and up to 100 valid keys (16 KiB each, 64 KiB total).",
        ));
    }
    Ok(())
}
