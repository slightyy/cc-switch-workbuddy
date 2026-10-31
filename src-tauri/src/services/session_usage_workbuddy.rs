//! WorkBuddy (腾讯 WorkBuddy 桌面版) session usage importer.
//!
//! WorkBuddy writes one JSONL transcript per session under
//! `~/.workbuddy/projects/<workspace>/<sessionId>.jsonl`, plus one transcript
//! per sub-agent under `<sessionId>/subagents/*.jsonl`. Every assistant /
//! tool-call record carries the provider-reported usage:
//!
//! ```json
//! {
//!   "id": "01a182...",
//!   "sessionId": "6fbc8b18-...",
//!   "timestamp": 1793187921885,
//!   "providerData": {
//!     "model": "deepseek-v4.1-flash",
//!     "usage": {
//!       "requests": 1,
//!       "inputTokens": 21822,
//!       "outputTokens": 163,
//!       "inputTokensDetails": [{ "cached_tokens": 10624 }],
//!       "outputTokensDetails": [{ "reasoning_tokens": 11 }]
//!     }
//!   }
//! }
//! ```
//!
//! `inputTokens` follows the OpenAI convention and **includes** the cached
//! portion, so rows are stored with [`INPUT_TOKEN_SEMANTICS_TOTAL`] and the
//! aggregation layer subtracts cache reads back out (see
//! `sql_helpers::CACHE_INCLUSIVE_APP_TYPES`). `reasoning_tokens` is already
//! part of `outputTokens`, so it is not added again.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL;
use crate::services::usage_stats::find_model_pricing;
use crate::workbuddy_config;
use rusqlite::params;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// 超过这个跨度的间隔不当作生成耗时：多半是用户走开、会话挂起，不是模型在生成。
/// 只截断到上限会把空闲时间算进速度里（实测把整体速度从 67 拉低到 40 tok/s）。
const LATENCY_MAX_MS: i64 = 120 * 1000;
/// 推不出耗时的行（文件里的第一条记录）先记 0，回填一遍后置为这个哨兵，避免每次同步重扫。
const LATENCY_UNKNOWN_MS: i64 = 1;

fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 6 {
        return;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) == Some("tool-results") {
                continue;
            }
            collect(&path, out, depth + 1);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
            if name.ends_with(".file-rollback.ndjson") || name.starts_with('.') {
                continue;
            }
            out.push(path);
        }
    }
}

pub fn sync_workbuddy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let root = workbuddy_config::data_dir().join("projects");
    if !root.is_dir() {
        return Ok(SessionSyncResult::default());
    }

    let mut result = SessionSyncResult::default();
    let mut conn = lock_conn!(db.conn);
    let tx = conn.transaction()?;
    import_root(&tx, &root, &mut result);
    tx.commit()?;
    Ok(result)
}

/// Walk every transcript under `root` and import the new tail of each file.
fn import_root(
    tx: &rusqlite::Transaction<'_>,
    root: &Path,
    result: &mut SessionSyncResult,
) {
    let mut files = Vec::new();
    collect(root, &mut files, 0);
    files.sort();
    for path in files {
        result.files_scanned += 1;
        if let Err(error) = import_file(tx, &path, result) {
            result.errors.push(format!("{}: {error}", path.display()));
        }
    }
    if let Err(error) = backfill_latency(tx, root) {
        result.errors.push(format!("latency backfill: {error}"));
    }
}

