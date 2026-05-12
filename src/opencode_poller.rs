use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::diagnose;
use crate::models::{OpenCodeUsageWindows, TokenWindowUsage, UsageData};

#[derive(Debug)]
pub enum PollError {
    NoDatabase,
    UnsupportedSchema,
    QueryFailed,
}

#[derive(Clone, Debug)]
struct TableInfo {
    name: String,
    columns: Vec<ColumnInfo>,
}

#[derive(Clone, Debug)]
struct ColumnInfo {
    name: String,
}

#[derive(Clone, Debug)]
struct CandidateTable {
    table: String,
    timestamp_column: String,
    data_column: Option<String>,
    token_columns: TokenColumnSet,
    score: i32,
}

#[derive(Clone, Debug, Default)]
struct TokenColumnSet {
    input: Option<String>,
    output: Option<String>,
    reasoning: Option<String>,
    cache_read: Option<String>,
    cache_write: Option<String>,
    cost: Option<String>,
}

#[derive(Clone, Debug)]
struct UsageRow {
    timestamp_ms: i64,
    usage: TokenWindowUsage,
}

struct WindowCutoffs {
    five_hour_ms: i64,
    today_ms: i64,
    month_ms: i64,
}

pub fn poll() -> Result<UsageData, PollError> {
    let path = resolve_database_path().ok_or_else(|| {
        diagnose::log("OpenCode database not found under LOCALAPPDATA\\opencode");
        PollError::NoDatabase
    })?;

    let windows = aggregate_from_path(&path, SystemTime::now())?;
    Ok(usage_data_from_windows(windows, &path))
}

fn usage_data_from_windows(windows: OpenCodeUsageWindows, path: &Path) -> UsageData {
    let mut data = UsageData::default();
    data.session.percentage = 0.0;
    data.weekly.percentage = 0.0;
    data.source_label = Some(format!("opencode-db:{}", path.display()));
    data.updated_at = Some(SystemTime::now());
    data.opencode_windows = Some(windows);
    data
}

fn resolve_database_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OPENCODE_DB") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() && trimmed != ":memory:" {
            let path = PathBuf::from(trimmed);
            if path.is_file() {
                return Some(path);
            }
        }
    }

    let mut candidates = Vec::new();
    for opencode_dir in candidate_data_dirs() {
        let default = opencode_dir.join("opencode.db");
        if default.is_file() {
            return Some(default);
        }

        if let Ok(entries) = std::fs::read_dir(opencode_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                if name.starts_with("opencode-") && name.ends_with(".db") && path.is_file() {
                    candidates.push(path);
                }
            }
        }
    }
    candidates.sort();
    candidates.into_iter().next()
}

fn candidate_data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local_app_data).join("opencode"));
    }
    if let Some(local_data_dir) = dirs::data_local_dir() {
        dirs.push(local_data_dir.join("opencode"));
    }
    if let Ok(app_data) = std::env::var("APPDATA") {
        dirs.push(PathBuf::from(app_data).join("opencode"));
    }
    if let Some(data_dir) = dirs::data_dir() {
        dirs.push(data_dir.join("opencode"));
    }
    if let Some(home_dir) = dirs::home_dir() {
        dirs.push(home_dir.join(".local").join("share").join("opencode"));
    }

    dirs.dedup();
    dirs
}

fn aggregate_from_path(path: &Path, now: SystemTime) -> Result<OpenCodeUsageWindows, PollError> {
    let conn = open_readonly(path)?;
    aggregate_from_connection(&conn, now)
}

fn open_readonly(path: &Path) -> Result<Connection, PollError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(|error| {
        diagnose::log(format!(
            "OpenCode read-only database open failed for {}: {error}",
            path.display()
        ));
        PollError::NoDatabase
    })?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|error| {
            diagnose::log(format!("OpenCode query_only pragma failed: {error}"));
            PollError::QueryFailed
        })?;
    conn.busy_timeout(Duration::from_millis(750))
        .map_err(|error| {
            diagnose::log(format!("OpenCode busy_timeout setup failed: {error}"));
            PollError::QueryFailed
        })?;
    Ok(conn)
}

