use std::collections::{HashMap, HashSet};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{
    HistorySnapshot, HistoryStorageMode, HistorySummary, HistorySyncSettings, ProjectUsageEntry,
    ProviderKind,
};
use crate::pricing;

pub const DEFAULT_ARCHIVE_REPO_URL: &str = "git@github.com:disc0nnctd/personal-token-usage.git";
pub const DEFAULT_WORKFLOW_FILE_NAME: &str = "build-history-report.yml";
const SAMPLE_WORKFLOW_YAML: &str = include_str!("../docs/build-history-report.sample.yml");

const HISTORY_CACHE_TTL: Duration = Duration::from_secs(60);
const HISTORY_STORE_VERSION: u32 = 1;
const LOCAL_REPORTS_TO_KEEP_AFTER_ARCHIVE: usize = 3;
const MAX_TOP_PROJECTS: usize = 10;

#[derive(Clone, Debug)]
struct CachedSnapshot {
    captured_at: SystemTime,
    snapshot: HistorySnapshot,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct HistoryStore {
    #[serde(default = "history_store_version")]
    version: u32,
    #[serde(default)]
    updated_at_unix: u64,
    #[serde(default)]
    sessions: Vec<StoredSessionRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct StoredSessionRecord {
    id: String,
    provider: ProviderKind,
    project_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_name: Option<String>,
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
    #[serde(default)]
    estimated_cost_usd: f64,
    #[serde(default)]
    priced: bool,
    #[serde(default)]
    last_seen_unix: u64,
}

#[derive(Clone, Debug, Default)]
struct ProjectAccumulator {
    display_path: String,
    sessions: u32,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    estimated_cost_usd: f64,
    unpriced_sessions: u32,
}

#[derive(Default)]
struct ClaudeSessionAggregate {
    project_path: Option<String>,
    model_name: Option<String>,
    input_tokens: u64,
    cache_write_5m_tokens: u64,
    cache_write_1h_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
}

#[derive(Deserialize)]
struct CodexRecord {
    #[serde(rename = "type")]
    record_type: String,
    payload: Value,
}

#[derive(Deserialize)]
struct CodexSessionMetaPayload {
    cwd: String,
}

#[derive(Deserialize)]
struct CodexTurnContextPayload {
    cwd: String,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize)]
struct CodexEventPayload {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    info: Option<CodexTokenInfo>,
}

#[derive(Deserialize)]
struct CodexTokenInfo {
    #[serde(default)]
    total_token_usage: CodexTokenTotals,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct CodexTokenTotals {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[derive(Deserialize)]
struct ClaudeProjectRecord {
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation: Option<ClaudeCacheCreation>,
}

#[derive(Deserialize)]
struct ClaudeCacheCreation {
    #[serde(default)]
    ephemeral_5m_input_tokens: u64,
    #[serde(default)]
    ephemeral_1h_input_tokens: u64,
}

#[derive(Clone, Debug)]
pub struct ArchiveResult {
    pub repo_url: String,
    pub cleaned_reports: usize,
    pub uploaded_conversation_files: usize,
    pub workflow_warning: Option<String>,
}

#[derive(Clone, Debug)]
pub struct FetchResult {
    pub repo_url: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct WorkflowInstallResult {
    pub location: String,
}

static HISTORY_CACHE: OnceLock<Mutex<Option<CachedSnapshot>>> = OnceLock::new();

pub fn read_snapshot(sync: &HistorySyncSettings, token: Option<&str>) -> HistorySnapshot {
    let cache = HISTORY_CACHE.get_or_init(|| Mutex::new(None));
    {
        let guard = cache.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached
                .captured_at
                .elapsed()
                .map(|elapsed| elapsed <= HISTORY_CACHE_TTL)
                .unwrap_or(false)
            {
                return cached.snapshot.clone();
            }
        }
    }

    let snapshot = collect_snapshot(sync, token);
    set_cached_snapshot(&snapshot);
    snapshot
}

pub fn write_report(sync: &HistorySyncSettings, token: Option<&str>) -> Result<PathBuf, String> {
    let reports_dir = reports_dir()?;
    std::fs::create_dir_all(&reports_dir)
        .map_err(|error| format!("Unable to create reports directory: {error}"))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = reports_dir.join(format!("usage-history-{timestamp}.html"));
    let snapshot = build_report_snapshot(sync, token);
    let html = build_html_report(&snapshot, timestamp);

    std::fs::write(&path, html)
        .map_err(|error| format!("Unable to write usage report: {error}"))?;

    Ok(path)
}

fn set_cached_snapshot(snapshot: &HistorySnapshot) {
    let cache = HISTORY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    *guard = Some(CachedSnapshot {
        captured_at: SystemTime::now(),
        snapshot: snapshot.clone(),
    });
}

fn clear_cached_snapshot() {
    let cache = HISTORY_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    *guard = None;
}

fn build_html_report(snapshot: &HistorySnapshot, generated_at_unix: u64) -> String {
    let mut html = String::new();
    let total_sessions = snapshot
        .claude
        .total_sessions
        .saturating_add(snapshot.codex.total_sessions);
    let total_projects = snapshot
        .claude
        .total_projects
        .saturating_add(snapshot.codex.total_projects);
    let total_input_tokens = snapshot
        .claude
        .input_tokens
        .saturating_add(snapshot.codex.input_tokens);
    let total_cached_tokens = snapshot
        .claude
        .cached_input_tokens
        .saturating_add(snapshot.codex.cached_input_tokens);
    let total_output_tokens = snapshot
        .claude
        .output_tokens
        .saturating_add(snapshot.codex.output_tokens);
    let total_reasoning_tokens = snapshot
        .claude
        .reasoning_output_tokens
        .saturating_add(snapshot.codex.reasoning_output_tokens);
    let total_cost = snapshot.claude.estimated_cost_usd + snapshot.codex.estimated_cost_usd;

    html.push_str(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Usage History Report</title>
<style>
:root {
    --paper: #f6f1e7;
    --paper-strong: #fffaf2;
    --ink: #231d17;
    --muted: #655d53;
    --line: rgba(35, 29, 23, 0.10);
    --shadow: 0 18px 48px rgba(58, 44, 28, 0.10);
    --claude: #b45d34;
    --claude-soft: #f3d5c4;
    --codex: #0d7b6b;
    --codex-soft: #c7e7df;
    --input: #c96c38;
    --cache: #d5a021;
    --output: #277da1;
    --reasoning: #7b5ea7;
}
* {
    box-sizing: border-box;
}
html, body {
    margin: 0;
    padding: 0;
    background:
        radial-gradient(circle at top left, rgba(201, 108, 56, 0.18), transparent 32%),
        radial-gradient(circle at top right, rgba(13, 123, 107, 0.16), transparent 28%),
        linear-gradient(180deg, #f8f3ea 0%, #f4ede1 100%);
    color: var(--ink);
    font-family: "Segoe UI Variable", "Aptos", "Trebuchet MS", sans-serif;
}
body {
    padding: 28px;
}
.shell {
    max-width: 1240px;
    margin: 0 auto;
}
.hero {
    background: rgba(255, 250, 242, 0.84);
    border: 1px solid rgba(255, 255, 255, 0.55);
    border-radius: 28px;
    box-shadow: var(--shadow);
    overflow: hidden;
}
.hero-top {
    padding: 30px 32px 20px;
    display: flex;
    gap: 18px;
    justify-content: space-between;
    align-items: end;
    flex-wrap: wrap;
}
.eyebrow {
    text-transform: uppercase;
    letter-spacing: 0.14em;
    font-size: 12px;
    font-weight: 700;
    color: var(--muted);
    margin-bottom: 10px;
}
h1 {
    margin: 0;
    font-size: clamp(32px, 5vw, 54px);
    line-height: 0.95;
    letter-spacing: -0.04em;
    max-width: 720px;
}
.hero-copy {
    margin: 14px 0 0;
    max-width: 780px;
    color: var(--muted);
    font-size: 15px;
    line-height: 1.55;
}
.stamp {
    min-width: 240px;
    padding: 18px 20px;
    border-radius: 20px;
    background: rgba(35, 29, 23, 0.04);
    border: 1px solid var(--line);
}
.stamp strong {
    display: block;
    font-size: 13px;
    text-transform: uppercase;
    letter-spacing: 0.10em;
    color: var(--muted);
    margin-bottom: 8px;
}
.stamp span {
    display: block;
    font-size: 15px;
    line-height: 1.5;
}
.kpi-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 1px;
    background: rgba(35, 29, 23, 0.06);
}
.kpi {
    padding: 22px 24px;
    background: rgba(255, 251, 245, 0.96);
}
.kpi-label {
    font-size: 12px;
    letter-spacing: 0.10em;
    text-transform: uppercase;
    color: var(--muted);
}
.kpi-value {
    margin-top: 6px;
    font-size: clamp(28px, 4vw, 36px);
    font-weight: 750;
    letter-spacing: -0.04em;
}
.kpi-note {
    margin-top: 6px;
    font-size: 13px;
    color: var(--muted);
}
.provider-grid {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 22px;
    margin-top: 26px;
}
.provider {
    background: rgba(255, 253, 249, 0.9);
    border: 1px solid rgba(255, 255, 255, 0.62);
    border-radius: 26px;
    box-shadow: var(--shadow);
    overflow: hidden;
}
.provider-header {
    padding: 24px 26px 20px;
    border-bottom: 1px solid var(--line);
    background:
        linear-gradient(135deg, color-mix(in srgb, var(--accent) 12%, white) 0%, rgba(255,255,255,0.60) 100%);
}
.provider-title-row {
    display: flex;
    justify-content: space-between;
    gap: 16px;
    align-items: baseline;
    flex-wrap: wrap;
}
.provider h2 {
    margin: 0;
    font-size: 30px;
    letter-spacing: -0.04em;
}
.provider-subtitle {
    margin-top: 10px;
    color: var(--muted);
    font-size: 14px;
    line-height: 1.5;
}
.provider-badge {
    padding: 8px 12px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--accent) 16%, white);
    color: var(--accent);
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 0.10em;
    font-weight: 700;
}
.provider-stats {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: 1px;
    background: rgba(35, 29, 23, 0.06);
}
.provider-stat {
    padding: 18px 20px;
    background: rgba(255, 253, 249, 0.96);
}
.provider-stat strong {
    display: block;
    font-size: 13px;
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.08em;
}
.provider-stat span {
    display: block;
    margin-top: 6px;
    font-size: 22px;
    font-weight: 730;
    letter-spacing: -0.03em;
}
.provider-body {
    padding: 24px 26px 28px;
}
.panel-grid {
    display: grid;
    grid-template-columns: 1.1fr 0.9fr;
    gap: 18px;
}
.panel {
    border: 1px solid var(--line);
    border-radius: 20px;
    background: rgba(255, 250, 242, 0.72);
    padding: 20px;
}
.panel h3 {
    margin: 0 0 6px;
    font-size: 18px;
    letter-spacing: -0.03em;
}
.panel-copy {
    color: var(--muted);
    font-size: 13px;
    line-height: 1.5;
    margin-bottom: 16px;
}
.bar-stack {
    display: flex;
    height: 16px;
    border-radius: 999px;
    overflow: hidden;
    background: rgba(35, 29, 23, 0.06);
}
.seg-input { background: var(--input); }
.seg-cache { background: var(--cache); }
.seg-output { background: var(--output); }
.seg-reasoning { background: var(--reasoning); }
.legend {
    display: grid;
    gap: 10px;
    margin-top: 16px;
}
.legend-item {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    align-items: center;
    font-size: 13px;
}
.legend-key {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    color: var(--muted);
}
.swatch {
    width: 10px;
    height: 10px;
    border-radius: 999px;
}
.project-bars {
    display: grid;
    gap: 14px;
}
.project-row {
    display: grid;
    gap: 7px;
}
.project-meta {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    align-items: baseline;
}
.project-title {
    font-size: 14px;
    font-weight: 700;
    letter-spacing: -0.02em;
}
.project-value {
    font-size: 13px;
    color: var(--muted);
    text-align: right;
}
.track {
    height: 12px;
    border-radius: 999px;
    background: rgba(35, 29, 23, 0.06);
    overflow: hidden;
}
.fill {
    height: 100%;
    min-width: 8px;
    border-radius: 999px;
    background: linear-gradient(90deg, var(--accent) 0%, color-mix(in srgb, var(--accent) 58%, white) 100%);
}
.project-detail {
    color: var(--muted);
    font-size: 12px;
    line-height: 1.45;
}
table {
    width: 100%;
    border-collapse: collapse;
    margin-top: 18px;
    font-size: 13px;
}
th, td {
    text-align: left;
    padding: 12px 10px;
    border-top: 1px solid var(--line);
    vertical-align: top;
}
th {
    color: var(--muted);
    text-transform: uppercase;
    letter-spacing: 0.08em;
    font-size: 11px;
    font-weight: 700;
}
td.num {
    text-align: right;
    white-space: nowrap;
}
.path {
    color: var(--muted);
    font-size: 12px;
    word-break: break-word;
}
.empty {
    padding: 28px;
    border: 1px dashed var(--line);
    border-radius: 18px;
    background: rgba(255, 250, 242, 0.55);
    color: var(--muted);
    font-size: 14px;
    line-height: 1.6;
}
.footer {
    margin-top: 22px;
    padding: 18px 6px 0;
    color: var(--muted);
    font-size: 12px;
    line-height: 1.6;
}
@media (max-width: 980px) {
    body {
        padding: 18px;
    }
    .kpi-grid,
    .provider-stats,
    .provider-grid,
    .panel-grid {
        grid-template-columns: 1fr;
    }
}
</style>
</head>
<body>
<div class="shell">
"#,
    );

