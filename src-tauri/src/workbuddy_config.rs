//! WorkBuddy (腾讯 WorkBuddy 桌面版) provider storage.
//!
//! WorkBuddy keeps every custom model in a single JSON array at
//! `~/.workbuddy/models.json`. Each element is one "provider" from CC Switch's
//! point of view, e.g.
//!
//! ```json
//! {
//!   "id": "deepseek-v4-flash",
//!   "name": "DeepSeek V4 Flash",
//!   "vendor": "Custom",
//!   "url": "https://api.deepseek.com/v1/chat/completions",
//!   "apiKey": "sk-...",
//!   "supportsToolCall": true,
//!   "supportsImages": false,
//!   "supportsReasoning": false,
//!   "useCustomProtocol": false
//! }
//! ```
//!
//! Because the file is a flat array, the provider's `settings_config` is stored
//! verbatim as one array element; unknown fields survive a round trip.

use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Serialises read-modify-write cycles inside this process.
static MODELS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn data_dir() -> PathBuf {
    std::env::var("WORKBUDDY_HOME")
        .ok()
        .map(|dir| dir.trim().to_string())
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| get_home_dir().join(".workbuddy"))
}

pub(crate) fn models_path() -> PathBuf {
    data_dir().join("models.json")
}

fn read_entries(path: &Path) -> Result<Vec<Value>, AppError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    // WorkBuddy always writes a bare array. A `{"models": [...]}` wrapper shows
    // up in older documentation, so accept it instead of destroying the file.
    let value: Value = serde_json::from_str(&text).map_err(|error| AppError::Json {
        path: path.display().to_string(),
        source: error,
    })?;
    let entries = match value {
        Value::Array(entries) => entries,
        Value::Object(mut map) => match map.remove("models") {
            Some(Value::Array(entries)) => entries,
            _ => {
                return Err(AppError::Config(
                    "WorkBuddy models.json must contain a JSON array of models".into(),
                ))
            }
        },
        Value::Null => Vec::new(),
        _ => {
            return Err(AppError::Config(
                "WorkBuddy models.json must contain a JSON array of models".into(),
            ))
        }
    };
    Ok(entries)
}

fn write_entries(path: &Path, entries: &[Value]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::Message(format!(
            "Cannot create WorkBuddy directory {}: {error}",
            parent.display()
        )))?;
    }
    let mut bytes = serde_json::to_vec_pretty(entries)
        .map_err(|error| AppError::Config(format!("Cannot serialize WorkBuddy models: {error}")))?;
    bytes.push(b'\n');
    atomic_write_private(path, &bytes)
}

fn entry_id(entry: &Value) -> Option<&str> {
    entry.get("id").and_then(Value::as_str).map(str::trim)
}

/// Providers currently present in `models.json`, keyed by their `id`.
pub(crate) fn get_providers() -> Result<IndexMap<String, Value>, AppError> {
    let path = models_path();
    let mut providers = IndexMap::new();
    for entry in read_entries(&path)? {
        if let Some(id) = entry_id(&entry) {
            if !id.is_empty() {
                providers.insert(id.to_string(), entry);
            }
        }
    }
    Ok(providers)
}

pub(crate) fn provider_key_exists(id: &str) -> Result<bool, AppError> {
    Ok(get_providers()?.contains_key(id.trim()))
}

