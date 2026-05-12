use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::diagnose;
use crate::models::{ActivitySummary, UsageData, UsageSection};

const CREATE_NO_WINDOW: u32 = 0x08000000;
const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const USAGE_PATH_BACKEND_API: &str = "/wham/usage";
const USAGE_PATH_CODEX_API: &str = "/api/codex/usage";
const ALLOW_UNTRUSTED_BASE_URL_ENV: &str = "CCUM_ALLOW_UNTRUSTED_CODEX_BASE_URL";

#[derive(Debug)]
pub enum PollError {
    NoCredentials,
    CliUnavailable,
    RequestFailed,
}

#[derive(Deserialize)]
struct AuthFile {
    #[serde(rename = "OPENAI_API_KEY")]
    openai_api_key: Option<String>,
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

struct Credentials {
    access_token: String,
    account_id: Option<String>,
}

#[derive(Deserialize)]
struct UsageResponse {
    email: Option<String>,
    plan_type: Option<String>,
    rate_limit: Option<RateLimitDetails>,
    code_review_rate_limit: Option<RateLimitDetails>,
    credits: Option<CreditDetails>,
}

#[derive(Deserialize)]
struct RateLimitDetails {
    primary_window: Option<UsageWindow>,
    secondary_window: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    used_percent: f64,
    reset_at: Option<i64>,
}

#[derive(Deserialize)]
struct CreditDetails {
    has_credits: bool,
    unlimited: bool,
    balance: Option<Value>,
}

#[derive(Deserialize)]
struct RpcRateLimitsEnvelope {
    #[serde(rename = "rateLimits")]
    rate_limits: RpcRateLimits,
}

#[derive(Deserialize)]
struct RpcRateLimits {
    primary: Option<RpcWindow>,
    secondary: Option<RpcWindow>,
    credits: Option<RpcCredits>,
}

#[derive(Deserialize)]
struct RpcWindow {
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct RpcCredits {
    #[serde(rename = "hasCredits")]
    has_credits: bool,
    unlimited: bool,
    balance: Option<String>,
}

#[derive(Deserialize)]
struct RpcAccountEnvelope {
    account: Option<RpcAccount>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RpcAccount {
    #[serde(rename = "chatgpt")]
    Chatgpt {
        email: Option<String>,
        #[serde(rename = "planType")]
        plan_type: Option<String>,
    },
    #[serde(rename = "apikey")]
    ApiKey,
}

#[derive(Deserialize)]
struct HistoryEntry {
    session_id: String,
    ts: i64,
    text: String,
}

pub fn poll() -> Result<UsageData, PollError> {
    let credentials = read_credentials();

    if let Some(creds) = credentials.as_ref() {
        match fetch_usage_via_oauth(creds) {
            Ok(data) => return Ok(data),
            Err(error) => diagnose::log(format!("Codex OAuth usage fetch failed: {error:?}")),
        }
    }

    match fetch_usage_via_rpc() {
        Ok(data) => Ok(data),
        Err(error) => {
            diagnose::log(format!("Codex RPC usage fetch failed: {error:?}"));
            if credentials.is_none() {
                Err(PollError::NoCredentials)
            } else {
                Err(error)
            }
        }
    }
}

pub fn read_activity_summary() -> Option<ActivitySummary> {
    let path = codex_home_dir().join("history.jsonl");
    let file = std::fs::File::open(path).ok()?;
    let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let cutoff_day = now_secs.saturating_sub(24 * 60 * 60);
    let cutoff_week = now_secs.saturating_sub(7 * 24 * 60 * 60);

    let mut prompts_last_24h = 0u32;
    let mut prompts_last_7d = 0u32;
    let mut sessions_last_24h = HashSet::new();
    let mut sessions_last_7d = HashSet::new();
    let mut latest: Option<HistoryEntry> = None;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { continue };
        let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) else {
            continue;
        };

        if entry.ts >= cutoff_day {
            prompts_last_24h = prompts_last_24h.saturating_add(1);
            sessions_last_24h.insert(entry.session_id.clone());
        }