    write!(
        html,
        r#"<section class="hero">
<div class="hero-top">
    <div>
        <div class="eyebrow">Offline Usage History</div>
        <h1>HTML report for Claude and Codex project usage.</h1>
        <p class="hero-copy">This export reads local session artifacts from <code>.claude</code> and <code>.codex</code>, rolls them up by project folder, and estimates spend using the embedded March 25, 2026 API price catalog. No prompts are embedded in the report.</p>
    </div>
    <div class="stamp">
        <strong>Generated</strong>
        <span>Unix time {generated_at_unix}</span>
        <span>Single-file HTML with inline charts.</span>
        <span>Open directly in any browser.</span>
    </div>
</div>
<div class="kpi-grid">
    <div class="kpi">
        <div class="kpi-label">Estimated Spend</div>
        <div class="kpi-value">{}</div>
        <div class="kpi-note">Claude + Codex combined</div>
    </div>
    <div class="kpi">
        <div class="kpi-label">Sessions</div>
        <div class="kpi-value">{}</div>
        <div class="kpi-note">Imported local history</div>
    </div>
    <div class="kpi">
        <div class="kpi-label">Tracked Projects</div>
        <div class="kpi-value">{}</div>
        <div class="kpi-note">Provider-local project roots</div>
    </div>
    <div class="kpi">
        <div class="kpi-label">Token Footprint</div>
        <div class="kpi-value">{}</div>
        <div class="kpi-note">{}</div>
    </div>
</div>
</section>"#,
        format_usd(total_cost),
        total_sessions,
        total_projects,
        format_token_count(total_input_tokens.saturating_add(total_cached_tokens).saturating_add(total_output_tokens)),
        if total_reasoning_tokens > 0 {
            format!("Includes {} reasoning tokens", format_token_count(total_reasoning_tokens))
        } else {
            "Input, cache, and output totals".to_string()
        }
    )
    .unwrap();