fn aggregate_from_connection(
    conn: &Connection,
    now: SystemTime,
) -> Result<OpenCodeUsageWindows, PollError> {
    let schema = discover_schema(conn)?;
    log_schema(&schema);
    let candidates = discover_candidates(&schema);
    if candidates.is_empty() {
        diagnose::log("OpenCode schema autodiscovery found no usable token/timestamp tables");
        return Err(PollError::UnsupportedSchema);
    }

    let now_ms = system_time_to_ms(now);
    let cutoffs = WindowCutoffs {
        five_hour_ms: now_ms.saturating_sub(5 * 60 * 60 * 1000),
        today_ms: start_of_utc_day_ms(now_ms),
        month_ms: start_of_utc_month_ms(now_ms),
    };

    let mut best_error = false;
    for candidate in candidates {
        match read_candidate_rows(conn, &candidate, cutoffs.month_ms) {
            Ok(rows) => {
                diagnose::log(format!(
                    "OpenCode candidate {} score {} yielded {} usage rows",
                    candidate.table,
                    candidate.score,
                    rows.len()
                ));
                if rows.is_empty() {
                    continue;
                }

                let mut windows = OpenCodeUsageWindows::default();
                for row in rows {
                    if row.timestamp_ms >= cutoffs.five_hour_ms {
                        windows.five_hour.add(&row.usage);
                    }
                    if row.timestamp_ms >= cutoffs.today_ms {
                        windows.today.add(&row.usage);
                    }
                    if row.timestamp_ms >= cutoffs.month_ms {
                        windows.month.add(&row.usage);
                    }
                }
                return Ok(windows);
            }
            Err(error) => {
                best_error = true;
                diagnose::log(format!(
                    "OpenCode candidate {} score {} query failed: {error}",
                    candidate.table, candidate.score
                ));
            }
        }
    }

    if best_error {
        Err(PollError::QueryFailed)
    } else {
        diagnose::log("OpenCode schema autodiscovery found tables but no usable token rows");
        Err(PollError::UnsupportedSchema)
    }
}

fn discover_schema(conn: &Connection) -> Result<Vec<TableInfo>, PollError> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .map_err(|error| {
            diagnose::log(format!(
                "OpenCode sqlite_master query prepare failed: {error}"
            ));
            PollError::QueryFailed
        })?;
    let table_names = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| {
            diagnose::log(format!("OpenCode sqlite_master query failed: {error}"));
            PollError::QueryFailed
        })?;

    let mut tables = Vec::new();
    for table_name in table_names.flatten() {
        let columns = read_columns(conn, &table_name)?;
        tables.push(TableInfo {
            name: table_name,
            columns,
        });
    }
    Ok(tables)
}

fn read_columns(conn: &Connection, table: &str) -> Result<Vec<ColumnInfo>, PollError> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")
        .map_err(|error| {
            diagnose::log(format!(
                "OpenCode pragma_table_info prepare failed: {error}"
            ));
            PollError::QueryFailed
        })?;
    let rows = stmt.query_map([table], |row| Ok(ColumnInfo { name: row.get(0)? }));
    let rows = rows.map_err(|error| {
        diagnose::log(format!(
            "OpenCode pragma_table_info failed for {table}: {error}"
        ));
        PollError::QueryFailed
    })?;

    Ok(rows.flatten().collect())
}

fn log_schema(schema: &[TableInfo]) {
    let summary = schema
        .iter()
        .map(|table| {
            let cols = table
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>()
                .join(",");
            format!("{}({cols})", table.name)
        })
        .collect::<Vec<_>>()
        .join("; ");
    diagnose::log(format!("OpenCode schema: {summary}"));
}