fn import_file(
    tx: &rusqlite::Transaction<'_>,
    path: &Path,
    result: &mut SessionSyncResult,
) -> Result<(), AppError> {
    let key = format!("workbuddy:{}", path.display());
    let meta = fs::metadata(path).map_err(|error| AppError::io(path, error))?;
    let size = meta.len();
    let mut offset = tx
        .query_row(
            "SELECT COALESCE(last_byte_offset, 0) FROM session_log_sync WHERE file_path = ?1",
            [&key],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u64;
    // Rewritten / truncated transcript: start over. Rows are de-duplicated by
    // request_id, so a replay cannot double count.
    if offset > size {
        offset = 0;
    }
    if offset == size {
        return Ok(());
    }

    let file = File::open(path).map_err(|error| AppError::io(path, error))?;
    let mut reader = BufReader::new(file);
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|error| AppError::io(path, error))?;

    let mut consumed = offset;
    let mut line = String::new();
    // Where this turn's generation started: the previous tool result (or the
    // previous turn's usage line when no tool ran in between). Reasoning /
    // message lines are written milliseconds before the usage line, so they
    // must NOT move the anchor — that was reading as 14 ms of "generation".
    let mut anchor_ts: Option<i64> = None;
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|error| AppError::io(path, error))?;
        if read == 0 {
            break;
        }
        // A trailing line without a newline may still be being written; leave it
        // for the next sync instead of parsing a half record.
        if !line.ends_with('\n') {
            break;
        }
        consumed += read as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match record_row(tx, trimmed, anchor_ts) {
            Ok((inserted, ts, is_anchor)) => {
                if inserted {
                    result.imported += 1;
                } else {
                    result.skipped += 1;
                }
                if is_anchor {
                    anchor_ts = ts;
                }
            }
            Err(_) => result.skipped += 1,
        }
    }

    tx.execute(
        "INSERT INTO session_log_sync (file_path, last_modified, last_line_offset, last_synced_at, last_byte_offset)
         VALUES (?1, ?2, 0, unixepoch(), ?3)
         ON CONFLICT(file_path) DO UPDATE SET
            last_modified = excluded.last_modified,
            last_synced_at = excluded.last_synced_at,
            last_byte_offset = excluded.last_byte_offset",
        params![key, meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0), consumed as i64],
    )?;
    Ok(())
}

/// Insert one transcript line.
///
/// Returns `(inserted, timestamp, moves_anchor)`. `moves_anchor` is true for the
/// records a turn starts counting from: tool results and usage lines.
fn record_row(
    tx: &rusqlite::Transaction<'_>,
    line: &str,
    anchor_ts: Option<i64>,
) -> Result<(bool, Option<i64>, bool), AppError> {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return Ok((false, None, false)),
    };
    let ts = value.get("timestamp").and_then(Value::as_i64);
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let is_tool_result = record_type == "function_call_result";
    let provider = value.get("providerData");
    let usage = provider
        .and_then(|data| data.get("usage"))
        .filter(|usage| usage.is_object());
    let usage = match usage {
        Some(usage) => usage,
        None => return Ok((false, ts, is_tool_result)),
    };

    let input_tokens = int_field(usage, "inputTokens");
    let output_tokens = int_field(usage, "outputTokens");
    if input_tokens == 0 && output_tokens == 0 {
        return Ok((false, ts, is_tool_result));
    }
    let cached = usage
        .get("inputTokensDetails")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| int_field(item, "cached_tokens"))
                .sum::<u32>()
        })
        .unwrap_or(0)
        .min(input_tokens);

    let model = provider
        .and_then(|data| data.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let created_at = ts.map(|ms| ms / 1000).unwrap_or(0);
    // Estimated generation time: from the previous tool result (or previous
    // turn) up to this usage line. Includes thinking and first-token wait.
    let latency_ms = match (anchor_ts, ts) {
        (Some(prev), Some(now)) if now > prev => {
            let gap = now - prev;
            if gap > LATENCY_MAX_MS {
                LATENCY_UNKNOWN_MS
            } else {
                gap
            }
        }
        _ => 0,
    };
    let request_id = request_id_of(&value, line);

    let token_usage = TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_tokens: cached,
        cache_creation_tokens: 0,
        cache_creation_1h_tokens: 0,
        model: Some(model.clone()),
        message_id: None,
    };
    let cost = find_model_pricing(tx, &model)
        .map(|pricing| {
            CostCalculator::calculate_for_app("workbuddy", &token_usage, &pricing, Decimal::ONE)
                .total_cost
                .to_string()
        })
        .unwrap_or_else(|| "0".into());

    let changed = tx.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd,
            total_cost_usd, latency_ms, status_code, session_id, provider_type,
            is_streaming, cost_multiplier, created_at, data_source, input_token_semantics
         ) VALUES (?1, '_workbuddy_session', 'workbuddy', ?2, ?2, ?3, ?4, ?5, 0,
                   '0', '0', '0', '0', ?6, ?10, 200, ?7, 'workbuddy_session',
                   1, '1', ?8, 'workbuddy_session', ?9)",
        params![
            request_id,
            model,
            input_tokens,
            output_tokens,
            cached,
            cost,
            session_id,
            created_at,
            INPUT_TOKEN_SEMANTICS_TOTAL,
            latency_ms
        ],
    )?;
    Ok((changed > 0, ts, true))
}