    html.push_str(r#"<div class="provider-grid">"#);
    render_provider_section(
        &mut html,
        "Claude",
        &snapshot.claude,
        "var(--claude)",
        "var(--claude-soft)",
    );
    render_provider_section(
        &mut html,
        "Codex",
        &snapshot.codex,
        "var(--codex)",
        "var(--codex-soft)",
    );
    html.push_str("</div>");

    html.push_str(
        r#"<div class="footer">
<strong>Pricing note:</strong> cost is estimated, not invoice-grade billing. Claude pricing is family-mapped by model name, Codex pricing is mapped to GPT-5 Codex variants, and unmatched models are counted as unpriced instead of guessed.
</div>
</div>
</body>
</html>
"#,
    );

    html
}

fn render_provider_section(
    html: &mut String,
    title: &str,
    summary: &HistorySummary,
    accent: &str,
    accent_soft: &str,
) {
    write!(
        html,
        r#"<section class="provider" style="--accent: {accent}; --accent-soft: {accent_soft};">
<div class="provider-header">
    <div class="provider-title-row">
        <h2>{}</h2>
        <div class="provider-badge">{}</div>
    </div>
    <div class="provider-subtitle">{}</div>
</div>"#,
        escape_html(title),
        if summary.total_sessions == 0 {
            "No local history"
        } else {
            "Local session aggregate"
        },
        if summary.total_sessions == 0 {
            format!(
                "{} session files were not found or did not contain usable project-level token data.",
                title
            )
        } else {
            format!(
                "{} history grouped by normalized project folder with cost, token, and session rollups.",
                title
            )
        }
    )
    .unwrap();

    if summary.total_sessions == 0 {
        html.push_str(
            r#"<div class="provider-body">
<div class="empty">No usable local history was found for this provider. The app can still poll current live usage, but historical charts need retained session artifacts on disk.</div>
</div>
</section>"#,
        );
        return;
    }

    write!(
        html,
        r#"<div class="provider-stats">
    <div class="provider-stat"><strong>Estimated Cost</strong><span>{}</span></div>
    <div class="provider-stat"><strong>Projects</strong><span>{}</span></div>
    <div class="provider-stat"><strong>Sessions</strong><span>{}</span></div>
    <div class="provider-stat"><strong>Input / Output</strong><span>{} / {}</span></div>
</div>
<div class="provider-body">"#,
        format_usd(summary.estimated_cost_usd),
        summary.total_projects,
        summary.total_sessions,
        format_token_count(summary.input_tokens),
        format_token_count(summary.output_tokens)
    )
    .unwrap();

    html.push_str(r#"<div class="panel-grid">"#);
    render_project_chart(html, summary);
    render_token_mix_panel(html, summary);
    html.push_str("</div>");
    render_project_table(html, summary);
    html.push_str("</div></section>");
}

fn render_project_chart(html: &mut String, summary: &HistorySummary) {
    let chart_by_cost = summary
        .top_projects
        .iter()
        .any(|entry| entry.estimated_cost_usd > 0.0);
    let max_value = summary
        .top_projects
        .iter()
        .map(|entry| {
            if chart_by_cost {
                entry.estimated_cost_usd
            } else {
                total_tokens(entry) as f64
            }
        })
        .fold(0.0_f64, f64::max);

    write!(
        html,
        r#"<div class="panel">
<h3>{}</h3>
<div class="panel-copy">{}</div>
<div class="project-bars">"#,
        if chart_by_cost {
            "Top Projects by Estimated Cost"
        } else {
            "Top Projects by Token Volume"
        },
        if chart_by_cost {
            "Bars are scaled to estimated spend. When pricing is unavailable for a model, the project still appears in the table below as unpriced activity."
        } else {
            "No priced models were matched here, so the chart falls back to total token volume."
        }
    )
    .unwrap();

    for entry in &summary.top_projects {
        let raw_value = if chart_by_cost {
            entry.estimated_cost_usd
        } else {
            total_tokens(entry) as f64
        };
        let width = if max_value > 0.0 {
            (raw_value / max_value * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };
        write!(
            html,
            r#"<div class="project-row">
<div class="project-meta">
    <div class="project-title">{}</div>
    <div class="project-value">{}</div>
</div>
<div class="track"><div class="fill" style="width: {:.2}%"></div></div>
<div class="project-detail">{} sessions | {} in | {} out{}{}</div>
</div>"#,
            escape_html(&project_label(&entry.project_path)),
            if chart_by_cost {
                format_usd(entry.estimated_cost_usd)
            } else {
                format_token_count(total_tokens(entry))
            },
            width,
            entry.sessions,
            format_token_count(entry.input_tokens),
            format_token_count(entry.output_tokens),
            if entry.cached_input_tokens > 0 {
                format!(" | {} cache", format_token_count(entry.cached_input_tokens))
            } else {
                String::new()
            },
            if entry.reasoning_output_tokens > 0 {
                format!(
                    " | {} reasoning",
                    format_token_count(entry.reasoning_output_tokens)
                )
            } else {
                String::new()
            }
        )
        .unwrap();
    }

    html.push_str("</div></div>");
}

fn render_token_mix_panel(html: &mut String, summary: &HistorySummary) {
    let total = summary
        .input_tokens
        .saturating_add(summary.cached_input_tokens)
        .saturating_add(summary.output_tokens)
        .saturating_add(summary.reasoning_output_tokens);
    let (input_pct, cache_pct, output_pct, reasoning_pct) = if total == 0 {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        (
            percent(summary.input_tokens, total),
            percent(summary.cached_input_tokens, total),
            percent(summary.output_tokens, total),
            percent(summary.reasoning_output_tokens, total),
        )
    };

    write!(
        html,
        r#"<div class="panel">
<h3>Token Composition</h3>
<div class="panel-copy">Input, cache, output, and reasoning totals across imported local history.</div>
<div class="bar-stack">
    <div class="seg-input" style="width: {:.2}%"></div>
    <div class="seg-cache" style="width: {:.2}%"></div>
    <div class="seg-output" style="width: {:.2}%"></div>
    <div class="seg-reasoning" style="width: {:.2}%"></div>
</div>
<div class="legend">
    {}
</div>
</div>"#,
        input_pct,
        cache_pct,
        output_pct,
        reasoning_pct,
        [
            legend_item("seg-input", "Input", summary.input_tokens, input_pct),
            legend_item(
                "seg-cache",
                "Cache",
                summary.cached_input_tokens,
                cache_pct,
            ),
            legend_item("seg-output", "Output", summary.output_tokens, output_pct),
            legend_item(
                "seg-reasoning",
                "Reasoning",
                summary.reasoning_output_tokens,
                reasoning_pct,
            ),
        ]
        .join("")
    )
    .unwrap();
}

fn render_project_table(html: &mut String, summary: &HistorySummary) {
    html.push_str(
        r#"<table>
<thead>
<tr>
    <th>Project</th>
    <th>Path</th>
    <th class="num">Sessions</th>
    <th class="num">Input</th>
    <th class="num">Cache</th>
    <th class="num">Output</th>
    <th class="num">Reasoning</th>
    <th class="num">Est. Cost</th>
</tr>
</thead>
<tbody>"#,
    );

    for entry in &summary.top_projects {
        write!(
            html,
            r#"<tr>
    <td>{}</td>
    <td class="path">{}</td>
    <td class="num">{}</td>
    <td class="num">{}</td>
    <td class="num">{}</td>
    <td class="num">{}</td>
    <td class="num">{}</td>
    <td class="num">{}</td>
</tr>"#,
            escape_html(&project_label(&entry.project_path)),
            escape_html(&entry.project_path),
            entry.sessions,
            format_token_count(entry.input_tokens),
            format_token_count(entry.cached_input_tokens),
            format_token_count(entry.output_tokens),
            if entry.reasoning_output_tokens > 0 {
                format_token_count(entry.reasoning_output_tokens)
            } else {
                "-".to_string()
            },
            if entry.estimated_cost_usd > 0.0 {
                format_usd(entry.estimated_cost_usd)
            } else {
                "Unpriced".to_string()
            }
        )
        .unwrap();
    }

    html.push_str("</tbody></table>");
}

fn legend_item(class_name: &str, label: &str, value: u64, pct: f64) -> String {
    format!(
        r#"<div class="legend-item">
<div class="legend-key"><span class="swatch {}"></span>{}</div>
<div>{} <span style="color: var(--muted);">({:.1}%)</span></div>
</div>"#,
        class_name,
        escape_html(label),
        format_token_count(value),
        pct
    )
}

fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        value as f64 / total as f64 * 100.0
    }
}

fn format_token_count(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.1}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_usd(value: f64) -> String {
    if value >= 1000.0 {
        format!("${:.0}", value)
    } else if value >= 100.0 {
        format!("${:.1}", value)
    } else {
        format!("${:.2}", value)
    }
}

fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn collect_snapshot(sync: &HistorySyncSettings, token: Option<&str>) -> HistorySnapshot {
    if sync.storage_mode == HistoryStorageMode::RemoteOnly {
        let live_store = collect_live_store();
        if let Some(token) = token {
            if let Ok(store) = fetch_remote_store(sync, token) {
                return summarize_store(&merge_history_stores(store, live_store));
            }
        }

        return summarize_store(&live_store);
    }

    summarize_store(&refresh_store_best_effort(sync))
}

pub fn archive_history(sync: &HistorySyncSettings, token: &str) -> Result<ArchiveResult, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("A GitHub token is required for archive upload.".to_string());
    }