        if entry.ts >= cutoff_week {
            prompts_last_7d = prompts_last_7d.saturating_add(1);
            sessions_last_7d.insert(entry.session_id.clone());
        }

        let is_newer = latest
            .as_ref()
            .map(|current| entry.ts > current.ts)
            .unwrap_or(true);
        if is_newer {
            latest = Some(entry);
        }
    }

    Some(ActivitySummary {
        prompts_last_24h,
        prompts_last_7d,
        sessions_last_24h: sessions_last_24h.len() as u32,
        sessions_last_7d: sessions_last_7d.len() as u32,
        last_prompt: latest.as_ref().map(|entry| entry.text.trim().to_string()),
        last_prompt_at: latest.and_then(|entry| unix_to_system_time(entry.ts)),
    })
}

fn read_credentials() -> Option<Credentials> {
    let content = std::fs::read_to_string(codex_home_dir().join("auth.json")).ok()?;
    let auth: AuthFile = serde_json::from_str(&content).ok()?;

    if let Some(api_key) = auth.openai_api_key {
        let trimmed = api_key.trim();
        if !trimmed.is_empty() {
            return Some(Credentials {
                access_token: trimmed.to_string(),
                account_id: None,
            });
        }
    }

    let tokens = auth.tokens?;
    let access_token = tokens.access_token?.trim().to_string();
    if access_token.is_empty() {
        return None;
    }

    Some(Credentials {
        access_token,
        account_id: tokens.account_id.map(|value| value.trim().to_string()),
    })
}

fn fetch_usage_via_oauth(credentials: &Credentials) -> Result<UsageData, PollError> {
    let agent = build_agent()?;
    let url = resolve_usage_url();
    let mut request = agent
        .get(&url)
        .set(
            "Authorization",
            &format!("Bearer {}", credentials.access_token),
        )
        .set("Accept", "application/json")
        .set(
            "User-Agent",
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        );

    if let Some(account_id) = credentials
        .account_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        request = request.set("ChatGPT-Account-Id", account_id);
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
            return Err(PollError::RequestFailed);
        }
        Err(_) => return Err(PollError::RequestFailed),
    };

    let body: UsageResponse = response.into_json().map_err(|_| PollError::RequestFailed)?;
    Ok(map_oauth_usage(body))
}

fn fetch_usage_via_rpc() -> Result<UsageData, PollError> {
    let mut client = CodexRpcClient::start()?;
    let result = (|| {
        client.initialize()?;
        let rate_limits = client.request("account/rateLimits/read", json!({}))?;
        let account = client.request("account/read", json!({})).ok();

        let limits: RpcRateLimitsEnvelope =
            serde_json::from_value(rate_limits).map_err(|_| PollError::RequestFailed)?;
        let account: Option<RpcAccountEnvelope> =
            account.and_then(|value| serde_json::from_value(value).ok());

        let mut data = UsageData {
            session: map_rpc_window(limits.rate_limits.primary),
            weekly: map_rpc_window(limits.rate_limits.secondary),
            review: None,
            credits_remaining: limits
                .rate_limits
                .credits
                .as_ref()
                .and_then(|credits| credits.balance.as_deref())
                .and_then(|value| value.parse::<f64>().ok()),
            has_credits: limits
                .rate_limits
                .credits
                .as_ref()
                .map(|credits| credits.has_credits)
                .unwrap_or(false),
            unlimited_credits: limits
                .rate_limits
                .credits
                .as_ref()
                .map(|credits| credits.unlimited)
                .unwrap_or(false),
            account_email: None,
            plan_name: None,
            source_label: Some("codex-rpc".to_string()),
            updated_at: Some(SystemTime::now()),
            opencode_windows: None,
        };

        if let Some(RpcAccount::Chatgpt { email, plan_type }) =
            account.and_then(|account| account.account)
        {
            data.account_email = email;
            data.plan_name = plan_type;
        }

        Ok(data)
    })();

    client.shutdown();
    result
}