fn request_id_of(value: &Value, line: &str) -> String {
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let message_key = value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fingerprint(line));
    format!("workbuddy:{session_id}:{message_key}")
}

/// 早期导入的行耗时硬编码为 0（当时没有估算逻辑）。这里重扫一遍转录补齐，补不上的
/// 置哨兵，保证每个库只重扫一次。只在确实存在 latency_ms = 0 的行时才跑。
fn backfill_latency(tx: &rusqlite::Transaction<'_>, root: &Path) -> Result<u64, AppError> {
    let pending: i64 = tx.query_row(
        "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'workbuddy' AND latency_ms = 0",
        [],
        |row| row.get(0),
    )?;
    if pending == 0 {
        return Ok(0);
    }

    let mut files = Vec::new();
    collect(root, &mut files, 0);
    let mut latencies: HashMap<String, i64> = HashMap::new();
    for path in files {
        let _ = scan_latencies(&path, &mut latencies);
    }

    let mut updated = 0u64;
    for (request_id, latency) in latencies {
        updated += tx.execute(
            "UPDATE proxy_request_logs SET latency_ms = ?1
             WHERE request_id = ?2 AND app_type = 'workbuddy' AND latency_ms = 0",
            params![latency, request_id],
        )? as u64;
    }
    tx.execute(
        "UPDATE proxy_request_logs SET latency_ms = ?1 WHERE app_type = 'workbuddy' AND latency_ms = 0",
        params![LATENCY_UNKNOWN_MS],
    )?;
    Ok(updated)
}

fn scan_latencies(path: &Path, out: &mut HashMap<String, i64>) -> Result<(), AppError> {
    let file = File::open(path).map_err(|error| AppError::io(path, error))?;
    let mut anchor: Option<i64> = None;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let ts = value.get("timestamp").and_then(Value::as_i64);
        let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let is_tool_result = record_type == "function_call_result";
        let has_usage = value
            .get("providerData")
            .and_then(|data| data.get("usage"))
            .is_some_and(|usage| usage.is_object());
        if has_usage {
            if let (Some(previous), Some(now)) = (anchor, ts) {
                let gap = now - previous;
                if gap > 0 && gap <= LATENCY_MAX_MS {
                    out.insert(request_id_of(&value, &line), gap);
                }
            }
        }
        if has_usage || is_tool_result {
            if ts.is_some() {
                anchor = ts;
            }
        }
    }
    Ok(())
}

fn int_field(value: &Value, name: &str) -> u32 {
    value
        .get(name)
        .and_then(Value::as_i64)
        .filter(|number| *number > 0)
        .map(|number| number.min(u32::MAX as i64) as u32)
        .unwrap_or(0)
}