    if !sync.upload_history_store && !sync.upload_html_reports && !sync.upload_conversations {
        return Err(
            "GitHub sync is configured to upload nothing. Enable at least one scope.".to_string(),
        );
    }

    let repo_url = effective_archive_repo_url(sync.repo_url.as_deref());
    let repo = parse_github_repo_spec(&repo_url)?;
    let store = refresh_store(sync)?;
    let snapshot = summarize_store(&store);
    let generated_at_unix = unix_now();
    let agent = github_agent()?;

    if sync.upload_history_store {
        upload_github_file_with_agent(
            &agent,
            &repo,
            token,
            sync.branch.as_deref(),
            "latest/history-store.json",
            &serde_json::to_vec_pretty(&store)
                .map_err(|error| format!("Unable to serialize history store: {error}"))?,
            &format!("Update history store at {generated_at_unix}"),
        )?;
    }
    if sync.upload_html_reports {
        upload_github_file_with_agent(
            &agent,
            &repo,
            token,
            sync.branch.as_deref(),
            "latest/history-report.html",
            build_html_report(&snapshot, generated_at_unix).as_bytes(),
            &format!("Update history report at {generated_at_unix}"),
        )?;
    }
    let uploaded_conversation_files = if sync.upload_conversations {
        archive_conversation_logs(
            &agent,
            &repo,
            token,
            sync.branch.as_deref(),
            generated_at_unix,
        )?
    } else {
        0
    };
    let workflow_warning = if let Some(workflow_file) = sync
        .workflow_file
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        trigger_report_workflow(&repo, token, sync.branch.as_deref(), workflow_file.trim()).err()
    } else {
        None
    };

    let cleaned_reports = cleanup_generated_reports(LOCAL_REPORTS_TO_KEEP_AFTER_ARCHIVE)?;
    clear_cached_snapshot();
    Ok(ArchiveResult {
        repo_url,
        cleaned_reports,
        uploaded_conversation_files,
        workflow_warning,
    })
}

pub fn fetch_history_from_github(
    sync: &HistorySyncSettings,
    token: &str,
) -> Result<FetchResult, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("A GitHub token is required for GitHub fetch.".to_string());
    }

    let repo_url = effective_archive_repo_url(sync.repo_url.as_deref());
    let store = fetch_remote_store(sync, token)?;
    let snapshot = summarize_store(&store);

    let message = if sync.storage_mode == HistoryStorageMode::Local {
        save_store(&store)?;
        clear_cached_snapshot();
        "Remote history imported into the local history store.".to_string()
    } else {
        set_cached_snapshot(&snapshot);
        "Remote history loaded into memory. Local app history storage remains off.".to_string()
    };

    Ok(FetchResult { repo_url, message })
}

pub fn install_sample_workflow_to_github(
    sync: &HistorySyncSettings,
    token: &str,
) -> Result<WorkflowInstallResult, String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("A GitHub token is required to install the workflow.".to_string());
    }

    let repo_url = effective_archive_repo_url(sync.repo_url.as_deref());
    let repo = parse_github_repo_spec(&repo_url)?;
    let workflow_file = workflow_file_name(sync.workflow_file.as_deref())?;
    let remote_path = format!(".github/workflows/{workflow_file}");
    upload_github_file(
        &repo,
        token,
        sync.branch.as_deref(),
        &remote_path,
        SAMPLE_WORKFLOW_YAML.as_bytes(),
        &format!("Install workflow {workflow_file}"),
    )?;

    Ok(WorkflowInstallResult {
        location: format!(
            "{repo_url} [{}] -> {}",
            effective_archive_branch(sync.branch.as_deref())
                .unwrap_or_else(|| "default branch".to_string()),
            remote_path
        ),
    })
}

pub fn install_sample_workflow_to_local_repo(
    local_repo_path: &Path,
    workflow_file: Option<&str>,
) -> Result<PathBuf, String> {
    let repo_root = validate_local_repo_root(local_repo_path)?;
    let workflow_name = workflow_file_name(workflow_file)?;
    let workflows_dir = repo_root.join(".github").join("workflows");
    std::fs::create_dir_all(&workflows_dir).map_err(|error| {
        format!(
            "Unable to create local workflows directory {}: {error}",
            workflows_dir.display()
        )
    })?;

    let workflow_path = workflows_dir.join(workflow_name);
    std::fs::write(&workflow_path, SAMPLE_WORKFLOW_YAML).map_err(|error| {
        format!(
            "Unable to write local workflow file {}: {error}",
            workflow_path.display()
        )
    })?;

    Ok(workflow_path)
}

pub fn local_workflows_dir(local_repo_path: &Path) -> Result<PathBuf, String> {
    Ok(validate_local_repo_root(local_repo_path)?
        .join(".github")
        .join("workflows"))
}

fn refresh_store_best_effort(sync: &HistorySyncSettings) -> HistoryStore {
    refresh_store(sync).unwrap_or_else(|_| {
        if sync.storage_mode == HistoryStorageMode::Local {
            load_store().unwrap_or_default()
        } else {
            collect_live_store()
        }
    })
}

fn refresh_store(sync: &HistorySyncSettings) -> Result<HistoryStore, String> {
    let mut store = if sync.storage_mode == HistoryStorageMode::Local {
        load_store()?
    } else {
        HistoryStore::default()
    };
    let mut changed = store.version != HISTORY_STORE_VERSION;

    changed |= merge_sessions(&mut store, scan_claude_sessions());
    changed |= merge_sessions(&mut store, scan_codex_sessions());

    store.version = HISTORY_STORE_VERSION;
    store.updated_at_unix = unix_now();

    if sync.storage_mode == HistoryStorageMode::Local && changed {
        save_store(&store)?;
    }

    Ok(store)
}

fn collect_live_store() -> HistoryStore {
    let mut store = HistoryStore {
        version: HISTORY_STORE_VERSION,
        updated_at_unix: unix_now(),
        sessions: Vec::new(),
    };
    let _ = merge_sessions(&mut store, scan_claude_sessions());
    let _ = merge_sessions(&mut store, scan_codex_sessions());
    store
}

fn build_report_snapshot(sync: &HistorySyncSettings, token: Option<&str>) -> HistorySnapshot {
    if sync.prefer_remote_reports {
        if let Some(token) = token {
            if let Ok(remote_store) = fetch_remote_store(sync, token) {
                let local_store = if sync.storage_mode == HistoryStorageMode::Local {
                    refresh_store_best_effort(sync)
                } else {
                    collect_live_store()
                };
                return summarize_store(&merge_history_stores(remote_store, local_store));
            }
        }
    }

    clear_cached_snapshot();
    read_snapshot(sync, token)
}

fn validate_local_repo_root(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!(
            "Local repository path does not exist: {}",
            path.display()
        ));
    }
    if !path.is_dir() {
        return Err(format!(
            "Local repository path is not a directory: {}",
            path.display()
        ));
    }
    if !path.join(".git").exists() {
        return Err(format!(
            "Local repository path does not look like a git repo: {}",
            path.display()
        ));
    }
    Ok(path.to_path_buf())
}