fn discover_candidates(schema: &[TableInfo]) -> Vec<CandidateTable> {
    let mut candidates = Vec::new();
    for table in schema {
        let timestamp_column = find_column(
            &table.columns,
            &[
                "time_created",
                "created_at",
                "timestamp",
                "ts",
                "created",
                "time",
            ],
        );
        let data_column = find_column(&table.columns, &["data", "json", "payload", "message"]);
        let token_columns = TokenColumnSet {
            input: find_column(&table.columns, &["input_tokens", "prompt_tokens", "input"]),
            output: find_column(
                &table.columns,
                &["output_tokens", "completion_tokens", "output"],
            ),
            reasoning: find_column(
                &table.columns,
                &["reasoning_output_tokens", "reasoning_tokens", "reasoning"],
            ),
            cache_read: find_column(
                &table.columns,
                &["cache_read_tokens", "cached_input_tokens", "cache_read"],
            ),
            cache_write: find_column(&table.columns, &["cache_write_tokens", "cache_write"]),
            cost: find_column(&table.columns, &["cost", "estimated_cost_usd", "cost_usd"]),
        };

        if let Some(timestamp_column) = timestamp_column {
            let has_json = data_column.is_some();
            let has_direct_tokens = token_columns.input.is_some()
                || token_columns.output.is_some()
                || token_columns.reasoning.is_some()
                || token_columns.cache_read.is_some()
                || token_columns.cache_write.is_some();

            if has_json || has_direct_tokens {
                candidates.push(CandidateTable {
                    table: table.name.clone(),
                    timestamp_column,
                    data_column,
                    token_columns,
                    score: candidate_score(&table.name, has_json, has_direct_tokens),
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then(left.table.cmp(&right.table))
    });
    candidates
}

fn candidate_score(table_name: &str, has_json: bool, has_direct_tokens: bool) -> i32 {
    let normalized = table_name.to_ascii_lowercase();
    let mut score = 0;
    if normalized == "part" {
        score += 120;
    } else if normalized == "message" {
        score += 100;
    } else if normalized == "session_message" {
        score += 90;
    } else if normalized.contains("message") {
        score += 50;
    } else if normalized.contains("usage") || normalized.contains("token") {
        score += 40;
    }
    if has_json {
        score += 20;
    }
    if has_direct_tokens {
        score += 10;
    }
    score
}

fn find_column(columns: &[ColumnInfo], names: &[&str]) -> Option<String> {
    for wanted in names {
        if let Some(column) = columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(wanted))
        {
            return Some(column.name.clone());
        }
    }
    None
}

fn read_candidate_rows(
    conn: &Connection,
    candidate: &CandidateTable,
    cutoff_ms: i64,
) -> rusqlite::Result<Vec<UsageRow>> {
    let mut select_columns = vec![candidate.timestamp_column.clone()];
    if let Some(data_column) = candidate.data_column.as_ref() {
        select_columns.push(data_column.clone());
    }
    for column in candidate.token_columns.iter() {
        if !select_columns.iter().any(|existing| existing == &column) {
            select_columns.push(column);
        }
    }

    let sql = format!(
        "SELECT {} FROM {} WHERE {} >= ?1 ORDER BY {} ASC",
        select_columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        quote_identifier(&candidate.table),
        quote_identifier(&candidate.timestamp_column),
        quote_identifier(&candidate.timestamp_column)
    );
    let query_cutoff = cutoff_for_query(cutoff_ms);
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([query_cutoff])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let raw_timestamp = read_i64(row.get_ref(0)?).unwrap_or_default();
        let timestamp_ms = normalize_timestamp_ms(raw_timestamp);
        if timestamp_ms < cutoff_ms {
            continue;
        }

        let mut usage = TokenWindowUsage::default();
        if let Some(data_column) = candidate.data_column.as_ref() {
            if let Some(index) = select_columns
                .iter()
                .position(|column| column == data_column)
            {
                if let Some(json_text) = read_string(row.get_ref(index)?) {
                    if let Ok(value) = serde_json::from_str::<Value>(&json_text) {
                        usage.add(&extract_usage_from_json(&value));
                    }
                }
            }
        }

        usage.add(&extract_usage_from_columns(
            row,
            &select_columns,
            &candidate.token_columns,
        )?);
        if usage.total_tokens() > 0 || usage.estimated_cost_usd > 0.0 {
            result.push(UsageRow {
                timestamp_ms,
                usage,
            });
        }
    }

    Ok(result)
}

impl TokenColumnSet {
    fn iter(&self) -> Vec<String> {
        [
            self.input.as_ref(),
            self.output.as_ref(),
            self.reasoning.as_ref(),
            self.cache_read.as_ref(),
            self.cache_write.as_ref(),
            self.cost.as_ref(),
        ]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
    }
}

fn extract_usage_from_columns(
    row: &rusqlite::Row<'_>,
    select_columns: &[String],
    token_columns: &TokenColumnSet,
) -> rusqlite::Result<TokenWindowUsage> {
    Ok(TokenWindowUsage {
        input_tokens: read_u64_column(row, select_columns, token_columns.input.as_deref())?,
        output_tokens: read_u64_column(row, select_columns, token_columns.output.as_deref())?,
        reasoning_output_tokens: read_u64_column(
            row,
            select_columns,
            token_columns.reasoning.as_deref(),
        )?,
        cached_input_tokens: read_u64_column(
            row,
            select_columns,
            token_columns.cache_read.as_deref(),
        )?
        .saturating_add(read_u64_column(
            row,
            select_columns,
            token_columns.cache_write.as_deref(),
        )?),
        estimated_cost_usd: read_f64_column(row, select_columns, token_columns.cost.as_deref())?,
        ..Default::default()
    })
}

fn read_u64_column(
    row: &rusqlite::Row<'_>,
    columns: &[String],
    column: Option<&str>,
) -> rusqlite::Result<u64> {
    let Some(column) = column else { return Ok(0) };
    let Some(index) = columns.iter().position(|value| value == column) else {
        return Ok(0);
    };
    Ok(read_u64(row.get_ref(index)?).unwrap_or(0))
}

fn read_f64_column(
    row: &rusqlite::Row<'_>,
    columns: &[String],
    column: Option<&str>,
) -> rusqlite::Result<f64> {
    let Some(column) = column else { return Ok(0.0) };
    let Some(index) = columns.iter().position(|value| value == column) else {
        return Ok(0.0);
    };
    Ok(read_f64(row.get_ref(index)?).unwrap_or(0.0))
}

fn extract_usage_from_json(value: &Value) -> TokenWindowUsage {
    let mut usage = TokenWindowUsage::default();
    collect_usage_objects(value, &mut usage);
    usage
}

fn collect_usage_objects(value: &Value, usage: &mut TokenWindowUsage) {
    match value {
        Value::Object(map) => {
            if let Some(tokens) = map.get("tokens") {
                usage.input_tokens = usage
                    .input_tokens
                    .saturating_add(json_u64_path(tokens, &["input"]));
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(json_u64_path(tokens, &["output"]));
                usage.reasoning_output_tokens = usage
                    .reasoning_output_tokens
                    .saturating_add(json_u64_path(tokens, &["reasoning"]));
                usage.cached_input_tokens = usage
                    .cached_input_tokens
                    .saturating_add(json_u64_path(tokens, &["cache", "read"]))
                    .saturating_add(json_u64_path(tokens, &["cache", "write"]));

                let cost = map.get("cost").and_then(json_f64).unwrap_or(0.0);
                usage.estimated_cost_usd += cost;
                if cost <= 0.0 {
                    usage.unpriced_rows = usage.unpriced_rows.saturating_add(1);
                }
            }

            for child in map.values() {
                collect_usage_objects(child, usage);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_usage_objects(child, usage);
            }
        }
        _ => {}
    }
}

fn json_u64_path(value: &Value, path: &[&str]) -> u64 {
    let mut current = value;
    for key in path {
        let Some(next) = current.get(*key) else {
            return 0;
        };
        current = next;
    }
    json_u64(current).unwrap_or(0)
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_f64().and_then(f64_to_u64))
        .or_else(|| value.as_str().and_then(|text| text.parse::<u64>().ok()))
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|number| number as f64))
        .or_else(|| value.as_u64().map(|number| number as f64))
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
}