fn map_oauth_usage(response: UsageResponse) -> UsageData {
    UsageData {
        session: map_oauth_window(
            response
                .rate_limit
                .as_ref()
                .and_then(|details| details.primary_window.as_ref()),
        ),
        weekly: map_oauth_window(
            response
                .rate_limit
                .as_ref()
                .and_then(|details| details.secondary_window.as_ref()),
        ),
        review: response
            .code_review_rate_limit
            .as_ref()
            .and_then(|details| details.primary_window.as_ref())
            .map(|window| map_oauth_window(Some(window))),
        credits_remaining: response
            .credits
            .as_ref()
            .and_then(|credits| balance_value(credits.balance.as_ref())),
        has_credits: response
            .credits
            .as_ref()
            .map(|credits| credits.has_credits)
            .unwrap_or(false),
        unlimited_credits: response
            .credits
            .as_ref()
            .map(|credits| credits.unlimited)
            .unwrap_or(false),
        account_email: response.email,
        plan_name: response.plan_type,
        source_label: Some("openai-oauth".to_string()),
        updated_at: Some(SystemTime::now()),
        opencode_windows: None,
    }
}

fn map_oauth_window(window: Option<&UsageWindow>) -> UsageSection {
    let Some(window) = window else {
        return UsageSection::default();
    };

    UsageSection {
        percentage: window.used_percent,
        resets_at: window.reset_at.and_then(unix_to_system_time),
    }
}

fn map_rpc_window(window: Option<RpcWindow>) -> UsageSection {
    let Some(window) = window else {
        return UsageSection::default();
    };

    UsageSection {
        percentage: window.used_percent,
        resets_at: window.resets_at.and_then(unix_to_system_time),
    }
}

fn build_agent() -> Result<ureq::Agent, PollError> {
    let tls = native_tls::TlsConnector::new().map_err(|_| PollError::RequestFailed)?;
    Ok(ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .tls_connector(std::sync::Arc::new(tls))
        .build())
}

fn balance_value(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn resolve_usage_url() -> String {
    let base = resolve_chatgpt_base_url();
    let trimmed = base.trim_end_matches('/');
    let path = if trimmed.contains("/backend-api") {
        USAGE_PATH_BACKEND_API
    } else {
        USAGE_PATH_CODEX_API
    };
    format!("{trimmed}{path}")
}

fn resolve_chatgpt_base_url() -> String {
    let config_path = codex_home_dir().join("config.toml");
    let config_contents = std::fs::read_to_string(config_path).ok();

    if let Some(contents) = config_contents {
        for raw_line in contents.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }

            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() != "chatgpt_base_url" {
                continue;
            }

            let mut value = value.trim().to_string();
            if value.len() >= 2
                && ((value.starts_with('"') && value.ends_with('"'))
                    || (value.starts_with('\'') && value.ends_with('\'')))
            {
                value = value[1..value.len() - 1].to_string();
            }

            if !value.is_empty() {
                if let Some(sanitized) = sanitize_chatgpt_base_url(&value) {
                    return sanitized;
                }

                diagnose::log(
                    "ignoring unsafe chatgpt_base_url from config.toml; falling back to default",
                );
            }
        }
    }

    DEFAULT_CHATGPT_BASE_URL.to_string()
}

fn sanitize_chatgpt_base_url(value: &str) -> Option<String> {
    let mut url = value.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return None;
    }

    if allow_untrusted_base_url_override() {
        return Some(url);
    }

    let host = extract_https_host(&url)?;
    if host != "chatgpt.com" && !host.ends_with(".chatgpt.com") {
        return None;
    }

    if host == "chatgpt.com" && !url.contains("/backend-api") {
        url.push_str("/backend-api");
    }

    Some(url)
}

fn allow_untrusted_base_url_override() -> bool {
    std::env::var(ALLOW_UNTRUSTED_BASE_URL_ENV)
        .map(|value| {
            let normalized = value.trim().to_ascii_lowercase();
            normalized == "1" || normalized == "true" || normalized == "yes"
        })
        .unwrap_or(false)
}