fn load_store() -> Result<HistoryStore, String> {
    let path = history_store_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HistoryStore {
                version: HISTORY_STORE_VERSION,
                updated_at_unix: 0,
                sessions: Vec::new(),
            });
        }
        Err(error) => {
            return Err(format!(
                "Unable to read history store {}: {error}",
                path.display()
            ));
        }
    };

    let mut store: HistoryStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Unable to parse history store {}: {error}", path.display()))?;
    if store.version == 0 {
        store.version = HISTORY_STORE_VERSION;
    }
    Ok(store)
}

fn save_store(store: &HistoryStore) -> Result<(), String> {
    let path = history_store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create history store directory: {error}"))?;
    }

    std::fs::write(
        &path,
        serde_json::to_vec(store)
            .map_err(|error| format!("Unable to serialize history store: {error}"))?,
    )
    .map_err(|error| format!("Unable to write history store {}: {error}", path.display()))
}

fn merge_sessions(store: &mut HistoryStore, incoming: Vec<StoredSessionRecord>) -> bool {
    let mut changed = false;
    let mut index = HashMap::<(ProviderKind, String), usize>::new();
    for (position, session) in store.sessions.iter().enumerate() {
        index.insert((session.provider, session.id.clone()), position);
    }

    for record in incoming {
        let key = (record.provider, record.id.clone());
        if let Some(position) = index.get(&key).copied() {
            let existing = &mut store.sessions[position];
            if should_replace_session(existing, &record) {
                *existing = record;
                changed = true;
            }
        } else {
            index.insert(key, store.sessions.len());
            store.sessions.push(record);
            changed = true;
        }
    }

    if changed {
        store.sessions.sort_by(|left, right| {
            right
                .last_seen_unix
                .cmp(&left.last_seen_unix)
                .then(
                    left.provider
                        .short_label()
                        .cmp(right.provider.short_label()),
                )
                .then(left.project_path.cmp(&right.project_path))
                .then(left.id.cmp(&right.id))
        });
    }

    changed
}

fn should_replace_session(existing: &StoredSessionRecord, incoming: &StoredSessionRecord) -> bool {
    session_total_tokens(incoming) > session_total_tokens(existing)
        || (session_total_tokens(incoming) == session_total_tokens(existing)
            && incoming.estimated_cost_usd > existing.estimated_cost_usd)
        || (existing.project_path.is_empty() && !incoming.project_path.is_empty())
        || (existing.model_name.is_none() && incoming.model_name.is_some())
}

fn session_total_tokens(record: &StoredSessionRecord) -> u64 {
    record
        .input_tokens
        .saturating_add(record.cached_input_tokens)
        .saturating_add(record.output_tokens)
        .saturating_add(record.reasoning_output_tokens)
}

fn scan_claude_sessions() -> Vec<StoredSessionRecord> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut files = Vec::new();
    walk_files(&home.join(".claude").join("projects"), &mut files);
    let mut sessions = HashMap::<String, ClaudeSessionAggregate>::new();
    let mut counted_messages = HashSet::<String>::new();

    for path in files.into_iter().filter(|path| is_jsonl_file(path)) {
        let Ok(file) = File::open(&path) else {
            continue;
        };

        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<ClaudeProjectRecord>(&line) else {
                continue;
            };

            let session_id = record
                .session_id
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| path.to_string_lossy().to_string());
            let entry = sessions.entry(session_id.clone()).or_default();

            if entry.project_path.is_none() {
                if let Some(cwd) = record
                    .cwd
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                {
                    entry.project_path = Some(cwd.to_string());
                }
            }

            let Some(message) = record.message else {
                continue;
            };
            if entry.model_name.is_none() {
                if let Some(model) = message
                    .model
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                {
                    entry.model_name = Some(model.to_string());
                }
            }
            if message.role.as_deref() != Some("assistant") {
                continue;
            }

            let Some(usage) = message.usage else {
                continue;
            };
            let dedupe_key = message
                .id
                .or(record.uuid)
                .map(|id| format!("{session_id}:{id}"))
                .unwrap_or_else(|| format!("{session_id}:{}", line.len()));
            if !counted_messages.insert(dedupe_key) {
                continue;
            }

            let cache_creation = usage.cache_creation.unwrap_or(ClaudeCacheCreation {
                ephemeral_5m_input_tokens: 0,
                ephemeral_1h_input_tokens: 0,
            });
            let cache_write_total = usage.cache_creation_input_tokens;
            let cache_write_5m = cache_creation
                .ephemeral_5m_input_tokens
                .min(cache_write_total);
            let cache_write_1h = cache_creation
                .ephemeral_1h_input_tokens
                .min(cache_write_total.saturating_sub(cache_write_5m));
            let cache_write_remaining =
                cache_write_total.saturating_sub(cache_write_5m + cache_write_1h);

            entry.input_tokens = entry.input_tokens.saturating_add(usage.input_tokens);
            entry.cache_write_5m_tokens = entry
                .cache_write_5m_tokens
                .saturating_add(cache_write_5m.saturating_add(cache_write_remaining));
            entry.cache_write_1h_tokens =
                entry.cache_write_1h_tokens.saturating_add(cache_write_1h);
            entry.cache_read_tokens = entry
                .cache_read_tokens
                .saturating_add(usage.cache_read_input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(usage.output_tokens);
        }
    }

    let mut records = Vec::new();
    for (session_id, session) in sessions {
        let Some(raw_project_path) = session.project_path else {
            continue;
        };
        let Some((_, display_path)) = normalize_project_path(&raw_project_path) else {
            continue;
        };

        let estimate = pricing::estimate_claude_cost(
            session.model_name.as_deref(),
            session.input_tokens,
            session.cache_write_5m_tokens,
            session.cache_write_1h_tokens,
            session.cache_read_tokens,
            session.output_tokens,
        );
        records.push(StoredSessionRecord {
            id: session_id,
            provider: ProviderKind::Claude,
            project_path: display_path,
            model_name: session.model_name,
            input_tokens: session.input_tokens,
            cached_input_tokens: session
                .cache_write_5m_tokens
                .saturating_add(session.cache_write_1h_tokens)
                .saturating_add(session.cache_read_tokens),
            output_tokens: session.output_tokens,
            reasoning_output_tokens: 0,
            estimated_cost_usd: estimate.usd,
            priced: estimate.priced,
            last_seen_unix: unix_now(),
        });
    }

    records
}