pub(crate) fn validate_provider(id: &str, config: &Value) -> Result<(), AppError> {
    let id = id.trim();
    if id.is_empty()
        || id
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.' | '/'))
    {
        return Err(AppError::InvalidInput(
            "WorkBuddy model id must contain letters, digits, '-', '_', '.' or '/'".into(),
        ));
    }
    if !config.is_object() {
        return Err(AppError::InvalidInput(
            "WorkBuddy model configuration must be an object".into(),
        ));
    }
    let url = config
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if url.is_empty() {
        return Err(AppError::InvalidInput("Enter an API endpoint URL".into()));
    }
    if !url::Url::parse(url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
    {
        return Err(AppError::InvalidInput(
            "Enter a valid API endpoint URL (http/https)".into(),
        ));
    }
    if config
        .get("apiKey")
        .and_then(Value::as_str)
        .is_none_or(|key| key.trim().is_empty())
    {
        return Err(AppError::InvalidInput("Enter an API key".into()));
    }
    Ok(())
}

/// Normalised entry: forces `id` to match the database key and fills defaults.
fn normalize(id: &str, config: Value) -> Value {
    let mut entry = match config {
        Value::Object(map) => Value::Object(map),
        other => other,
    };
    let object = entry.as_object_mut().expect("validated object");
    object.insert("id".into(), json!(id.trim()));
    let name_missing = object
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(|name| name.trim().is_empty());
    if name_missing {
        object.insert("name".into(), json!(id.trim()));
    }
    object
        .entry("vendor")
        .or_insert_with(|| json!("Custom"));
    object
        .entry("supportsToolCall")
        .or_insert_with(|| json!(true));
    object
        .entry("supportsImages")
        .or_insert_with(|| json!(false));
    object
        .entry("supportsReasoning")
        .or_insert_with(|| json!(false));
    object
        .entry("useCustomProtocol")
        .or_insert_with(|| json!(false));
    entry
}

fn write_entry(id: &str, config: Option<Value>, create_only: bool) -> Result<(), AppError> {
    let _guard = MODELS_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let path = models_path();
    let previous = match fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(AppError::io(&path, error)),
    };
    let mut entries = read_entries(&path)?;
    let position = entries
        .iter()
        .position(|entry| entry_id(entry) == Some(id.trim()));

    if create_only && position.is_some() {
        return Err(AppError::InvalidInput(format!(
            "WorkBuddy model '{id}' already exists"
        )));
    }

    match (config, position) {
        (Some(config), Some(index)) => entries[index] = normalize(id, config),
        (Some(config), None) => entries.push(normalize(id, config)),
        (None, Some(index)) => {
            entries.remove(index);
        }
        (None, None) => return Ok(()),
    }

    match write_entries(&path, &entries) {
        Ok(()) => Ok(()),
        Err(error) => {
            let rollback = match previous {
                Some(bytes) => atomic_write_private(&path, &bytes),
                None => fs::remove_file(&path).map_err(|error| AppError::io(&path, error)),
            };
            if let Err(rollback_error) = rollback {
                return Err(AppError::Message(format!(
                    "Config update failed ({error}); restoring {} also failed: {rollback_error}",
                    path.display()
                )));
            }
            Err(error)
        }
    }
}

/// Insert or replace the model entry with this id.
pub(crate) fn set_provider(id: &str, config: Value) -> Result<(), AppError> {
    validate_provider(id, &config)?;
    write_entry(id, Some(config), false)
}

/// Insert a new entry, rejecting ids that already exist in `models.json`.
#[allow(dead_code)]
pub(crate) fn add_provider(id: &str, config: Value) -> Result<(), AppError> {
    validate_provider(id, &config)?;
    write_entry(id, Some(config), true)
}

pub(crate) fn remove_provider(id: &str) -> Result<(), AppError> {
    if !models_path().exists() {
        return Ok(());
    }
    write_entry(id, None, false)
}

/// Base URL used for usage/quota queries: the configured `url` minus any
/// `chat/completions` suffix.
pub(crate) fn provider_base_url(config: &Value) -> Option<String> {
    let url = config.get("url").and_then(Value::as_str)?.trim();
    if url.is_empty() {
        return None;
    }
    let trimmed = url
        .trim_end_matches('/')
        .trim_end_matches("/chat/completions")
        .to_string();
    Some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str) -> Value {
        json!({
            "id": id,
            "name": id,
            "url": "https://api.deepseek.com/v1/chat/completions",
            "apiKey": "sk-test",
        })
    }

    #[test]
    fn base_url_strips_chat_completions() {
        assert_eq!(
            provider_base_url(&sample("x")).as_deref(),
            Some("https://api.deepseek.com/v1")
        );
    }

    #[test]
    fn normalize_forces_id_and_defaults() {
        let entry = normalize("my-model", json!({"url": "https://a.dev/v1", "apiKey": "k"}));
        assert_eq!(entry["id"], json!("my-model"));
        assert_eq!(entry["name"], json!("my-model"));
        assert_eq!(entry["vendor"], json!("Custom"));
        assert_eq!(entry["supportsToolCall"], json!(true));
    }

    #[test]
    fn validate_rejects_bad_url_and_empty_key() {
        assert!(validate_provider("x", &json!({"url": "ftp://a", "apiKey": "k"})).is_err());
        assert!(validate_provider("x", &json!({"url": "https://a.dev", "apiKey": " "})).is_err());
        assert!(validate_provider("bad id!", &sample("bad id!")).is_err());
        assert!(validate_provider("ok-id", &sample("ok-id")).is_ok());
    }
}