fn extract_https_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        return None;
    }

    Some(host)
}

fn codex_home_dir() -> PathBuf {
    if let Ok(path) = std::env::var("CODEX_HOME") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }

    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
}

fn unix_to_system_time(unix_secs: i64) -> Option<SystemTime> {
    if unix_secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(unix_secs as u64))
}

fn resolve_windows_codex_path() -> Option<String> {
    for name in &["codex.cmd", "codex"] {
        if let Ok(output) = Command::new("where.exe")
            .arg(name)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for line in stdout.lines() {
                    let path = line.trim();
                    if is_trusted_windows_cli_path(path) {
                        return Some(path.to_string());
                    }
                }
            }
        }
    }

    None
}

fn is_trusted_windows_cli_path(path: &str) -> bool {
    let path = path.trim().trim_matches('"');
    let candidate = PathBuf::from(path);
    if !candidate.is_absolute() || !candidate.exists() {
        return false;
    }

    let normalized_candidate = normalize_windows_path(&candidate);
    trusted_cli_roots()
        .into_iter()
        .map(|root| normalize_windows_path(&root))
        .any(|root| normalized_candidate.starts_with(&root))
}

fn trusted_cli_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(program_files) = std::env::var("ProgramFiles") {
        roots.push(PathBuf::from(program_files));
    } else {
        roots.push(PathBuf::from(r"C:\Program Files"));
    }

    if let Ok(program_files_x86) = std::env::var("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files_x86));
    } else {
        roots.push(PathBuf::from(r"C:\Program Files (x86)"));
    }

    if let Ok(appdata) = std::env::var("APPDATA") {
        roots.push(PathBuf::from(appdata).join("npm"));
    }

    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(local_app_data).join("Programs"));
    }

    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".local").join("bin"));
    }

    roots
}

fn normalize_windows_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

struct CodexRpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl CodexRpcClient {
    fn start() -> Result<Self, PollError> {
        let codex_path = resolve_windows_codex_path().ok_or(PollError::CliUnavailable)?;
        let is_cmd = codex_path.to_ascii_lowercase().ends_with(".cmd");

        let mut command = if is_cmd {
            let mut command = Command::new("cmd.exe");
            command.arg("/c").arg(&codex_path);
            command
        } else {
            Command::new(&codex_path)
        };

        command
            .args(["-s", "read-only", "-a", "untrusted", "app-server"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|_| PollError::CliUnavailable)?;
        let stdin = child.stdin.take().ok_or(PollError::CliUnavailable)?;
        let stdout = child.stdout.take().ok_or(PollError::CliUnavailable)?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> Result<(), PollError> {
        let _ = self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "opencode-usage-monitor",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )?;
        self.notify("initialized", json!({}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, PollError> {
        let id = self.next_id;
        self.next_id += 1;

        self.send(json!({
            "id": id,
            "method": method,
            "params": params,
        }))?;

        loop {
            let message = self.read_message()?;
            if message.get("id").and_then(|value| value.as_u64()) != Some(id) {
                continue;
            }

            if message.get("error").is_some() {
                return Err(PollError::RequestFailed);
            }

            return message
                .get("result")
                .cloned()
                .ok_or(PollError::RequestFailed);
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), PollError> {
        self.send(json!({
            "method": method,
            "params": params,
        }))
    }

    fn send(&mut self, value: Value) -> Result<(), PollError> {
        let payload = serde_json::to_vec(&value).map_err(|_| PollError::RequestFailed)?;
        self.stdin
            .write_all(&payload)
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|_| PollError::RequestFailed)
    }

    fn read_message(&mut self) -> Result<Value, PollError> {
        let mut line = String::new();

        loop {
            line.clear();
            let read = self
                .stdout
                .read_line(&mut line)
                .map_err(|_| PollError::RequestFailed)?;

            if read == 0 {
                return Err(PollError::CliUnavailable);
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value: Value =
                serde_json::from_str(trimmed).map_err(|_| PollError::RequestFailed)?;
            return Ok(value);
        }
    }

    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