fn scan_codex_sessions() -> Vec<StoredSessionRecord> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let sessions_dir = home.join(".codex").join("sessions");
    let mut files = Vec::new();
    walk_files(&sessions_dir, &mut files);
    let mut records = Vec::new();

    for path in files.into_iter().filter(|path| is_jsonl_file(path)) {
        let Ok(file) = File::open(&path) else {
            continue;
        };
        let reader = BufReader::new(file);
        let mut raw_project_path: Option<String> = None;
        let mut model_name: Option<String> = None;
        let mut totals = CodexTokenTotals::default();

        for line in reader.lines() {
            let Ok(line) = line else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<CodexRecord>(&line) else {
                continue;
            };

            match record.record_type.as_str() {
                "session_meta" if raw_project_path.is_none() => {
                    if let Ok(payload) =
                        serde_json::from_value::<CodexSessionMetaPayload>(record.payload)
                    {
                        raw_project_path = Some(payload.cwd);
                    }
                }
                "turn_context" => {
                    if let Ok(payload) =
                        serde_json::from_value::<CodexTurnContextPayload>(record.payload)
                    {
                        if raw_project_path.is_none() {
                            raw_project_path = Some(payload.cwd);
                        }
                        if model_name.is_none() {
                            model_name = payload.model;
                        }
                    }
                }
                "event_msg" => {
                    if let Ok(payload) = serde_json::from_value::<CodexEventPayload>(record.payload)
                    {
                        if payload.event_type == "token_count" {
                            if let Some(info) = payload.info {
                                totals.input_tokens =
                                    totals.input_tokens.max(info.total_token_usage.input_tokens);
                                totals.cached_input_tokens = totals
                                    .cached_input_tokens
                                    .max(info.total_token_usage.cached_input_tokens);
                                totals.output_tokens = totals
                                    .output_tokens
                                    .max(info.total_token_usage.output_tokens);
                                totals.reasoning_output_tokens = totals
                                    .reasoning_output_tokens
                                    .max(info.total_token_usage.reasoning_output_tokens);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        let Some(raw_project_path) = raw_project_path else {
            continue;
        };
        let Some((_, display_path)) = normalize_project_path(&raw_project_path) else {
            continue;
        };

        let estimate = pricing::estimate_codex_cost(
            model_name.as_deref(),
            totals.input_tokens,
            totals.cached_input_tokens,
            totals.output_tokens,
        );
        records.push(StoredSessionRecord {
            id: path.to_string_lossy().to_string(),
            provider: ProviderKind::Codex,
            project_path: display_path,
            model_name,
            input_tokens: totals.input_tokens,
            cached_input_tokens: totals.cached_input_tokens,
            output_tokens: totals.output_tokens,
            reasoning_output_tokens: totals.reasoning_output_tokens,
            estimated_cost_usd: estimate.usd,
            priced: estimate.priced,
            last_seen_unix: unix_now(),
        });
    }

    records
}

fn summarize_store(store: &HistoryStore) -> HistorySnapshot {
    HistorySnapshot {
        claude: summarize_sessions(
            store
                .sessions
                .iter()
                .filter(|session| session.provider == ProviderKind::Claude),
        ),
        codex: summarize_sessions(
            store
                .sessions
                .iter()
                .filter(|session| session.provider == ProviderKind::Codex),
        ),
    }
}

fn merge_history_stores(mut base: HistoryStore, incoming: HistoryStore) -> HistoryStore {
    merge_sessions(&mut base, incoming.sessions);
    base.version = HISTORY_STORE_VERSION;
    base.updated_at_unix = base.updated_at_unix.max(incoming.updated_at_unix);
    base
}

fn summarize_sessions<'a, I>(sessions: I) -> HistorySummary
where
    I: IntoIterator<Item = &'a StoredSessionRecord>,
{
    let mut projects = HashMap::<String, ProjectAccumulator>::new();
    for session in sessions {
        let key = session.project_path.to_ascii_lowercase();
        let entry = projects.entry(key).or_default();
        if entry.display_path.is_empty() {
            entry.display_path = session.project_path.clone();
        }
        entry.sessions = entry.sessions.saturating_add(1);
        entry.input_tokens = entry.input_tokens.saturating_add(session.input_tokens);
        entry.cached_input_tokens = entry
            .cached_input_tokens
            .saturating_add(session.cached_input_tokens);
        entry.output_tokens = entry.output_tokens.saturating_add(session.output_tokens);
        entry.reasoning_output_tokens = entry
            .reasoning_output_tokens
            .saturating_add(session.reasoning_output_tokens);
        entry.estimated_cost_usd += session.estimated_cost_usd;
        if !session.priced {
            entry.unpriced_sessions = entry.unpriced_sessions.saturating_add(1);
        }
    }

    summarize_projects(projects)
}

fn summarize_projects(projects: HashMap<String, ProjectAccumulator>) -> HistorySummary {
    let mut summary = HistorySummary {
        total_projects: projects.len() as u32,
        ..HistorySummary::default()
    };
    for entry in projects.values() {
        summary.total_sessions = summary.total_sessions.saturating_add(entry.sessions);
        summary.input_tokens = summary.input_tokens.saturating_add(entry.input_tokens);
        summary.cached_input_tokens = summary
            .cached_input_tokens
            .saturating_add(entry.cached_input_tokens);
        summary.output_tokens = summary.output_tokens.saturating_add(entry.output_tokens);
        summary.reasoning_output_tokens = summary
            .reasoning_output_tokens
            .saturating_add(entry.reasoning_output_tokens);
        summary.estimated_cost_usd += entry.estimated_cost_usd;
        summary.unpriced_sessions = summary
            .unpriced_sessions
            .saturating_add(entry.unpriced_sessions);
    }

    let mut top_projects: Vec<ProjectUsageEntry> = projects
        .into_values()
        .map(|entry| ProjectUsageEntry {
            project_path: entry.display_path,
            sessions: entry.sessions,
            input_tokens: entry.input_tokens,
            cached_input_tokens: entry.cached_input_tokens,
            output_tokens: entry.output_tokens,
            reasoning_output_tokens: entry.reasoning_output_tokens,
            estimated_cost_usd: entry.estimated_cost_usd,
        })
        .collect();

    top_projects.sort_by(|left, right| {
        right
            .estimated_cost_usd
            .partial_cmp(&left.estimated_cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(total_tokens(right).cmp(&total_tokens(left)))
            .then(right.sessions.cmp(&left.sessions))
            .then(left.project_path.cmp(&right.project_path))
    });
    top_projects.truncate(MAX_TOP_PROJECTS);
    summary.top_projects = top_projects;
    summary
}

pub fn effective_archive_repo_url(repo_spec: Option<&str>) -> String {
    repo_spec
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_ARCHIVE_REPO_URL)
        .to_string()
}

fn effective_archive_branch(branch: Option<&str>) -> Option<String> {
    branch
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Validates that a token looks like a GitHub PAT (classic `ghp_` or fine-grained `github_pat_`).
/// Returns `Ok(())` on success or a user-friendly error describing the expected format.
pub fn validate_github_token_format(token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("GitHub token cannot be empty.".to_string());
    }
    if token.starts_with("ghp_") || token.starts_with("github_pat_") {
        Ok(())
    } else {
        Err(
            "Token does not look like a GitHub personal access token. Classic tokens start with ghp_ and fine-grained tokens start with github_pat_."
                .to_string(),
        )
    }
}

/// Calls the GitHub `/user` endpoint to verify a token is valid and returns the authenticated
/// username on success.
pub fn validate_github_token_live(token: &str) -> Result<String, String> {
    let token = token.trim();
    let agent = github_agent_with_timeout(Duration::from_secs(15))
        .map_err(|error| format!("Unable to validate token: {error}"))?;
    match agent
        .get("https://api.github.com/user")
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "OpencodeUsageMonitor")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
    {
        Ok(response) => {
            #[derive(Deserialize)]
            struct GhUser {
                login: String,
            }
            let user: GhUser = response
                .into_json()
                .map_err(|error| format!("Token accepted but unable to parse response: {error}"))?;
            Ok(user.login)
        }
        Err(ureq::Error::Status(401, _)) => {
            Err("Token is invalid or has been revoked.".to_string())
        }
        Err(ureq::Error::Status(403, _)) => {
            Err("Token was rejected (403 Forbidden). It may lack the required scopes or be IP-restricted.".to_string())
        }
        Err(error) => Err(format!("Unable to validate token: {error}")),
    }
}

fn workflow_file_name(value: Option<&str>) -> Result<String, String> {
    let raw = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_WORKFLOW_FILE_NAME)
        .replace('\\', "/");
    let trimmed = raw
        .trim_start_matches(".github/workflows/")
        .trim_start_matches('/');
    if trimmed.is_empty() {
        return Err("Workflow file name cannot be empty.".to_string());
    }
    if trimmed.contains('/') {
        return Err("Workflow file must be a file name, not a nested path.".to_string());
    }
    if !(trimmed.ends_with(".yml") || trimmed.ends_with(".yaml")) {
        return Err("Workflow file must end with .yml or .yaml.".to_string());
    }
    Ok(trimmed.to_string())
}

#[derive(Clone, Debug)]
struct GitHubRepoSpec {
    owner: String,
    repo: String,
}

#[derive(Clone, Debug)]
struct ConversationArchiveFile {
    local_path: PathBuf,
    remote_path: String,
}

#[derive(Serialize)]
struct ConversationArchiveManifest {
    version: u32,
    generated_at_unix: u64,
    files: Vec<ConversationArchiveManifestEntry>,
}

#[derive(Serialize)]
struct ConversationArchiveManifestEntry {
    provider: &'static str,
    remote_path: String,
    size_bytes: u64,
    modified_at_unix: u64,
}

#[derive(Deserialize)]
struct GitHubContentSha {
    sha: String,
}

#[derive(Deserialize)]
struct GitHubRepoInfo {
    default_branch: String,
}

fn parse_github_repo_spec(value: &str) -> Result<GitHubRepoSpec, String> {
    let trimmed = value.trim().trim_end_matches('/');
    let raw = trimmed
        .strip_prefix("git@github.com:")
        .or_else(|| trimmed.strip_prefix("https://github.com/"))
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .unwrap_or(trimmed)
        .trim_end_matches(".git");

    let mut parts = raw.split('/');
    let owner = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| "GitHub repo must include an owner.".to_string())?;
    let repo = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| "GitHub repo must include a repository name.".to_string())?;
    if parts.next().is_some() {
        return Err("GitHub repo must be in owner/repo form.".to_string());
    }

    Ok(GitHubRepoSpec {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn github_agent() -> Result<ureq::Agent, String> {
    github_agent_with_timeout(Duration::from_secs(30))
}

fn github_agent_with_timeout(timeout: Duration) -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new()
        .map_err(|error| format!("Unable to initialize TLS support for GitHub sync: {error}"))?;
    Ok(ureq::AgentBuilder::new()
        .timeout(timeout)
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn upload_github_file(
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    path: &str,
    content: &[u8],
    message: &str,
) -> Result<(), String> {
    upload_github_file_with_agent(
        &github_agent()?,
        repo,
        token,
        branch,
        path,
        content,
        message,
    )
}

fn upload_github_file_with_agent(
    agent: &ureq::Agent,
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    path: &str,
    content: &[u8],
    message: &str,
) -> Result<(), String> {
    let sha = fetch_github_file_sha(agent, repo, token, branch, path)?;

    let mut body = serde_json::Map::new();
    body.insert("message".to_string(), Value::String(message.to_string()));
    body.insert("content".to_string(), Value::String(base64_encode(content)));
    if let Some(branch) = effective_archive_branch(branch) {
        body.insert("branch".to_string(), Value::String(branch));
    }
    if let Some(sha) = sha {
        body.insert("sha".to_string(), Value::String(sha));
    }

    let url = github_contents_url(repo, path, None);
    match agent
        .put(&url)
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "OpencodeUsageMonitor")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .send_json(Value::Object(body))
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, response)) => Err(format!(
            "GitHub upload failed for {path}: HTTP {code}{} {}",
            github_error_hint(code),
            compact_response_body(response.into_string().unwrap_or_default())
        )),
        Err(error) => Err(format!("GitHub upload failed for {path}: {error}")),
    }
}

fn fetch_remote_store(sync: &HistorySyncSettings, token: &str) -> Result<HistoryStore, String> {
    let repo_url = effective_archive_repo_url(sync.repo_url.as_deref());
    let repo = parse_github_repo_spec(&repo_url)?;
    let bytes = download_github_file(
        &repo,
        token,
        sync.branch.as_deref(),
        "latest/history-store.json",
    )?
    .ok_or_else(|| "No GitHub history store was found in latest/history-store.json.".to_string())?;
    let mut store: HistoryStore = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Unable to parse GitHub history store: {error}"))?;
    if store.version == 0 {
        store.version = HISTORY_STORE_VERSION;
    }
    Ok(store)
}

fn download_github_file(
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    path: &str,
) -> Result<Option<Vec<u8>>, String> {
    let agent = github_agent()?;
    let url = github_contents_url(repo, path, branch);
    match agent
        .get(&url)
        .set("Accept", "application/vnd.github.raw+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "OpencodeUsageMonitor")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
    {
        Ok(response) => {
            let mut bytes = Vec::new();
            response
                .into_reader()
                .read_to_end(&mut bytes)
                .map_err(|error| format!("Unable to read GitHub file {path}: {error}"))?;
            Ok(Some(bytes))
        }
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(code, response)) => Err(format!(
            "Unable to download GitHub file {path}: HTTP {code}{} {}",
            github_error_hint(code),
            compact_response_body(response.into_string().unwrap_or_default())
        )),
        Err(error) => Err(format!("Unable to download GitHub file {path}: {error}")),
    }
}

fn fetch_github_file_sha(
    agent: &ureq::Agent,
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    path: &str,
) -> Result<Option<String>, String> {
    let url = github_contents_url(repo, path, branch);
    match agent
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "OpencodeUsageMonitor")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
    {
        Ok(response) => {
            let content: GitHubContentSha = response
                .into_json()
                .map_err(|error| format!("Unable to parse GitHub metadata for {path}: {error}"))?;
            Ok(Some(content.sha))
        }
        Err(ureq::Error::Status(404, _)) => Ok(None),
        Err(ureq::Error::Status(code, response)) => Err(format!(
            "Unable to check existing GitHub file {path}: HTTP {code}{} {}",
            github_error_hint(code),
            compact_response_body(response.into_string().unwrap_or_default())
        )),
        Err(error) => Err(format!(
            "Unable to check existing GitHub file {path}: {error}"
        )),
    }
}