fn read_i64(value: ValueRef<'_>) -> Option<i64> {
    match value {
        ValueRef::Integer(value) => Some(value),
        ValueRef::Real(value) => Some(value as i64),
        ValueRef::Text(value) => std::str::from_utf8(value).ok()?.parse::<i64>().ok(),
        _ => None,
    }
}

fn read_u64(value: ValueRef<'_>) -> Option<u64> {
    match value {
        ValueRef::Integer(value) => u64::try_from(value).ok(),
        ValueRef::Real(value) => f64_to_u64(value),
        ValueRef::Text(value) => std::str::from_utf8(value).ok()?.parse::<u64>().ok(),
        _ => None,
    }
}

fn read_f64(value: ValueRef<'_>) -> Option<f64> {
    match value {
        ValueRef::Integer(value) => Some(value as f64),
        ValueRef::Real(value) => Some(value),
        ValueRef::Text(value) => std::str::from_utf8(value).ok()?.parse::<f64>().ok(),
        _ => None,
    }
}

fn read_string(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Text(value) => Some(std::str::from_utf8(value).ok()?.to_string()),
        ValueRef::Blob(value) => Some(String::from_utf8_lossy(value).to_string()),
        _ => None,
    }
}

fn f64_to_u64(value: f64) -> Option<u64> {
    if value.is_finite() && value >= 0.0 && value <= u64::MAX as f64 {
        Some(value as u64)
    } else {
        None
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn cutoff_for_query(cutoff_ms: i64) -> i64 {
    cutoff_ms / 1000
}

fn normalize_timestamp_ms(value: i64) -> i64 {
    if value > 10_000_000_000_000 {
        value / 1000
    } else if value > 10_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}

fn system_time_to_ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn start_of_utc_day_ms(now_ms: i64) -> i64 {
    let day_ms = 86_400_000;
    now_ms.saturating_div(day_ms).saturating_mul(day_ms)
}

fn start_of_utc_month_ms(now_ms: i64) -> i64 {
    let days = now_ms.saturating_div(86_400_000);
    let (year, month, _) = civil_from_days(days);
    days_from_civil(year, month, 1).saturating_mul(86_400_000)
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_official_json_message_shape() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute(
            "CREATE TABLE message (id TEXT, time_created INTEGER NOT NULL, data TEXT NOT NULL)",
            [],
        )
        .expect("create message table");
        conn.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            (
                "m1",
                1_700_000_000_000i64,
                r#"{"role":"assistant","tokens":{"input":100,"output":50,"reasoning":25,"cache":{"read":10,"write":5}},"cost":0.0123}"#,
            ),
        )
        .expect("insert row");

        let windows =
            aggregate_from_connection(&conn, UNIX_EPOCH + Duration::from_millis(1_700_000_100_000))
                .expect("aggregate");

        assert_eq!(windows.five_hour.input_tokens, 100);
        assert_eq!(windows.five_hour.output_tokens, 50);
        assert_eq!(windows.five_hour.reasoning_output_tokens, 25);
        assert_eq!(windows.five_hour.cached_input_tokens, 15);
        assert!(windows.five_hour.estimated_cost_usd > 0.012);
    }

    #[test]
    fn normalizes_second_timestamps() {
        assert_eq!(normalize_timestamp_ms(1_700_000_000), 1_700_000_000_000);
    }

    #[test]
    fn aggregates_direct_token_columns() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute(
            "CREATE TABLE usage (time_created INTEGER NOT NULL, input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER, cached_input_tokens INTEGER, cost REAL)",
            [],
        )
        .expect("create usage table");
        conn.execute(
            "INSERT INTO usage (time_created, input_tokens, output_tokens, reasoning_tokens, cached_input_tokens, cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (1_700_000_000i64, 11i64, 22i64, 33i64, 44i64, 0.5f64),
        )
        .expect("insert usage row");

        let windows =
            aggregate_from_connection(&conn, UNIX_EPOCH + Duration::from_millis(1_700_000_100_000))
                .expect("aggregate direct columns");

        assert_eq!(windows.five_hour.total_tokens(), 110);
        assert!(windows.five_hour.estimated_cost_usd > 0.49);
    }

    #[test]
    fn prefers_step_finish_part_table_when_message_table_also_has_tokens() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute(
            "CREATE TABLE message (id TEXT, time_created INTEGER NOT NULL, data TEXT NOT NULL)",
            [],
        )
        .expect("create message table");
        conn.execute(
            "CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER NOT NULL, data TEXT NOT NULL)",
            [],
        )
        .expect("create part table");
        conn.execute(
            "INSERT INTO message (id, time_created, data) VALUES (?1, ?2, ?3)",
            (
                "m1",
                1_700_000_000_000i64,
                r#"{"role":"assistant","tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}},"cost":0}"#,
            ),
        )
        .expect("insert message row");
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, time_created, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                "p1",
                "m1",
                "s1",
                1_700_000_000_000i64,
                r#"{"reason":"tool-calls","type":"step-finish","tokens":{"total":30,"input":10,"output":5,"reasoning":3,"cache":{"write":2,"read":10}},"cost":0}"#,
            ),
        )
        .expect("insert part row");

        let windows =
            aggregate_from_connection(&conn, UNIX_EPOCH + Duration::from_millis(1_700_000_100_000))
                .expect("aggregate preferred part table");

        assert_eq!(windows.five_hour.input_tokens, 10);
        assert_eq!(windows.five_hour.output_tokens, 5);
        assert_eq!(windows.five_hour.reasoning_output_tokens, 3);
        assert_eq!(windows.five_hour.cached_input_tokens, 12);
    }
}