fn fingerprint(line: &str) -> String {
    let mut hasher = DefaultHasher::new();
    line.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "CREATE TABLE proxy_request_logs (
                request_id TEXT PRIMARY KEY,
                provider_id TEXT NOT NULL,
                app_type TEXT NOT NULL,
                model TEXT NOT NULL,
                request_model TEXT,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cache_creation_tokens INTEGER NOT NULL,
                input_cost_usd TEXT NOT NULL,
                output_cost_usd TEXT NOT NULL,
                cache_read_cost_usd TEXT NOT NULL,
                cache_creation_cost_usd TEXT NOT NULL,
                total_cost_usd TEXT NOT NULL,
                latency_ms INTEGER,
                status_code INTEGER,
                session_id TEXT,
                provider_type TEXT,
                is_streaming INTEGER,
                cost_multiplier TEXT,
                created_at INTEGER,
                data_source TEXT,
                input_token_semantics INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
    }

    #[test]
    fn parses_usage_and_dedupes() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema(&conn);
        let tx = conn.transaction().unwrap();
        let line = json!({
            "id": "msg-1",
            "sessionId": "s1",
            "timestamp": 1793187921885_i64,
            "providerData": {
                "model": "deepseek-v4.1-flash",
                "usage": {
                    "inputTokens": 1000,
                    "outputTokens": 50,
                    "inputTokensDetails": [{"cached_tokens": 800}]
                }
            }
        })
        .to_string();
        assert!(record_row(&tx, &line, None).unwrap().0);
        assert!(
            !record_row(&tx, &line, None).unwrap().0,
            "same id must not double count"
        );
        let (input, output, cache, semantics, created): (i64, i64, i64, i64, i64) = tx
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, input_token_semantics, created_at
                 FROM proxy_request_logs",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .unwrap();
        assert_eq!((input, output, cache), (1000, 50, 800));
        assert_eq!(semantics, INPUT_TOKEN_SEMANTICS_TOTAL);
        assert_eq!(created, 1793187921);
    }

    fn drift_schema(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "CREATE TABLE session_log_sync (
                file_path TEXT PRIMARY KEY,
                last_modified INTEGER NOT NULL,
                last_line_offset INTEGER NOT NULL DEFAULT 0,
                last_synced_at INTEGER NOT NULL,
                last_byte_offset INTEGER,
                last_tail_fingerprint INTEGER
            );
            CREATE TABLE model_pricing (
                model_id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                input_cost_per_million TEXT NOT NULL,
                output_cost_per_million TEXT NOT NULL,
                cache_read_cost_per_million TEXT NOT NULL DEFAULT '0',
                cache_creation_cost_per_million TEXT NOT NULL DEFAULT '0'
            );",
        )
        .unwrap();
    }

    fn sample_line(id: &str, input: i64, output: i64, cached: i64) -> String {
        json!({
            "id": id,
            "sessionId": "sess-1",
            "timestamp": 1793187921885_i64,
            "providerData": {
                "model": "deepseek-v4.1-flash",
                "usage": {
                    "inputTokens": input,
                    "outputTokens": output,
                    "inputTokensDetails": [{"cached_tokens": cached}]
                }
            }
        })
        .to_string()
    }

    #[test]
    fn imports_files_incrementally_and_is_idempotent() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema(&conn);
        drift_schema(&conn);

        let root = std::env::temp_dir().join(format!(
            "workbuddy-usage-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let ws = root.join("Users-test-WorkBuddy-2026-10-31");
        let sub = ws.join("sess-1").join("subagents");
        fs::create_dir_all(&sub).unwrap();
        fs::write(ws.join("sess-1.jsonl"), format!("{}\n", sample_line("m1", 1000, 10, 800))).unwrap();
        fs::write(sub.join("agent-a.jsonl"), format!("{}\n", sample_line("m2", 500, 5, 0))).unwrap();

        let mut first = SessionSyncResult::default();
        {
            let tx = conn.transaction().unwrap();
            import_root(&tx, &root, &mut first);
            tx.commit().unwrap();
        }
        assert_eq!(first.imported, 2, "errors: {:?}", first.errors);

        // 第二次同步：文件没变，不该重复导入
        let mut second = SessionSyncResult::default();
        {
            let tx = conn.transaction().unwrap();
            import_root(&tx, &root, &mut second);
            tx.commit().unwrap();
        }
        assert_eq!(second.imported, 0);

        // 追加一行后只导入新增的那行
        let mut handle = fs::OpenOptions::new()
            .append(true)
            .open(ws.join("sess-1.jsonl"))
            .unwrap();
        use std::io::Write;
        writeln!(handle, "{}", sample_line("m3", 100, 1, 0)).unwrap();
        drop(handle);
        let mut third = SessionSyncResult::default();
        {
            let tx = conn.transaction().unwrap();
            import_root(&tx, &root, &mut third);
            tx.commit().unwrap();
        }
        assert_eq!(third.imported, 1);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_request_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let sources: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'workbuddy' AND data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sources, 3);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn estimates_latency_from_previous_record() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema(&conn);
        let tx = conn.transaction().unwrap();
        let first = json!({
            "id": "m1",
            "sessionId": "s1",
            "timestamp": 1_000_000_i64,
            "providerData": {"model": "m", "usage": {"inputTokens": 10, "outputTokens": 5}}
        })
        .to_string();
        let second = json!({
            "id": "m2",
            "sessionId": "s1",
            "timestamp": 1_007_500_i64,
            "providerData": {"model": "m", "usage": {"inputTokens": 10, "outputTokens": 5}}
        })
        .to_string();

        let (inserted, ts, moves) = record_row(&tx, &first, None).unwrap();
        assert!(inserted && moves);
        assert_eq!(ts, Some(1_000_000));
        // 首条没有上一条记录，耗时未知
        assert_eq!(latency_of(&tx, "workbuddy:s1:m1"), 0);

        let (inserted, ts, moves) = record_row(&tx, &second, ts).unwrap();
        assert!(inserted && moves);
        assert_eq!(ts, Some(1_007_500));
        assert_eq!(latency_of(&tx, "workbuddy:s1:m2"), 7_500);
    }

    fn latency_of(tx: &rusqlite::Transaction<'_>, request_id: &str) -> i64 {
        tx.query_row(
            "SELECT latency_ms FROM proxy_request_logs WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn tool_result_is_the_generation_anchor() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema(&conn);
        let tx = conn.transaction().unwrap();
        // 上一轮 usage 在 1_000_000，工具返回在 1_003_000，本轮 usage 在 1_010_000：
        // 生成耗时应当从工具返回算起（7000ms），而不是从上一行（reasoning）算。
        let tool_result = json!({
            "type": "function_call_result",
            "timestamp": 1_003_000_i64,
            "sessionId": "s1"
        })
        .to_string();
        let reasoning = json!({
            "type": "reasoning",
            "timestamp": 1_009_985_i64,
            "sessionId": "s1"
        })
        .to_string();
        let usage = json!({
            "id": "m9",
            "sessionId": "s1",
            "timestamp": 1_010_000_i64,
            "type": "function_call",
            "providerData": {"model": "m", "usage": {"inputTokens": 10, "outputTokens": 300}}
        })
        .to_string();

        let (_, ts, moves) = record_row(&tx, &tool_result, None).unwrap();
        assert!(moves, "tool result moves the anchor");
        let (_, ts2, moves2) = record_row(&tx, &reasoning, ts).unwrap();
        assert!(!moves2, "reasoning must not move the anchor");
        assert_eq!(ts2, Some(1_009_985));
        let (inserted, _, _) = record_row(&tx, &usage, ts).unwrap();
        assert!(inserted);
        assert_eq!(latency_of(&tx, "workbuddy:s1:m9"), 7_000);
    }

    #[test]
    fn ignores_lines_without_usage() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        schema(&conn);
        let tx = conn.transaction().unwrap();
        assert!(!record_row(&tx, r#"{"id":"x","type":"reasoning"}"#, None).unwrap().0);
        assert!(!record_row(&tx, "not json", None).unwrap().0);
    }
}