fn trigger_report_workflow(
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    workflow_file: &str,
) -> Result<(), String> {
    let agent = github_agent()?;
    let workflow_ref = if let Some(branch) = effective_archive_branch(branch) {
        branch
    } else {
        let repo_info: GitHubRepoInfo = agent
            .get(&format!(
                "https://api.github.com/repos/{}/{}",
                repo.owner, repo.repo
            ))
            .set("Accept", "application/vnd.github+json")
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", "OpencodeUsageMonitor")
            .set("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .map_err(|error| format!("Unable to read GitHub repo metadata: {error}"))?
            .into_json()
            .map_err(|error| format!("Unable to parse GitHub repo metadata: {error}"))?;
        repo_info.default_branch
    };

    match agent
        .post(&format!(
            "https://api.github.com/repos/{}/{}/actions/workflows/{}/dispatches",
            repo.owner, repo.repo, workflow_file
        ))
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "OpencodeUsageMonitor")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .send_json(serde_json::json!({ "ref": workflow_ref }))
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, response)) => Err(format!(
            "Unable to trigger GitHub workflow {}: HTTP {code}{} {}",
            workflow_file,
            github_error_hint(code),
            compact_response_body(response.into_string().unwrap_or_default())
        )),
        Err(error) => Err(format!(
            "Unable to trigger GitHub workflow {}: {error}",
            workflow_file
        )),
    }
}

fn github_contents_url(repo: &GitHubRepoSpec, path: &str, branch: Option<&str>) -> String {
    let base = format!(
        "https://api.github.com/repos/{}/{}/contents/{}",
        repo.owner, repo.repo, path
    );
    if let Some(branch) = effective_archive_branch(branch) {
        format!("{base}?ref={}", url_encode_component(&branch))
    } else {
        base
    }
}

fn compact_response_body(body: String) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        String::new()
    } else {
        // Strip fields that could leak credentials or internal details.
        let sanitized = compact.replace(|c: char| c.is_ascii_control(), "");
        format!("- {}", trim_to_chars(&sanitized, 180))
    }
}

/// Returns a user-friendly hint for common GitHub HTTP error codes.
fn github_error_hint(code: u16) -> &'static str {
    match code {
        401 => " (token is invalid or has been revoked)",
        403 => " (token may lack required permissions or scopes)",
        404 => " (repository or path not found — check the repo URL and token scope)",
        409 => " (conflict — the file may have been modified concurrently)",
        422 => " (unprocessable — check that the branch exists and content is valid)",
        _ => "",
    }
}

fn cleanup_generated_reports(keep_latest: usize) -> Result<usize, String> {
    let reports_dir = reports_dir()?;
    let entries = match std::fs::read_dir(&reports_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "Unable to read reports directory {}: {error}",
                reports_dir.display()
            ));
        }
    };

    let mut candidates = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .map(|value| {
                    value.starts_with("usage-history-")
                        && (value.ends_with(".html") || value.ends_with(".md"))
                })
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    let mut removed = 0;
    for path in candidates.into_iter().skip(keep_latest) {
        std::fs::remove_file(&path)
            .map_err(|error| format!("Unable to remove old report {}: {error}", path.display()))?;
        removed += 1;
    }

    Ok(removed)
}

