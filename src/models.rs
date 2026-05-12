use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[default]
    Claude,
    Codex,
    OpenCode,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 3] = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::OpenCode,
    ];

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Claude => "C",
            Self::Codex => "O",
            Self::OpenCode => "OC",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TokenWindowUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cached_input_tokens: u64,
    pub estimated_cost_usd: f64,
    pub unpriced_rows: u32,
}

impl TokenWindowUsage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.reasoning_output_tokens)
            .saturating_add(self.cached_input_tokens)
    }

    pub fn add(&mut self, usage: &TokenWindowUsage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.estimated_cost_usd += usage.estimated_cost_usd;
        self.unpriced_rows = self.unpriced_rows.saturating_add(usage.unpriced_rows);
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenCodeUsageWindows {
    pub five_hour: TokenWindowUsage,
    pub today: TokenWindowUsage,
    pub month: TokenWindowUsage,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStorageMode {
    #[default]
    Local,
    RemoteOnly,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistorySyncSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_repo_path: Option<String>,
    #[serde(default = "default_true")]
    pub upload_history_store: bool,
    #[serde(default = "default_true")]
    pub upload_html_reports: bool,
    #[serde(default)]
    pub upload_conversations: bool,
    #[serde(default)]
    pub prefer_remote_reports: bool,
    #[serde(default)]
    pub workflow_file: Option<String>,
    #[serde(default)]
    pub storage_mode: HistoryStorageMode,
}

impl Default for HistorySyncSettings {
    fn default() -> Self {
        Self {
            repo_url: None,
            branch: None,
            local_repo_path: None,
            upload_history_store: true,
            upload_html_reports: true,
            upload_conversations: false,
            prefer_remote_reports: false,
            workflow_file: None,
            storage_mode: HistoryStorageMode::Local,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default)]
pub struct UsageSection {
    pub percentage: f64,
    pub resets_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct UsageData {
    pub session: UsageSection,
    pub weekly: UsageSection,
    pub review: Option<UsageSection>,
    pub credits_remaining: Option<f64>,
    pub has_credits: bool,
    pub unlimited_credits: bool,
    pub account_email: Option<String>,
    pub plan_name: Option<String>,
    pub source_label: Option<String>,
    pub updated_at: Option<SystemTime>,
    pub opencode_windows: Option<OpenCodeUsageWindows>,
}

#[derive(Clone, Debug, Default)]
pub struct ActivitySummary {
    pub prompts_last_24h: u32,
    pub prompts_last_7d: u32,
    pub sessions_last_24h: u32,
    pub sessions_last_7d: u32,
    pub last_prompt: Option<String>,
    pub last_prompt_at: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectUsageEntry {
    pub project_path: String,
    pub sessions: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub estimated_cost_usd: f64,
}

#[derive(Clone, Debug, Default)]
pub struct HistorySummary {
    pub total_projects: u32,
    pub total_sessions: u32,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub estimated_cost_usd: f64,
    pub unpriced_sessions: u32,
    pub top_projects: Vec<ProjectUsageEntry>,
}

#[derive(Clone, Debug, Default)]
pub struct HistorySnapshot {
    pub claude: HistorySummary,
    pub codex: HistorySummary,
}
