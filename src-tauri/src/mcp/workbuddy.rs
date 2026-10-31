//! WorkBuddy MCP 同步。
//!
//! WorkBuddy 把用户级 MCP 服务器放在 `~/.workbuddy/connectors/default/mcp.json`
//! 的 `mcpServers` 里，条目形状和它的 connector 一致：
//!
//! ```json
//! { "mcpServers": { "connector:linear-mcp": { "url": "...", "disabled": true } } }
//! ```
//!
//! 本模块只增删自己写入的条目，键名加 `ccswitch:` 前缀，避免和 WorkBuddy 自带的
//! `connector:*` 冲突；其他条目原样保留。

use crate::config::atomic_write_private;
use crate::error::AppError;
use serde_json::{json, Value};
use std::{fs, path::PathBuf, sync::Mutex};

static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn config_path() -> PathBuf {
    crate::workbuddy_config::data_dir().join("connectors/default/mcp.json")
}

fn entry_key(id: &str) -> String {
    format!("ccswitch:{id}")
}

/// 写入或删除一个 MCP 服务器。`spec` 为 `None` 时删除。
pub fn sync(id: &str, spec: Option<&Value>) -> Result<(), AppError> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|error| AppError::Message(error.to_string()))?;
    sync_at(&config_path(), id, spec)
}

/// `sync` 的可测内核：直接对指定文件读写。
fn sync_at(path: &std::path::Path, id: &str, spec: Option<&Value>) -> Result<(), AppError> {
    let mut document = read(path)?;

    let servers = document
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| AppError::Config("Invalid WorkBuddy mcpServers object".into()))?;

    match spec {
        Some(spec) => {
            super::validation::validate_server_spec(spec)?;
            servers.insert(entry_key(id), to_entry(spec));
        }
        None => {
            servers.remove(&entry_key(id));
        }
    }

    write(path, &document)
}

pub fn remove_server_from_workbuddy(id: &str) -> Result<(), AppError> {
    sync(id, None)
}

fn read(path: &std::path::Path) -> Result<Value, AppError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "mcpServers": {} }))
        }
        Err(error) => return Err(AppError::io(path, error)),
    };
    if text.trim().is_empty() {
        return Ok(json!({ "mcpServers": {} }));
    }
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|_| AppError::Config("Invalid WorkBuddy MCP JSON".into()))?;
    if !value.is_object() {
        return Err(AppError::Config(
            "Invalid WorkBuddy MCP configuration".into(),
        ));
    }
    if value.get("mcpServers").is_none() {
        value["mcpServers"] = json!({});
    }
    if value.get("mcpServers").is_some_and(|servers| !servers.is_object()) {
        return Err(AppError::Config(
            "Invalid WorkBuddy mcpServers object".into(),
        ));
    }
    Ok(value)
}

fn write(path: &std::path::Path, document: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| AppError::Message(format!("Cannot create WorkBuddy dir: {error}")))?;
    }
    let mut data = serde_json::to_vec_pretty(document)
        .map_err(|error| AppError::Config(format!("Cannot serialize WorkBuddy MCP: {error}")))?;
    data.push(b'\n');
    atomic_write_private(path, &data)
}

/// 把 cc-switch 的服务器配置翻译成 WorkBuddy 的条目形状。
fn to_entry(spec: &Value) -> Value {
    let mut entry = serde_json::Map::new();
    for field in ["command", "args", "env", "url", "headers"] {
        if let Some(value) = spec.get(field) {
            if !value.is_null() {
                entry.insert(field.to_string(), value.clone());
            }
        }
    }
    if let Some(kind) = spec.get("type").and_then(Value::as_str) {
        entry.insert("type".into(), json!(kind));
    } else if entry.contains_key("url") {
        // WorkBuddy 的远程连接器默认用 streamable HTTP。
        entry.insert("type".into(), json!("streamableHttp"));
    }
    // 从 cc-switch 启用的服务器要立刻可用，不能落成 disabled。
    entry.insert("disabled".into(), json!(false));
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_keeps_transport_and_enables_it() {
        let entry = to_entry(&json!({"type": "sse", "url": "https://a.dev/mcp"}));
        assert_eq!(entry["type"], json!("sse"));
        assert_eq!(entry["url"], json!("https://a.dev/mcp"));
        assert_eq!(entry["disabled"], json!(false));
    }

    #[test]
    fn entry_defaults_remote_type_to_streamable_http() {
        let entry = to_entry(&json!({"url": "https://a.dev/mcp"}));
        assert_eq!(entry["type"], json!("streamableHttp"));
    }

    #[test]
    fn stdio_entry_keeps_command_fields() {
        let entry = to_entry(&json!({"command": "npx", "args": ["-y", "server"]}));
        assert_eq!(entry["command"], json!("npx"));
        assert_eq!(entry["args"], json!(["-y", "server"]));
        assert!(entry.get("type").is_none());
    }

    #[test]
    fn keys_are_namespaced() {
        assert_eq!(entry_key("github"), "ccswitch:github");
    }

    #[test]
    fn writes_and_removes_entry_keeping_connectors() {
        let dir = std::env::temp_dir().join(format!("workbuddy-mcp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mcp.json");
        fs::write(
            &path,
            json!({ "mcpServers": { "connector:linear-mcp": { "url": "https://x", "disabled": true } } })
                .to_string(),
        )
        .unwrap();

        let spec = json!({"command": "npx", "args": ["-y", "server"]});
        sync_at(&path, "github", Some(&spec)).unwrap();
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let servers = doc["mcpServers"].as_object().unwrap();
        assert!(servers.contains_key("connector:linear-mcp"), "原有条目必须保留");
        assert_eq!(servers["ccswitch:github"]["command"], json!("npx"));
        assert_eq!(servers["ccswitch:github"]["disabled"], json!(false));

        sync_at(&path, "github", None).unwrap();
        let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!doc["mcpServers"]
            .as_object()
            .unwrap()
            .contains_key("ccswitch:github"));
        assert!(doc["mcpServers"]
            .as_object()
            .unwrap()
            .contains_key("connector:linear-mcp"));
        let _ = fs::remove_dir_all(&dir);
    }
}