fn history_store_version() -> u32 {
    HISTORY_STORE_VERSION
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let packed = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;

        output.push(TABLE[((packed >> 18) & 0x3F) as usize] as char);
        output.push(TABLE[((packed >> 12) & 0x3F) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((packed >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(packed & 0x3F) as usize] as char
        } else {
            '='
        });
    }

    output
}

fn url_encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let is_unreserved =
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{:02X}", byte);
        }
    }
    encoded
}

fn trim_to_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut trimmed = String::new();
    for ch in value.chars().take(max_chars.saturating_sub(1)) {
        trimmed.push(ch);
    }
    trimmed.push_str("...");
    trimmed
}

fn total_tokens(entry: &ProjectUsageEntry) -> u64 {
    entry
        .input_tokens
        .saturating_add(entry.cached_input_tokens)
        .saturating_add(entry.output_tokens)
}

fn walk_files(dir: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, output);
        } else if path.is_file() {
            output.push(path);
        }
    }
}

fn is_jsonl_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false)
}

fn normalize_project_path(raw_path: &str) -> Option<(String, String)> {
    let trimmed = raw_path.trim().trim_matches('"');
    if trimmed.is_empty() {
        return None;
    }

    let candidate = PathBuf::from(trimmed);
    let rooted = if candidate.exists() {
        find_git_root(&candidate).unwrap_or(candidate)
    } else {
        candidate
    };

    let display_path = rooted
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_string();
    if display_path.is_empty() {
        return None;
    }

    Some((display_path.to_ascii_lowercase(), display_path))
}

fn find_git_root(path: &Path) -> Option<PathBuf> {
    let mut current = if path.is_file() { path.parent()? } else { path };
    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn project_label(project_path: &str) -> String {
    let path = Path::new(project_path);
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(project_path)
        .to_string()
}

fn app_data_dir() -> Result<PathBuf, String> {
    dirs::data_dir()
        .or_else(dirs::data_local_dir)
        .map(|dir| dir.join("OpencodeUsageMonitor"))
        .ok_or_else(|| "Unable to determine an application data directory.".to_string())
}

fn history_store_path() -> Result<PathBuf, String> {
    Ok(app_data_dir()?.join("history").join("session-store.json"))
}

fn reports_dir() -> Result<PathBuf, String> {
    Ok(app_data_dir()?.join("reports"))
}

fn archive_conversation_logs(
    agent: &ureq::Agent,
    repo: &GitHubRepoSpec,
    token: &str,
    branch: Option<&str>,
    generated_at_unix: u64,
) -> Result<usize, String> {
    let files = collect_conversation_archive_files();
    let mut manifest_files = Vec::with_capacity(files.len());

    for file in &files {
        let bytes = std::fs::read(&file.local_path).map_err(|error| {
            format!(
                "Unable to read conversation log {}: {error}",
                file.local_path.display()
            )
        })?;
        upload_github_file_with_agent(
            agent,
            repo,
            token,
            branch,
            &file.remote_path,
            &bytes,
            &format!(
                "Update conversation log {} at {generated_at_unix}",
                file.remote_path
            ),
        )?;

        let metadata = std::fs::metadata(&file.local_path).map_err(|error| {
            format!(
                "Unable to read conversation log metadata {}: {error}",
                file.local_path.display()
            )
        })?;
        manifest_files.push(ConversationArchiveManifestEntry {
            provider: conversation_provider_label(&file.remote_path),
            remote_path: file.remote_path.clone(),
            size_bytes: metadata.len(),
            modified_at_unix: metadata
                .modified()
                .ok()
                .and_then(system_time_to_unix)
                .unwrap_or(0),
        });
    }

    let manifest = ConversationArchiveManifest {
        version: 1,
        generated_at_unix,
        files: manifest_files,
    };
    upload_github_file_with_agent(
        agent,
        repo,
        token,
        branch,
        "latest/conversations/manifest.json",
        &serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("Unable to serialize conversation manifest: {error}"))?,
        &format!("Update conversation manifest at {generated_at_unix}"),
    )?;

    Ok(files.len())
}

fn collect_conversation_archive_files() -> Vec<ConversationArchiveFile> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };

    let mut files = Vec::new();
    push_conversation_archive_dir(
        &mut files,
        &home.join(".claude").join("projects"),
        "latest/conversations/claude/projects",
    );
    push_conversation_archive_dir(
        &mut files,
        &home.join(".codex").join("sessions"),
        "latest/conversations/codex/sessions",
    );

    let codex_history_path = home.join(".codex").join("history.jsonl");
    if codex_history_path.is_file() {
        files.push(ConversationArchiveFile {
            local_path: codex_history_path,
            remote_path: "latest/conversations/codex/history.jsonl".to_string(),
        });
    }

    files.sort_by(|left, right| left.remote_path.cmp(&right.remote_path));
    files
}

fn push_conversation_archive_dir(
    output: &mut Vec<ConversationArchiveFile>,
    root: &Path,
    remote_prefix: &str,
) {
    let mut paths = Vec::new();
    walk_files(root, &mut paths);

    for path in paths.into_iter().filter(|path| is_jsonl_file(path)) {
        let Ok(relative_path) = path.strip_prefix(root) else {
            continue;
        };
        let remote_suffix = relative_path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().replace('\\', "-"))
            .collect::<Vec<_>>()
            .join("/");
        if remote_suffix.is_empty() {
            continue;
        }
        output.push(ConversationArchiveFile {
            local_path: path,
            remote_path: format!("{remote_prefix}/{remote_suffix}"),
        });
    }
}

fn conversation_provider_label(remote_path: &str) -> &'static str {
    if remote_path.contains("/claude/") {
        "claude"
    } else {
        "codex"
    }
}

fn system_time_to_unix(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(
        provider: ProviderKind,
        id: &str,
        project_path: &str,
        input_tokens: u64,
        output_tokens: u64,
        updated_at_unix: u64,
    ) -> StoredSessionRecord {
        StoredSessionRecord {
            id: id.to_string(),
            provider,
            project_path: project_path.to_string(),
            model_name: Some("gpt-5.4".to_string()),
            input_tokens,
            cached_input_tokens: 0,
            output_tokens,
            reasoning_output_tokens: 0,
            estimated_cost_usd: input_tokens as f64 / 1000.0,
            priced: true,
            last_seen_unix: updated_at_unix,
        }
    }

    #[test]
    fn merge_history_stores_replaces_existing_sessions_and_keeps_new_ones() {
        let remote = HistoryStore {
            version: HISTORY_STORE_VERSION,
            updated_at_unix: 10,
            sessions: vec![session(
                ProviderKind::Codex,
                "session-a",
                "C:\\repo",
                100,
                10,
                10,
            )],
        };
        let local = HistoryStore {
            version: HISTORY_STORE_VERSION,
            updated_at_unix: 20,
            sessions: vec![
                session(ProviderKind::Codex, "session-a", "C:\\repo", 250, 30, 20),
                session(ProviderKind::Codex, "session-b", "C:\\repo", 75, 5, 20),
            ],
        };

        let merged = merge_history_stores(remote, local);

        assert_eq!(merged.updated_at_unix, 20);
        assert_eq!(merged.sessions.len(), 2);
        assert_eq!(merged.sessions[0].id, "session-a");
        assert_eq!(merged.sessions[0].input_tokens, 250);
        assert_eq!(merged.sessions[1].id, "session-b");
    }

    #[test]
    fn conversation_provider_label_matches_archive_prefix() {
        assert_eq!(
            conversation_provider_label("latest/conversations/claude/projects/a.jsonl"),
            "claude"
        );
        assert_eq!(
            conversation_provider_label("latest/conversations/codex/sessions/a.jsonl"),
            "codex"
        );
    }
}
