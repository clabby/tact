//! Serde adapters shared by configuration sections.
//!
//! The effective configuration renders every user-facing key, so unset optional strings render as
//! empty strings and secrets render as `[REDACTED]` when present. Rendering an empty value for an
//! unset key keeps the rendered file loadable with the same meaning, because loading treats blank
//! strings as unset.

use crate::app::secret::SecretString;
use serde::{Deserialize, Deserializer, Serializer};
use std::sync::Arc;

/// The rendered value of every secret that is present.
pub(super) const REDACTED: &str = "[REDACTED]";

pub(super) fn serialize_optional_string<S>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(value.as_deref().unwrap_or_default())
}

/// Reads a secret, treating a blank value as absent. Present secrets are shared through `Arc` so
/// cloned configurations do not duplicate secret bytes.
pub(super) fn deserialize_optional_secret<'de, D>(
    deserializer: D,
) -> Result<Option<Arc<SecretString>>, D::Error>
where
    D: Deserializer<'de>,
{
    let secret = SecretString::deserialize(deserializer)?;
    Ok((!secret.expose_secret().trim().is_empty()).then(|| Arc::new(secret)))
}

pub(super) fn serialize_optional_secret<S>(
    secret: &Option<Arc<SecretString>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(if secret.is_some() { REDACTED } else { "" })
}

/// Treats an empty string from the CLI or the file as unset.
pub(super) fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}
