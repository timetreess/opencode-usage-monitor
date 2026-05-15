use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::diagnose;
use crate::history;
use crate::localization::{self, LanguageId, Strings};
use crate::models::{
    ActivitySummary, HistoryStorageMode, HistorySyncSettings, ProviderKind, UsageData,
};
use crate::native_interop::{
    self, Color, TIMER_COUNTDOWN, TIMER_POLL, TIMER_RESET_POLL, WM_APP_TRAY_ICON,
    WM_APP_USAGE_UPDATED,
};
use crate::theme;
use crate::updater::{self, InstallChannel, ReleaseDescriptor, UpdateCheckResult};
use crate::{poller, provider_adapter, secret_store};

/// Wrapper to make HWND sendable across threads (safe for PostMessage usage)
#[derive(Clone, Copy)]
struct SendHwnd(isize);

unsafe impl Send for SendHwnd {}

impl SendHwnd {
    fn from_hwnd(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
    fn to_hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

/// Shared application state
struct AppState {
    hwnd: SendHwnd,
    taskbar_hwnd: Option<HWND>,
    tray_notify_hwnd: Option<HWND>,
    tray_icon_added: bool,
    win_event_hook: Option<HWINEVENTHOOK>,
    is_dark: bool,
    embedded: bool,
    language_override: Option<LanguageId>,
    language: LanguageId,
    install_channel: InstallChannel,
    active_provider: ProviderKind,
    history_sync: HistorySyncSettings,
    claude: ProviderState,
    codex: ProviderState,
    opencode: ProviderState,

    poll_interval_ms: u32,
    retry_count: u32,
    update_status: UpdateStatus,

    tray_offset: i32,
    dragging: bool,
    drag_start_mouse_x: i32,
    drag_start_offset: i32,
}

#[derive(Clone, Debug)]
struct ProviderState {
    data: Option<UsageData>,
    activity: Option<ActivitySummary>,
    session_percent: f64,
    session_text: String,
    weekly_percent: f64,
    weekly_text: String,
    last_poll_ok: bool,
}

impl Default for ProviderState {
    fn default() -> Self {
        Self {
            data: None,
            activity: None,
            session_percent: 0.0,
            session_text: "--".to_string(),
            weekly_percent: 0.0,
            weekly_text: "--".to_string(),
            last_poll_ok: false,
        }
    }
}

#[derive(Clone, Debug)]
enum UpdateStatus {
    Idle,
    Checking,
    Applying,
    UpToDate,
    Available(ReleaseDescriptor),
}

const RETRY_BASE_MS: u32 = 30_000; // 30 seconds

const POLL_1_MIN: u32 = 60_000;
const POLL_5_MIN: u32 = 300_000;
const POLL_15_MIN: u32 = 900_000;
const POLL_1_HOUR: u32 = 3_600_000;

// Menu item IDs for update frequency
const IDM_FREQ_1MIN: u16 = 10;
const IDM_FREQ_5MIN: u16 = 11;
const IDM_FREQ_15MIN: u16 = 12;
const IDM_FREQ_1HOUR: u16 = 13;
const IDM_START_WITH_WINDOWS: u16 = 20;
const IDM_RESET_POSITION: u16 = 30;
const IDM_VERSION_ACTION: u16 = 31;
const IDM_PROVIDER_CLAUDE: u16 = 32;
const IDM_PROVIDER_CODEX: u16 = 33;
const IDM_PROVIDER_OPENCODE: u16 = 34;
const IDM_LANG_SYSTEM: u16 = 40;
const IDM_LANG_ENGLISH: u16 = 41;
const IDM_LANG_SPANISH: u16 = 42;
const IDM_LANG_FRENCH: u16 = 43;
const IDM_LANG_GERMAN: u16 = 44;
const IDM_LANG_JAPANESE: u16 = 45;
const IDM_EXPORT_HISTORY_REPORT: u16 = 46;
const IDM_ARCHIVE_HISTORY_GITHUB: u16 = 47;
const IDM_SET_ARCHIVE_REPO: u16 = 48;
const IDM_FETCH_HISTORY_GITHUB: u16 = 49;
const IDM_SETUP_HISTORY_SYNC: u16 = 50;
const IDM_INSTALL_WORKFLOW_GITHUB: u16 = 51;
const IDM_INSTALL_WORKFLOW_LOCAL: u16 = 52;
const IDM_OPEN_LOCAL_WORKFLOWS: u16 = 53;
const IDM_OPEN_APP_DATA_FOLDER: u16 = 54;
const IDM_OPEN_LOCAL_HISTORY_FOLDER: u16 = 55;
const IDM_OPEN_REPORTS_FOLDER: u16 = 56;
const IDM_OPEN_CLAUDE_SOURCE_FOLDER: u16 = 57;
const IDM_OPEN_CODEX_SOURCE_FOLDER: u16 = 58;
const IDM_SET_GITHUB_TOKEN: u16 = 59;
const IDM_CLEAR_GITHUB_TOKEN: u16 = 60;
const IDM_TOGGLE_CONVERSATION_SYNC: u16 = 61;
const CREATE_NO_WINDOW: u32 = 0x08000000;

const DIVIDER_HIT_ZONE: i32 = 13; // LEFT_DIVIDER_W + DIVIDER_RIGHT_MARGIN

const WM_DPICHANGED_MSG: u32 = 0x02E0;
const WM_APP_UPDATE_CHECK_COMPLETE: u32 = WM_APP + 2;

/// Current system DPI (96 = 100% scaling, 144 = 150%, 192 = 200%, etc.)
static CURRENT_DPI: AtomicU32 = AtomicU32::new(96);

/// Scale a base pixel value (designed at 96 DPI) to the current DPI.
fn sc(px: i32) -> i32 {
    let dpi = CURRENT_DPI.load(Ordering::Relaxed);
    (px as f64 * dpi as f64 / 96.0).round() as i32
}

/// Re-query the monitor DPI for our window and update the cached value.
/// Uses GetDpiForWindow which returns the live DPI (unlike GetDpiForSystem
/// which is cached at process startup and never changes).
fn refresh_dpi() {
    let hwnd = {
        let state = lock_state();
        state.as_ref().map(|s| s.hwnd.to_hwnd())
    };
    if let Some(hwnd) = hwnd {
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        if dpi > 0 {
            CURRENT_DPI.store(dpi, Ordering::Relaxed);
        }
    }
}

unsafe impl Send for AppState {}

static STATE: Mutex<Option<AppState>> = Mutex::new(None);

/// Lock STATE safely, recovering from poisoned mutex
fn lock_state() -> MutexGuard<'static, Option<AppState>> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn provider_state(state: &AppState, provider: ProviderKind) -> &ProviderState {
    match provider {
        ProviderKind::Claude => &state.claude,
        ProviderKind::Codex => &state.codex,
        ProviderKind::OpenCode => &state.opencode,
    }
}

fn provider_state_mut(state: &mut AppState, provider: ProviderKind) -> &mut ProviderState {
    match provider {
        ProviderKind::Claude => &mut state.claude,
        ProviderKind::Codex => &mut state.codex,
        ProviderKind::OpenCode => &mut state.opencode,
    }
}

fn active_provider_state(state: &AppState) -> &ProviderState {
    provider_state(state, state.active_provider)
}

fn active_provider_state_mut(state: &mut AppState) -> &mut ProviderState {
    provider_state_mut(state, state.active_provider)
}

fn settings_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata)
        .join("OpencodeUsageMonitor")
        .join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    tray_offset: i32,
    #[serde(default = "default_poll_interval")]
    poll_interval_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_provider: Option<ProviderKind>,
    #[serde(default)]
    history_sync: HistorySyncSettings,
}

impl Default for SettingsFile {
    fn default() -> Self {
        Self {
            tray_offset: 0,
            poll_interval_ms: default_poll_interval(),
            language: None,
            active_provider: None,
            history_sync: HistorySyncSettings::default(),
        }
    }
}

fn default_poll_interval() -> u32 {
    POLL_15_MIN
}

fn load_settings() -> SettingsFile {
    let content = match std::fs::read_to_string(settings_path()) {
        Ok(c) => c,
        Err(_) => return SettingsFile::default(),
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_settings(settings: &SettingsFile) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

fn save_state_settings() {
    let state = lock_state();
    if let Some(s) = state.as_ref() {
        save_settings(&SettingsFile {
            tray_offset: s.tray_offset,
            poll_interval_ms: s.poll_interval_ms,
            language: s
                .language_override
                .map(|language| language.code().to_string()),
            active_provider: Some(s.active_provider),
            history_sync: s.history_sync.clone(),
        });
    }
}

fn refresh_provider_usage_texts(
    provider: ProviderKind,
    provider_state: &mut ProviderState,
    strings: Strings,
) {
    if !provider_state.last_poll_ok {
        return;
    }

    let Some(data) = provider_state.data.as_ref() else {
        return;
    };

    if provider == ProviderKind::OpenCode {
        if let Some(windows) = data.opencode_windows.as_ref() {
            provider_state.session_text = format_token_window_line(&windows.five_hour);
            provider_state.weekly_text = format_token_window_line(&windows.today);
        }
    } else {
        provider_state.session_text = poller::format_line(&data.session, strings);
        provider_state.weekly_text = poller::format_line(&data.weekly, strings);
    }
}

fn refresh_usage_texts(state: &mut AppState) {
    let strings = state.language.strings();
    refresh_provider_usage_texts(ProviderKind::Claude, &mut state.claude, strings);
    refresh_provider_usage_texts(ProviderKind::Codex, &mut state.codex, strings);
    refresh_provider_usage_texts(ProviderKind::OpenCode, &mut state.opencode, strings);
}

fn set_provider_data(
    state: &mut AppState,
    provider: ProviderKind,
    data: UsageData,
    activity: Option<ActivitySummary>,
) {
    if provider == ProviderKind::OpenCode {
        write_daily_opencode_log(&data);
    }

    let provider_state = provider_state_mut(state, provider);
    provider_state.session_percent = data.session.percentage;
    provider_state.weekly_percent = data.weekly.percentage;
    provider_state.data = Some(data);
    provider_state.activity = activity;
    provider_state.last_poll_ok = true;
}

fn set_provider_loading(state: &mut AppState, provider: ProviderKind) {
    let provider_state = provider_state_mut(state, provider);
    provider_state.session_text = "...".to_string();
    provider_state.weekly_text = "...".to_string();
    provider_state.last_poll_ok = false;
}

fn set_provider_unavailable(state: &mut AppState, provider: ProviderKind) {
    let provider_state = provider_state_mut(state, provider);
    provider_state.session_text = "--".to_string();
    provider_state.weekly_text = "--".to_string();
    provider_state.last_poll_ok = false;
}

fn provider_is_selectable(provider_state: &ProviderState) -> bool {
    provider_state.last_poll_ok
        || provider_state.data.is_some()
        || provider_state.activity.is_some()
}

fn cycle_active_provider(state: &mut AppState) -> bool {
    let selectable: Vec<ProviderKind> = ProviderKind::ALL
        .into_iter()
        .filter(|provider| provider_is_selectable(provider_state(state, *provider)))
        .collect();

    if selectable.len() <= 1 {
        return false;
    }

    let current_index = selectable
        .iter()
        .position(|provider| *provider == state.active_provider)
        .unwrap_or(0);
    let next_index = (current_index + 1) % selectable.len();
    state.active_provider = selectable[next_index];
    true
}

fn provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "Claude",
        ProviderKind::Codex => "Codex",
        ProviderKind::OpenCode => "OpenCode",
    }
}

fn provider_summary_line(provider: ProviderKind, provider_state: &ProviderState) -> String {
    if provider == ProviderKind::OpenCode {
        return format!(
            "{}: 5h {} | Today {}",
            provider_name(provider),
            provider_state.session_text,
            provider_state.weekly_text
        );
    }

    format!(
        "{}: 5h {} | 7d {}",
        provider_name(provider),
        provider_state.session_text,
        provider_state.weekly_text
    )
}

fn provider_raw_usage_line(provider: ProviderKind, data: &UsageData) -> String {
    if provider == ProviderKind::OpenCode {
        if let Some(windows) = data.opencode_windows.as_ref() {
            return format!(
                "OpenCode month: {} in / {} out / {} reasoning / {} cache · {}",
                format_token_count(windows.month.input_tokens),
                format_token_count(windows.month.output_tokens),
                format_token_count(windows.month.reasoning_output_tokens),
                format_token_count(windows.month.cached_input_tokens),
                format_cost(windows.month.estimated_cost_usd)
            );
        }
    }

    let s_used = data.session.percentage.clamp(0.0, 100.0);
    let s_remaining = (100.0 - s_used).max(0.0);
    let w_used = data.weekly.percentage.clamp(0.0, 100.0);
    let w_remaining = (100.0 - w_used).max(0.0);

    format!(
        "{} raw: 5h used {:.0}% rem {:.0}% | 7d used {:.0}% rem {:.0}%",
        provider_name(provider),
        s_used,
        s_remaining,
        w_used,
        w_remaining
    )
}

fn opencode_detail_lines(data: &UsageData) -> Vec<String> {
    let Some(windows) = data.opencode_windows.as_ref() else {
        return Vec::new();
    };

    vec![
        format_opencode_detail_line("OpenCode 5h", &windows.five_hour),
        format_opencode_detail_line("OpenCode today", &windows.today),
        format_opencode_detail_line("OpenCode month", &windows.month),
    ]
}

fn format_opencode_detail_line(label: &str, usage: &crate::models::TokenWindowUsage) -> String {
    format!(
        "{label}: {} total ({} in, {} out, {} reasoning, {} cache) · {}",
        format_token_count(usage.total_tokens()),
        format_token_count(usage.input_tokens),
        format_token_count(usage.output_tokens),
        format_token_count(usage.reasoning_output_tokens),
        format_token_count(usage.cached_input_tokens),
        format_cost(usage.estimated_cost_usd)
    )
}

fn format_token_window_line(usage: &crate::models::TokenWindowUsage) -> String {
    format!(
        "{} {}",
        format_token_count(usage.total_tokens()),
        format_cost(usage.estimated_cost_usd)
    )
}

fn format_token_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn format_cost(value: f64) -> String {
    if value <= 0.0 {
        "$0.00".to_string()
    } else if value < 0.01 {
        format!("${value:.4}")
    } else {
        format!("${value:.2}")
    }
}

fn write_daily_opencode_log(data: &UsageData) {
    let Some(windows) = data.opencode_windows.as_ref() else {
        return;
    };

    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let folder = daily_log_folder_path();
    if let Err(error) = std::fs::create_dir_all(&folder) {
        diagnose::log(format!(
            "OpenCode daily log folder create failed for {}: {error}",
            folder.display()
        ));
        return;
    }

    let source = data.source_label.as_deref().unwrap_or("opencode-db");
    let generated_at = chrono::Local::now().to_rfc3339();
    let markdown = format!(
        "# OpenCode Token Usage - {today}\n\n\
Generated: {generated_at}\n\
Source: `{source}`\n\n\
| Window | Total | Input | Output | Reasoning | Cache | Estimated Cost |\n\
|---|---:|---:|---:|---:|---:|---:|\n\
| 5h | {} | {} | {} | {} | {} | {} |\n\
| Today | {} | {} | {} | {} | {} | {} |\n\
| Month | {} | {} | {} | {} | {} | {} |\n",
        format_token_count(windows.five_hour.total_tokens()),
        format_token_count(windows.five_hour.input_tokens),
        format_token_count(windows.five_hour.output_tokens),
        format_token_count(windows.five_hour.reasoning_output_tokens),
        format_token_count(windows.five_hour.cached_input_tokens),
        format_cost(windows.five_hour.estimated_cost_usd),
        format_token_count(windows.today.total_tokens()),
        format_token_count(windows.today.input_tokens),
        format_token_count(windows.today.output_tokens),
        format_token_count(windows.today.reasoning_output_tokens),
        format_token_count(windows.today.cached_input_tokens),
        format_cost(windows.today.estimated_cost_usd),
        format_token_count(windows.month.total_tokens()),
        format_token_count(windows.month.input_tokens),
        format_token_count(windows.month.output_tokens),
        format_token_count(windows.month.reasoning_output_tokens),
        format_token_count(windows.month.cached_input_tokens),
        format_cost(windows.month.estimated_cost_usd),
    );

    let json = format!(
        "{{\n\
  \"date\": \"{}\",\n\
  \"generated_at\": \"{}\",\n\
  \"source\": \"{}\",\n\
  \"five_hour\": {},\n\
  \"today\": {},\n\
  \"month\": {}\n\
}}\n",
        json_escape(&today),
        json_escape(&generated_at),
        json_escape(source),
        token_window_json(&windows.five_hour),
        token_window_json(&windows.today),
        token_window_json(&windows.month),
    );

    for (extension, content) in [("md", markdown), ("json", json)] {
        let path = folder.join(format!("{today}.{extension}"));
        if let Err(error) = std::fs::write(&path, content) {
            diagnose::log(format!(
                "OpenCode daily log write failed for {}: {error}",
                path.display()
            ));
        }
    }
}

fn token_window_json(usage: &crate::models::TokenWindowUsage) -> String {
    format!(
        "{{\"total_tokens\":{},\"input_tokens\":{},\"output_tokens\":{},\"reasoning_output_tokens\":{},\"cached_input_tokens\":{},\"estimated_cost_usd\":{:.6},\"unpriced_rows\":{}}}",
        usage.total_tokens(),
        usage.input_tokens,
        usage.output_tokens,
        usage.reasoning_output_tokens,
        usage.cached_input_tokens,
        usage.estimated_cost_usd,
        usage.unpriced_rows
    )
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn credits_text(data: &UsageData) -> Option<String> {
    if data.unlimited_credits {
        return Some("Credits: unlimited".to_string());
    }

    if data.has_credits || data.credits_remaining.unwrap_or(0.0) > 0.0 {
        return Some(format!(
            "Credits: {:.0}",
            data.credits_remaining.unwrap_or_default()
        ));
    }

    None
}

fn activity_lines(activity: &ActivitySummary) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Prompts: {} today | {} / 7d",
            activity.prompts_last_24h, activity.prompts_last_7d
        ),
        format!(
            "Sessions: {} today | {} / 7d",
            activity.sessions_last_24h, activity.sessions_last_7d
        ),
    ];

    if let Some(last_prompt) = activity
        .last_prompt
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        let suffix = activity
            .last_prompt_at
            .map(relative_time_text)
            .map(|text| format!(" ({text})"))
            .unwrap_or_default();
        lines.push(format!(
            "Last prompt: {}{}",
            trim_for_menu(last_prompt, 60),
            suffix
        ));
    }

    lines
}

fn trim_for_menu(value: &str, max_chars: usize) -> String {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    let mut trimmed = String::new();
    for ch in normalized.chars().take(max_chars.saturating_sub(1)) {
        trimmed.push(ch);
    }
    trimmed.push('…');
    trimmed
}

fn relative_time_text(timestamp: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(timestamp)
        .unwrap_or_default()
        .as_secs();

    if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3_600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    }
}

fn append_disabled_menu_line(menu: HMENU, text: &str) {
    unsafe {
        let label = native_interop::wide_str(text);
        let _ = AppendMenuW(menu, MF_GRAYED, 0, PCWSTR::from_raw(label.as_ptr()));
    }
}

fn set_window_title(hwnd: HWND, strings: Strings) {
    unsafe {
        let title = native_interop::wide_str(strings.window_title);
        let _ = SetWindowTextW(hwnd, PCWSTR::from_raw(title.as_ptr()));
    }
}

fn show_info_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

fn show_error_message(hwnd: HWND, title: &str, message: &str) {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        let _ = MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn ask_yes_no(hwnd: HWND, title: &str, message: &str) -> bool {
    unsafe {
        let title_wide = native_interop::wide_str(title);
        let message_wide = native_interop::wide_str(message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn prompt_for_text(title: &str, prompt: &str, default_value: &str) -> Option<String> {
    let script = format!(
        r#"
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$form = New-Object Windows.Forms.Form
$form.Text = '{title}'
$form.StartPosition = 'CenterScreen'
$form.Size = New-Object Drawing.Size(700, 175)
$form.TopMost = $true
$form.FormBorderStyle = [Windows.Forms.FormBorderStyle]::FixedDialog
$form.MaximizeBox = $false
$form.MinimizeBox = $false

$label = New-Object Windows.Forms.Label
$label.Text = '{prompt}'
$label.Location = New-Object Drawing.Point(14, 14)
$label.Size = New-Object Drawing.Size(656, 36)
$form.Controls.Add($label)

$textbox = New-Object Windows.Forms.TextBox
$textbox.Location = New-Object Drawing.Point(14, 58)
$textbox.Size = New-Object Drawing.Size(656, 26)
$textbox.Text = '{default_value}'
$form.Controls.Add($textbox)

$ok = New-Object Windows.Forms.Button
$ok.Text = 'OK'
$ok.Location = New-Object Drawing.Point(514, 100)
$ok.Size = New-Object Drawing.Size(75, 28)
$ok.DialogResult = [Windows.Forms.DialogResult]::OK
$form.Controls.Add($ok)

$cancel = New-Object Windows.Forms.Button
$cancel.Text = 'Cancel'
$cancel.Location = New-Object Drawing.Point(595, 100)
$cancel.Size = New-Object Drawing.Size(75, 28)
$cancel.DialogResult = [Windows.Forms.DialogResult]::Cancel
$form.Controls.Add($cancel)

$form.AcceptButton = $ok
$form.CancelButton = $cancel

if ($form.ShowDialog() -eq [Windows.Forms.DialogResult]::OK) {{
    [Console]::OutputEncoding = [System.Text.Encoding]::UTF8
    Write-Output $textbox.Text
    exit 0
}}

exit 1
"#,
        title = powershell_literal(title),
        prompt = powershell_literal(prompt),
        default_value = powershell_literal(default_value),
    );

    let output = Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(script)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn powershell_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn effective_archive_repo_url(repo_override: Option<&str>) -> String {
    history::effective_archive_repo_url(repo_override)
}

fn archive_repo_line(sync: &HistorySyncSettings) -> String {
    let effective = effective_archive_repo_url(sync.repo_url.as_deref());
    if sync
        .repo_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        format!("Archive repo: {}", trim_for_menu(&effective, 58))
    } else {
        format!("Archive repo: default {}", trim_for_menu(&effective, 49))
    }
}

fn archive_branch_line(sync: &HistorySyncSettings) -> String {
    match sync
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(branch) => format!("Archive branch: {}", trim_for_menu(branch, 54)),
        None => "Archive branch: default".to_string(),
    }
}

fn local_repo_line(sync: &HistorySyncSettings) -> String {
    match sync
        .local_repo_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(path) => format!("Local repo: {}", trim_for_menu(path, 56)),
        None => "Local repo: not set".to_string(),
    }
}

fn github_token_line() -> String {
    if archive_token_from_env().is_some() {
        "PAT: env".to_string()
    } else if secret_store::has_github_pat() {
        "PAT: saved securely".to_string()
    } else {
        "PAT: not set".to_string()
    }
}

fn archive_token_from_env() -> Option<String> {
    ["CCUM_GITHUB_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn archive_token_from_sources() -> Option<String> {
    archive_token_from_env().or_else(secret_store::load_github_pat)
}

fn effective_local_repo_path(sync: &HistorySyncSettings) -> Option<String> {
    sync.local_repo_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn prompt_local_repo_path(current: Option<&str>) -> Option<String> {
    let default_path = current.unwrap_or("");
    prompt_for_text(
        "Local Workflow Repo",
        "Local git repository path where the sample workflow should be installed or opened. No administrator access is required; the folder just needs to be writable.",
        default_path,
    )
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
}

fn open_in_explorer(path: &std::path::Path) -> Result<(), String> {
    Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .map_err(|error| format!("Unable to open {} in Explorer: {error}", path.display()))?;
    Ok(())
}

fn app_data_folder_path() -> PathBuf {
    settings_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn local_history_folder_path() -> PathBuf {
    app_data_folder_path().join("history")
}

fn daily_log_folder_path() -> PathBuf {
    app_data_folder_path().join("log")
}

fn reports_folder_path() -> PathBuf {
    app_data_folder_path().join("reports")
}

fn claude_source_folder_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude").join("projects"))
}

fn codex_source_folder_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".codex").join("sessions"))
}

fn open_or_create_folder(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Unable to create {}: {error}", path.display()))?;
    open_in_explorer(path)
}

fn open_existing_folder(path: &std::path::Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("Folder does not exist: {}", path.display()));
    }
    open_in_explorer(path)
}

fn resolve_local_repo_path(current_sync: &HistorySyncSettings) -> Option<String> {
    effective_local_repo_path(current_sync)
        .or_else(|| prompt_local_repo_path(current_sync.local_repo_path.as_deref()))
}

fn run_sync_setup(hwnd: HWND, current: &HistorySyncSettings) -> Option<HistorySyncSettings> {
    let repo_default = current
        .repo_url
        .clone()
        .unwrap_or_else(|| history::DEFAULT_ARCHIVE_REPO_URL.to_string());
    let repo_value = prompt_for_text(
        "GitHub Sync Setup",
        "GitHub repo in owner/repo, https://github.com/owner/repo.git, or git@github.com:owner/repo.git form. Leave blank to use the default archive repo.",
        &repo_default,
    )?;
    let repo_url = if repo_value.trim().is_empty()
        || repo_value
            .trim()
            .eq_ignore_ascii_case(history::DEFAULT_ARCHIVE_REPO_URL)
    {
        None
    } else {
        Some(repo_value.trim().to_string())
    };
    let branch_value = prompt_for_text(
        "GitHub Sync Setup",
        "Branch to sync against. Leave blank to use the repository default branch.",
        current.branch.as_deref().unwrap_or(""),
    )?;
    let branch = {
        let trimmed = branch_value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };
    let local_repo_path = prompt_local_repo_path(current.local_repo_path.as_deref());

    let use_remote_only = ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "Use GitHub as the primary history store? Yes keeps only source logs locally and avoids persisting the app's own history database on disk.",
    );
    let upload_history_store = ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "Upload the compact history store to GitHub? Recommended: Yes.",
    );
    let upload_html_reports = ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "Upload generated HTML usage reports to GitHub? Recommended: Yes if you want a browser-ready snapshot.",
    );
    let prefer_remote_reports = ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "When exporting reports, prefer the GitHub copy first? Choose No to always generate reports locally.",
    );
    let wants_conversation_sync = ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "Allow raw conversation/session log syncing? Recommended: No. This is privacy-heavy and not lightweight.",
    );
    if wants_conversation_sync {
        show_info_message(
            hwnd,
            "GitHub Sync Setup",
            "Raw conversation syncing is enabled for this configuration. It will upload .claude and .codex session logs to GitHub and may be slower on large histories.",
        );
    }
    let workflow_prompt = if ask_yes_no(
        hwnd,
        "GitHub Sync Setup",
        "Trigger a GitHub Actions workflow after sync to build reports on GitHub? This requires a workflow file in the target repo and Actions/Workflows permission.",
    ) {
        prompt_for_text(
            "Workflow File",
            "Workflow file name to dispatch, for example build-history-report.yml. Leave blank to skip remote workflow dispatch.",
            current
                .workflow_file
                .as_deref()
                .unwrap_or(history::DEFAULT_WORKFLOW_FILE_NAME),
        )
    } else {
        Some(String::new())
    }?;

    Some(HistorySyncSettings {
        repo_url,
        branch,
        local_repo_path,
        upload_history_store,
        upload_html_reports,
        upload_conversations: wants_conversation_sync,
        prefer_remote_reports,
        workflow_file: {
            let trimmed = workflow_prompt.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        },
        storage_mode: if use_remote_only {
            HistoryStorageMode::RemoteOnly
        } else {
            HistoryStorageMode::Local
        },
    })
}

fn sync_mode_line(sync: &HistorySyncSettings) -> String {
    let mode = match sync.storage_mode {
        HistoryStorageMode::Local => "Local store + GitHub sync",
        HistoryStorageMode::RemoteOnly => "Remote-first, no local app store",
    };
    format!("Sync mode: {mode}")
}

fn sync_scopes_line(sync: &HistorySyncSettings) -> String {
    let mut scopes = Vec::new();
    if sync.upload_history_store {
        scopes.push("history store");
    }
    if sync.upload_html_reports {
        scopes.push("html reports");
    }
    if sync.upload_conversations {
        scopes.push("conversations");
    }
    if scopes.is_empty() {
        "GitHub scopes: none".to_string()
    } else {
        format!("GitHub scopes: {}", scopes.join(", "))
    }
}

fn sync_report_source_line(sync: &HistorySyncSettings) -> String {
    if sync.prefer_remote_reports {
        "Reports: prefer GitHub copy".to_string()
    } else {
        "Reports: generate locally".to_string()
    }
}

fn show_update_prompt(hwnd: HWND, strings: Strings, release: &ReleaseDescriptor) -> bool {
    let message = strings
        .update_prompt_now
        .replace("{version}", &release.latest_version);

    unsafe {
        let title_wide = native_interop::wide_str(strings.update_available);
        let message_wide = native_interop::wide_str(&message);
        MessageBoxW(
            hwnd,
            PCWSTR::from_raw(message_wide.as_ptr()),
            PCWSTR::from_raw(title_wide.as_ptr()),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn apply_language_to_state(state: &mut AppState, language_override: Option<LanguageId>) {
    state.language_override = language_override;
    state.language = localization::resolve_language(language_override);
    set_window_title(state.hwnd.to_hwnd(), state.language.strings());
    refresh_usage_texts(state);
}

fn update_language_change() -> bool {
    let mut state = lock_state();
    let Some(app_state) = state.as_mut() else {
        return false;
    };

    if app_state.language_override.is_some() {
        return false;
    }

    let new_language = localization::detect_system_language();
    if new_language == app_state.language {
        return false;
    }

    apply_language_to_state(app_state, None);
    true
}

fn version_action_label(
    strings: Strings,
    language: LanguageId,
    install_channel: InstallChannel,
    status: &UpdateStatus,
) -> String {
    let current = env!("CARGO_PKG_VERSION");
    if install_channel == InstallChannel::Winget {
        return format!("v{current} - {}", localization::update_via_winget(language));
    }

    match status {
        UpdateStatus::Idle => format!("v{current} - {}", strings.check_for_updates),
        UpdateStatus::Checking => format!("v{current} - {}", strings.checking_for_updates),
        UpdateStatus::Applying => format!("v{current} - {}", strings.applying_update),
        UpdateStatus::UpToDate => format!("v{current} - {}", strings.up_to_date_short),
        UpdateStatus::Available(release) => {
            format!(
                "v{current} - {} v{}",
                strings.update_to, release.latest_version
            )
        }
    }
}

fn begin_update_check(hwnd: HWND) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Checking;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::check_for_updates() {
            Ok(UpdateCheckResult::UpToDate) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::UpToDate;
                    }
                }
                show_info_message(hwnd, strings.updates, strings.up_to_date);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Ok(UpdateCheckResult::Available(release)) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release.clone());
                    }
                }
                if show_update_prompt(hwnd, strings, &release) {
                    begin_update_apply(hwnd, release);
                }
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Idle;
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_update_apply(hwnd: HWND, release: ReleaseDescriptor) {
    let send_hwnd = SendHwnd::from_hwnd(hwnd);
    let strings = {
        let mut state = lock_state();
        let Some(app_state) = state.as_mut() else {
            return;
        };

        if matches!(
            app_state.update_status,
            UpdateStatus::Checking | UpdateStatus::Applying
        ) {
            show_info_message(
                hwnd,
                app_state.language.strings().updates,
                app_state.language.strings().update_in_progress,
            );
            return;
        }

        app_state.update_status = UpdateStatus::Applying;
        app_state.language.strings()
    };

    std::thread::spawn(move || {
        let hwnd = send_hwnd.to_hwnd();
        match updater::begin_self_update(&release) {
            Ok(()) => unsafe {
                diagnose::log(
                    "update helper launched; closing app so the helper can replace and restart it",
                );
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Err(error) => {
                {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        s.update_status = UpdateStatus::Available(release);
                    }
                }
                let message = format!("{}.\n\n{}", strings.update_failed, error);
                show_error_message(hwnd, strings.updates, &message);
                unsafe {
                    let _ = PostMessageW(hwnd, WM_APP_UPDATE_CHECK_COMPLETE, WPARAM(0), LPARAM(0));
                }
            }
        }
    });
}

fn begin_winget_update(hwnd: HWND) {
    let strings = {
        let state = lock_state();
        state.as_ref().map(|s| s.language.strings())
    }
    .unwrap_or(LanguageId::English.strings());

    if let Err(error) = updater::begin_winget_update() {
        let message = format!("{}.\n\n{}", strings.update_failed, error);
        show_error_message(hwnd, strings.updates, &message);
    }
}

const STARTUP_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REGISTRY_KEY: &str = "OpencodeUsageMonitor";

/// Returns true only if the startup registry value points to this executable.
fn is_startup_enabled() -> bool {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);
        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if result.is_err() {
            return false;
        }

        // Query the size of the value
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            None,
            Some(&mut data_size),
        );
        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(hkey);
            return false;
        }

        // Read the value
        let mut buf = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(key_name.as_ptr()),
            None,
            None,
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        );
        let _ = RegCloseKey(hkey);
        if result.is_err() {
            return false;
        }

        // Convert the registry value (UTF-16) to a string
        let wide_slice =
            std::slice::from_raw_parts(buf.as_ptr() as *const u16, data_size as usize / 2);
        let reg_value = String::from_utf16_lossy(wide_slice)
            .trim_end_matches('\0')
            .to_string();

        // Get the current executable path
        let mut exe_buf = [0u16; 260];
        let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
        if len == 0 {
            return false;
        }
        let current_exe = String::from_utf16_lossy(&exe_buf[..len]);
        let expected_command = startup_command_for_executable(&current_exe);

        // Case-insensitive comparison (Windows paths are case-insensitive)
        reg_value.eq_ignore_ascii_case(&current_exe)
            || reg_value.eq_ignore_ascii_case(&expected_command)
    }
}

fn set_startup_enabled(enable: bool) {
    unsafe {
        let path = native_interop::wide_str(STARTUP_REGISTRY_PATH);

        let mut hkey = HKEY::default();
        let result = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(path.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        );
        if result.is_err() {
            return;
        }

        let key_name = native_interop::wide_str(STARTUP_REGISTRY_KEY);

        if enable {
            let mut exe_buf = [0u16; 260];
            let len = GetModuleFileNameW(None, &mut exe_buf) as usize;
            if len > 0 {
                let current_exe = String::from_utf16_lossy(&exe_buf[..len]);
                let startup_value = startup_command_for_executable(&current_exe);
                let startup_wide = native_interop::wide_str(&startup_value);
                let byte_len = (startup_wide.len() * 2) as u32;
                let _ = RegSetValueExW(
                    hkey,
                    PCWSTR::from_raw(key_name.as_ptr()),
                    0,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        startup_wide.as_ptr() as *const u8,
                        byte_len as usize,
                    )),
                );
            }
        } else {
            let _ = RegDeleteValueW(hkey, PCWSTR::from_raw(key_name.as_ptr()));
        }

        let _ = RegCloseKey(hkey);
    }
}

fn startup_command_for_executable(exe_path: &str) -> String {
    format!("\"{exe_path}\"")
}

// Dimensions matching the C# version
const SEGMENT_W: i32 = 10;
const SEGMENT_H: i32 = 13;
const SEGMENT_GAP: i32 = 1;
const SEGMENT_COUNT: i32 = 10;
const CORNER_RADIUS: i32 = 2;

const LEFT_DIVIDER_W: i32 = 3;
const DIVIDER_RIGHT_MARGIN: i32 = 10;
const PROVIDER_BADGE_W: i32 = 26;
const PROVIDER_BADGE_RIGHT_MARGIN: i32 = 8;
const LABEL_WIDTH: i32 = 18;
const LABEL_RIGHT_MARGIN: i32 = 10;
const BAR_RIGHT_MARGIN: i32 = 4;
const TEXT_WIDTH: i32 = 82;
const RIGHT_MARGIN: i32 = 1;
const WIDGET_HEIGHT: i32 = 46;

fn total_widget_width() -> i32 {
    sc(LEFT_DIVIDER_W)
        + sc(DIVIDER_RIGHT_MARGIN)
        + sc(PROVIDER_BADGE_W)
        + sc(PROVIDER_BADGE_RIGHT_MARGIN)
        + sc(LABEL_WIDTH)
        + sc(LABEL_RIGHT_MARGIN)
        + (sc(SEGMENT_W) + sc(SEGMENT_GAP)) * SEGMENT_COUNT
        - sc(SEGMENT_GAP)
        + sc(BAR_RIGHT_MARGIN)
        + sc(TEXT_WIDTH)
        + sc(RIGHT_MARGIN)
}

fn provider_accent(provider: ProviderKind) -> Color {
    match provider {
        ProviderKind::Claude => Color::from_hex("#D97757"),
        ProviderKind::Codex => Color::from_hex("#10A37F"),
        ProviderKind::OpenCode => Color::from_hex("#4F8CFF"),
    }
}

fn provider_badge_palette(provider: ProviderKind) -> (Color, Color, Color) {
    match provider {
        ProviderKind::Claude => (
            Color::from_hex("#D97757"), // fill
            Color::from_hex("#8E4A36"), // border
            Color::from_hex("#FFFFFF"), // text
        ),
        ProviderKind::Codex => (
            Color::from_hex("#10A37F"), // fill
            Color::from_hex("#0A6A52"), // border
            Color::from_hex("#FFFFFF"), // text
        ),
        ProviderKind::OpenCode => (
            Color::from_hex("#4F8CFF"), // fill
            Color::from_hex("#2459B8"), // border
            Color::from_hex("#FFFFFF"), // text
        ),
    }
}

pub fn run() {
    // Enable Per-Monitor DPI Awareness V2 for crisp rendering at any scale factor
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CURRENT_DPI.store(GetDpiForSystem(), Ordering::Relaxed);
    }
    diagnose::log("window::run started");

    // Single-instance guard: silently exit if another instance is running
    let mutex_name = native_interop::wide_str("Global\\OpencodeUsageMonitor");
    let _mutex = unsafe {
        let handle = CreateMutexW(None, false, PCWSTR::from_raw(mutex_name.as_ptr()));
        match handle {
            Ok(h) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    diagnose::log("startup aborted: another instance is already running");
                    return;
                }
                h
            }
            Err(error) => {
                diagnose::log_error(
                    "startup aborted: unable to create single-instance mutex",
                    error,
                );
                return;
            }
        }
    };

    let class_name = native_interop::wide_str("OpencodeUsageMonitor");

    unsafe {
        let hinstance = match GetModuleHandleW(PCWSTR::null()) {
            Ok(handle) => handle,
            Err(error) => {
                diagnose::log_error("startup aborted: unable to get module handle", error);
                return;
            }
        };

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            lpszClassName: PCWSTR::from_raw(class_name.as_ptr()),
            ..Default::default()
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            diagnose::log("RegisterClassExW returned 0");
        }

        let settings = load_settings();
        let language_override = settings.language.as_deref().and_then(LanguageId::from_code);
        let language = localization::resolve_language(language_override);
        let install_channel = updater::current_install_channel();
        let active_provider = settings.active_provider.unwrap_or(ProviderKind::OpenCode);
        let history_sync = settings.history_sync.clone();

        // Create as layered popup (will be reparented into taskbar)
        let title = native_interop::wide_str(language.strings().window_title);
        let hwnd = match CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            total_widget_width(),
            sc(WIDGET_HEIGHT),
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                diagnose::log_error("startup aborted: unable to create main window", error);
                return;
            }
        };
        diagnose::log(format!("main window created hwnd={:?}", hwnd));

        let is_dark = theme::is_dark_mode();
        let mut embedded = false;

        {
            let mut state = lock_state();
            *state = Some(AppState {
                hwnd: SendHwnd::from_hwnd(hwnd),
                taskbar_hwnd: None,
                tray_notify_hwnd: None,
                tray_icon_added: false,
                win_event_hook: None,
                is_dark,
                embedded: false,
                language_override,
                language,
                install_channel,
                active_provider,
                history_sync,
                claude: ProviderState::default(),
                codex: ProviderState::default(),
                opencode: ProviderState::default(),
                poll_interval_ms: settings.poll_interval_ms,
                retry_count: 0,
                update_status: UpdateStatus::Idle,
                tray_offset: settings.tray_offset,
                dragging: false,
                drag_start_mouse_x: 0,
                drag_start_offset: 0,
            });
        }

        // Try to embed in taskbar
        if let Some(taskbar_hwnd) = native_interop::find_taskbar() {
            diagnose::log(format!("taskbar found hwnd={:?}", taskbar_hwnd));
            native_interop::embed_in_taskbar(hwnd, taskbar_hwnd);
            embedded = true;

            let mut state = lock_state();
            let Some(s) = state.as_mut() else {
                diagnose::log("startup aborted: app state missing after window creation");
                return;
            };
            s.taskbar_hwnd = Some(taskbar_hwnd);
            s.embedded = true;

            let tray_notify = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd");
            s.tray_notify_hwnd = tray_notify;
            if tray_notify.is_some() {
                diagnose::log("TrayNotifyWnd found");
            } else {
                diagnose::log("TrayNotifyWnd not found");
            }

            if let Some(tray_hwnd) = tray_notify {
                let thread_id = native_interop::get_window_thread_id(tray_hwnd);
                let hook = native_interop::set_tray_event_hook(thread_id, on_tray_location_changed);
                s.win_event_hook = hook;
                if hook.is_some() {
                    diagnose::log("tray event hook installed");
                } else {
                    diagnose::log("tray event hook could not be installed");
                }
            }
        } else {
            diagnose::log("taskbar not found; using fallback popup window");
        }

        // If not embedded, fall back to topmost popup with SetLayeredWindowAttributes
        if !embedded {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        // Position and show
        position_at_taskbar();
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        diagnose::log("window shown");
        refresh_tray_icon();

        // Initial render via UpdateLayeredWindow (for embedded) or InvalidateRect (fallback)
        render_layered();

        // Poll timer: 15 minutes
        let initial_poll_ms = {
            let state = lock_state();
            state
                .as_ref()
                .map(|s| s.poll_interval_ms)
                .unwrap_or(POLL_15_MIN)
        };
        SetTimer(hwnd, TIMER_POLL, initial_poll_ms, None);

        // Initial poll
        let send_hwnd = SendHwnd::from_hwnd(hwnd);
        std::thread::spawn(move || {
            diagnose::log("initial poll thread started");
            do_poll(send_hwnd);
        });

        // Initial theme check
        check_theme_change();

        // Message loop
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Render widget content and push to the layered window via UpdateLayeredWindow.
/// Renders fully opaque with the actual taskbar background colour so that
/// ClearType sub-pixel font rendering can be used for crisp, OS-native text.
fn render_layered() {
    refresh_dpi();
    let (
        hwnd_val,
        is_dark,
        embedded,
        strings,
        active_provider,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
    ) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else { return };
        let provider_state = active_provider_state(s);
        (
            s.hwnd,
            s.is_dark,
            s.embedded,
            s.language.strings(),
            s.active_provider,
            provider_state.session_percent,
            provider_state.session_text.clone(),
            provider_state.weekly_percent,
            provider_state.weekly_text.clone(),
        )
    };

    let hwnd = hwnd_val.to_hwnd();

    // For non-embedded fallback, just invalidate and let WM_PAINT handle it
    if !embedded {
        unsafe {
            let _ = InvalidateRect(hwnd, None, false);
        }
        return;
    }

    let width = total_widget_width();
    let height = sc(WIDGET_HEIGHT);

    let accent = provider_accent(active_provider);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let screen_dc = GetDC(hwnd);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let mem_dc = CreateCompatibleDC(screen_dc);
        let dib =
            CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).unwrap_or_default();

        if dib.is_invalid() || bits.is_null() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(hwnd, screen_dc);
            return;
        }

        let old_bmp = SelectObject(mem_dc, dib);
        let pixel_count = (width * height) as usize;

        // Render once with the actual taskbar background colour.
        // Using an opaque background lets us use CLEARTYPE_QUALITY for
        // sub-pixel font rendering that matches the rest of the OS.
        paint_content(
            mem_dc,
            PaintContentArgs {
                width,
                height,
                is_dark,
                bg: &bg_color,
                text_color: &text_color,
                accent: &accent,
                track: &track,
                strings,
                active_provider,
                session_pct,
                session_text: &session_text,
                weekly_pct,
                weekly_text: &weekly_text,
            },
        );

        // Background pixels → alpha 1 (nearly invisible but still hittable for right-click).
        // Content pixels → fully opaque (preserves ClearType sub-pixel rendering).
        let bg_bgr = bg_color.to_colorref();
        let pixel_data = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
        for px in pixel_data.iter_mut() {
            let rgb = *px & 0x00FFFFFF;
            if rgb == bg_bgr {
                *px = 0x01000000;
            } else {
                *px = rgb | 0xFF000000;
            }
        }

        // Push to window via UpdateLayeredWindow
        let pt_src = POINT { x: 0, y: 0 };
        let sz = SIZE {
            cx: width,
            cy: height,
        };
        let blend = BLENDFUNCTION {
            BlendOp: 0, // AC_SRC_OVER
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 1, // AC_SRC_ALPHA
        };

        let _ = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            None,
            Some(&sz),
            mem_dc,
            Some(&pt_src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );

        // Cleanup
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(hwnd, screen_dc);
    }
}

/// Paint all widget content onto a DC with a given background color.
struct PaintContentArgs<'a> {
    width: i32,
    height: i32,
    is_dark: bool,
    bg: &'a Color,
    text_color: &'a Color,
    accent: &'a Color,
    track: &'a Color,
    strings: Strings,
    active_provider: ProviderKind,
    session_pct: f64,
    session_text: &'a str,
    weekly_pct: f64,
    weekly_text: &'a str,
}

fn paint_content(hdc: HDC, args: PaintContentArgs<'_>) {
    let PaintContentArgs {
        width,
        height,
        is_dark,
        bg,
        text_color,
        accent,
        track,
        strings,
        active_provider,
        session_pct,
        session_text,
        weekly_pct,
        weekly_text,
    } = args;

    unsafe {
        let client_rect = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };

        let bg_brush = CreateSolidBrush(COLORREF(bg.to_colorref()));
        FillRect(hdc, &client_rect, bg_brush);
        let _ = DeleteObject(bg_brush);

        // Left divider
        let divider_h = sc(25);
        let divider_top = (height - divider_h) / 2;
        let divider_bottom = divider_top + divider_h;

        let (div_left, div_right) = if is_dark {
            ((80, 80, 80), (40, 40, 40))
        } else {
            ((160, 160, 160), (230, 230, 230))
        };

        let left_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_left.0, div_left.1, div_left.2,
        )));
        let left_rect = RECT {
            left: 0,
            top: divider_top,
            right: sc(2),
            bottom: divider_bottom,
        };
        FillRect(hdc, &left_rect, left_brush);
        let _ = DeleteObject(left_brush);

        let right_brush = CreateSolidBrush(COLORREF(native_interop::colorref(
            div_right.0,
            div_right.1,
            div_right.2,
        )));
        let right_rect = RECT {
            left: sc(2),
            top: divider_top,
            right: sc(3),
            bottom: divider_bottom,
        };
        FillRect(hdc, &right_rect, right_brush);
        let _ = DeleteObject(right_brush);

        let content_x = sc(LEFT_DIVIDER_W) + sc(DIVIDER_RIGHT_MARGIN);
        let badge_rect = RECT {
            left: content_x,
            top: (height - sc(24)) / 2,
            right: content_x + sc(PROVIDER_BADGE_W),
            bottom: (height - sc(24)) / 2 + sc(24),
        };
        draw_provider_badge(hdc, &badge_rect, active_provider, accent);

        let rows_x = content_x + sc(PROVIDER_BADGE_W) + sc(PROVIDER_BADGE_RIGHT_MARGIN);
        let row1_y = sc(5);
        let row2_y = sc(5) + sc(SEGMENT_H) + sc(10);

        let _ = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text_color.to_colorref()));

        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_MEDIUM.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);

        let (row1_label, row2_label) = if active_provider == ProviderKind::OpenCode {
            ("5h", "Day")
        } else {
            (strings.session_window, strings.weekly_window)
        };

        draw_row(
            hdc,
            rows_x,
            row1_y,
            RowRender {
                label: row1_label,
                percent: session_pct,
                text: session_text,
            },
            RowColors { accent, track },
        );
        draw_row(
            hdc,
            rows_x,
            row2_y,
            RowRender {
                label: row2_label,
                percent: weekly_pct,
                text: weekly_text,
            },
            RowColors { accent, track },
        );

        SelectObject(hdc, old_font);
        let _ = DeleteObject(font);
    }
}

fn do_poll(send_hwnd: SendHwnd) {
    let hwnd = send_hwnd.to_hwnd();
    let provider = {
        let state = lock_state();
        state
            .as_ref()
            .map(|s| s.active_provider)
            .unwrap_or(ProviderKind::OpenCode)
    };
    let handle = std::thread::spawn(move || provider_adapter::poll_provider(provider));

    let mut any_ok = false;
    let mut needs_fast_reset_poll = false;

    {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if let Ok(result) = handle.join() {
                provider_state_mut(s, result.provider).activity = result.activity.clone();
                match result.data {
                    Ok(data) => {
                        if result.provider != ProviderKind::OpenCode {
                            needs_fast_reset_poll |= poller::is_past_reset(&data);
                        }
                        set_provider_data(s, result.provider, data, result.activity);
                        any_ok = true;
                    }
                    Err(provider_adapter::ProviderPollError::Unavailable) => {
                        set_provider_unavailable(s, result.provider);
                    }
                    Err(provider_adapter::ProviderPollError::Loading) => {
                        set_provider_loading(s, result.provider);
                    }
                }
            }

            refresh_usage_texts(s);
            if any_ok {
                if s.retry_count > 0 {
                    s.retry_count = 0;
                    unsafe {
                        SetTimer(hwnd, TIMER_POLL, s.poll_interval_ms, None);
                    }
                }
            } else {
                s.retry_count = s.retry_count.saturating_add(1);
                let backoff = RETRY_BASE_MS
                    .saturating_mul(1u32.checked_shl(s.retry_count - 1).unwrap_or(u32::MAX));
                let retry_ms = backoff.min(s.poll_interval_ms);

                unsafe {
                    SetTimer(hwnd, TIMER_POLL, retry_ms, None);
                }
            }
        }
    }

    unsafe {
        // Show refresh indicator — retry will recover silently
        if needs_fast_reset_poll {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        } else {
            let _ = KillTimer(hwnd, TIMER_RESET_POLL);
        }

        let _ = PostMessageW(hwnd, WM_APP_USAGE_UPDATED, WPARAM(0), LPARAM(0));
    }
}

fn schedule_countdown_timer() {
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    if !active_provider_state(s).last_poll_ok {
        return;
    }

    let data = match &active_provider_state(s).data {
        Some(d) => d,
        None => return,
    };

    let hwnd = s.hwnd.to_hwnd();

    // If a reset time has passed, poll every 5s to pick up fresh data
    if poller::is_past_reset(data) {
        unsafe {
            SetTimer(hwnd, TIMER_RESET_POLL, 5_000, None);
        }
    }

    let session_delay = poller::time_until_display_change(data.session.resets_at);
    let weekly_delay = poller::time_until_display_change(data.weekly.resets_at);

    let min_delay = match (session_delay, weekly_delay) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    let ms = min_delay
        .unwrap_or(Duration::from_secs(60))
        .as_millis()
        .max(1000) as u32;

    unsafe {
        SetTimer(hwnd, TIMER_COUNTDOWN, ms, None);
    }
}

fn check_theme_change() {
    let new_dark = theme::is_dark_mode();
    let changed = {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            if s.is_dark != new_dark {
                s.is_dark = new_dark;
                true
            } else {
                false
            }
        } else {
            false
        }
    };
    if changed {
        render_layered();
    }
}

fn check_language_change() {
    if update_language_change() {
        render_layered();
    }
}

fn update_display() {
    let mut state = lock_state();
    let s = match state.as_mut() {
        Some(s) => s,
        None => return,
    };

    // Don't overwrite error text with stale cached data
    if !active_provider_state(s).last_poll_ok {
        return;
    }

    refresh_usage_texts(s);
}

fn refresh_tray_icon() {
    let (hwnd, added, tooltip) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else { return };
        (s.hwnd.to_hwnd(), s.tray_icon_added, tray_tooltip_text(s))
    };

    let data = build_notify_icon_data(hwnd, &tooltip);
    let result = unsafe {
        if added {
            Shell_NotifyIconW(NIM_MODIFY, &data)
        } else {
            Shell_NotifyIconW(NIM_ADD, &data)
        }
    };

    if result.as_bool() && !added {
        let mut state = lock_state();
        if let Some(s) = state.as_mut() {
            s.tray_icon_added = true;
        }
    }
}

fn remove_tray_icon() {
    let (hwnd, added) = {
        let state = lock_state();
        let Some(s) = state.as_ref() else { return };
        (s.hwnd.to_hwnd(), s.tray_icon_added)
    };
    if !added {
        return;
    }

    let data = build_notify_icon_data(hwnd, "");
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
}

fn build_notify_icon_data(hwnd: HWND, tooltip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY_ICON,
        ..Default::default()
    };

    data.hIcon = unsafe { LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default() };
    let tip = native_interop::wide_str(&trim_for_menu(tooltip, data.szTip.len().saturating_sub(1)));
    for (index, value) in tip.into_iter().take(data.szTip.len()).enumerate() {
        data.szTip[index] = value;
    }
    data
}

fn tray_tooltip_text(state: &AppState) -> String {
    let provider = state.active_provider;
    let provider_state = active_provider_state(state);
    let mut lines = vec![format!("{} Usage", provider_name(provider))];
    lines.push(provider_summary_line(provider, provider_state));
    if provider == ProviderKind::OpenCode {
        if let Some(data) = provider_state.data.as_ref() {
            lines.extend(opencode_detail_lines(data));
        }
    } else if let Some(data) = provider_state.data.as_ref() {
        lines.push(provider_raw_usage_line(provider, data));
    }
    lines.join("\n")
}

fn position_at_taskbar() {
    refresh_dpi();
    let state = lock_state();
    let s = match state.as_ref() {
        Some(s) => s,
        None => return,
    };

    // Don't fight the user's drag
    if s.dragging {
        return;
    }

    let hwnd = s.hwnd.to_hwnd();
    let embedded = s.embedded;
    let tray_offset = s.tray_offset;

    let taskbar_hwnd = match s.taskbar_hwnd {
        Some(h) => h,
        None => {
            diagnose::log("position_at_taskbar skipped: no taskbar handle");
            return;
        }
    };

    let taskbar_rect = match native_interop::get_taskbar_rect(taskbar_hwnd) {
        Some(r) => r,
        None => {
            diagnose::log("position_at_taskbar skipped: unable to query taskbar rect");
            return;
        }
    };

    let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
    let mut tray_left = taskbar_rect.right;

    if let Some(tray_hwnd) = native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd") {
        if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd) {
            tray_left = tray_rect.left;
        }
    }

    let widget_width = total_widget_width();

    let widget_height = sc(WIDGET_HEIGHT);
    if embedded {
        // Child window: coordinates relative to parent (taskbar)
        let x = tray_left - taskbar_rect.left - widget_width - tray_offset;
        let y = (taskbar_height - widget_height) / 2;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned embedded widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    } else {
        // Topmost popup: screen coordinates
        let x = tray_left - widget_width - tray_offset;
        let y = taskbar_rect.top + (taskbar_height - widget_height) / 2;
        native_interop::move_window(hwnd, x, y, widget_width, widget_height);
        diagnose::log(format!(
            "positioned fallback widget at x={x} y={y} w={widget_width} h={widget_height}"
        ));
    }
}

/// WinEvent callback for tray icon location changes
unsafe extern "system" fn on_tray_location_changed(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    static LAST_REPOSITION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

    let is_tray = {
        let state = lock_state();
        state
            .as_ref()
            .and_then(|s| s.tray_notify_hwnd)
            .map(|h| h == hwnd)
            .unwrap_or(false)
    };

    if is_tray {
        let should_reposition = {
            let mut last = LAST_REPOSITION.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            if last
                .map(|t| now.duration_since(t).as_millis() > 500)
                .unwrap_or(true)
            {
                *last = Some(now);
                true
            } else {
                false
            }
        };
        if should_reposition {
            position_at_taskbar();
            render_layered();
        }
    }
}

/// Main window procedure
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // For non-embedded fallback, paint normally
            let embedded = {
                let state = lock_state();
                state.as_ref().map(|s| s.embedded).unwrap_or(false)
            };
            if embedded {
                // Layered windows don't use WM_PAINT; just validate the region
                let mut ps = PAINTSTRUCT::default();
                let _ = BeginPaint(hwnd, &mut ps);
                let _ = EndPaint(hwnd, &ps);
            } else {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                paint(hdc, hwnd);
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DISPLAYCHANGE | WM_DPICHANGED_MSG | WM_SETTINGCHANGE => {
            if msg == WM_DPICHANGED_MSG {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                CURRENT_DPI.store(new_dpi, Ordering::Relaxed);
            }
            if msg == WM_SETTINGCHANGE {
                check_theme_change();
                check_language_change();
            }
            refresh_dpi();
            position_at_taskbar();
            render_layered();
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wparam.0;
            match timer_id {
                TIMER_POLL => {
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                TIMER_COUNTDOWN => {
                    update_display();
                    render_layered();
                    schedule_countdown_timer();
                }
                TIMER_RESET_POLL => {
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_APP_USAGE_UPDATED => {
            check_theme_change();
            check_language_change();
            render_layered();
            refresh_tray_icon();
            schedule_countdown_timer();
            LRESULT(0)
        }
        WM_APP_TRAY_ICON => {
            let mouse_message = lparam.0 as u32;
            if mouse_message == WM_RBUTTONUP {
                show_context_menu(hwnd);
            } else if mouse_message == WM_LBUTTONUP {
                let switched = {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        cycle_active_provider(s)
                    } else {
                        false
                    }
                };
                if switched {
                    save_state_settings();
                    render_layered();
                    refresh_tray_icon();
                    schedule_countdown_timer();
                }
            }
            LRESULT(0)
        }
        WM_APP_UPDATE_CHECK_COMPLETE => LRESULT(0),
        WM_SETCURSOR => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            // Always show resize cursor while dragging or when hovering divider zone
            let hit_test = (lparam.0 & 0xFFFF) as u16;
            if is_dragging {
                let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                SetCursor(cursor);
                return LRESULT(1);
            }
            if hit_test == 1 {
                // HTCLIENT
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = ScreenToClient(hwnd, &mut pt);
                if pt.x < sc(DIVIDER_HIT_ZONE) {
                    let cursor = LoadCursorW(HINSTANCE::default(), IDC_SIZEWE).unwrap_or_default();
                    SetCursor(cursor);
                    return LRESULT(1);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let client_x = (lparam.0 & 0xFFFF) as i16 as i32;
            if client_x < sc(DIVIDER_HIT_ZONE) {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    s.dragging = true;
                    s.drag_start_mouse_x = pt.x;
                    s.drag_start_offset = s.tray_offset;
                }
                SetCapture(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let is_dragging = {
                let state = lock_state();
                state.as_ref().map(|s| s.dragging).unwrap_or(false)
            };
            if is_dragging {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);

                let mut state = lock_state();
                let s = match state.as_mut() {
                    Some(s) => s,
                    None => return LRESULT(0),
                };

                // Moving mouse left = positive delta = larger offset (further left)
                let delta = s.drag_start_mouse_x - pt.x;
                let mut new_offset = s.drag_start_offset + delta;

                // Clamp: offset >= 0 (can't go right of default)
                if new_offset < 0 {
                    new_offset = 0;
                }

                // Clamp: don't go past left edge of taskbar
                if let Some(taskbar_hwnd) = s.taskbar_hwnd {
                    if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                        let mut tray_left = taskbar_rect.right;
                        if let Some(tray_hwnd) =
                            native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                        {
                            if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd)
                            {
                                tray_left = tray_rect.left;
                            }
                        }
                        let widget_width = total_widget_width();
                        let max_offset = tray_left - taskbar_rect.left - widget_width;
                        if new_offset > max_offset {
                            new_offset = max_offset;
                        }
                    }
                }

                s.tray_offset = new_offset;

                // Move window directly
                let hwnd_val = s.hwnd.to_hwnd();
                if let Some(taskbar_hwnd) = s.taskbar_hwnd {
                    if let Some(taskbar_rect) = native_interop::get_taskbar_rect(taskbar_hwnd) {
                        let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
                        let mut tray_left = taskbar_rect.right;
                        if let Some(tray_hwnd) =
                            native_interop::find_child_window(taskbar_hwnd, "TrayNotifyWnd")
                        {
                            if let Some(tray_rect) = native_interop::get_window_rect_safe(tray_hwnd)
                            {
                                tray_left = tray_rect.left;
                            }
                        }
                        let widget_width = total_widget_width();
                        let widget_height = sc(WIDGET_HEIGHT);
                        if s.embedded {
                            let x = tray_left - taskbar_rect.left - widget_width - new_offset;
                            let y = (taskbar_height - widget_height) / 2;
                            native_interop::move_window(
                                hwnd_val,
                                x,
                                y,
                                widget_width,
                                widget_height,
                            );
                        } else {
                            let x = tray_left - widget_width - new_offset;
                            let y = taskbar_rect.top + (taskbar_height - widget_height) / 2;
                            native_interop::move_window(
                                hwnd_val,
                                x,
                                y,
                                widget_width,
                                widget_height,
                            );
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let was_dragging = {
                let mut state = lock_state();
                if let Some(s) = state.as_mut() {
                    if s.dragging {
                        s.dragging = false;
                        let offset = s.tray_offset;
                        Some(offset)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if was_dragging.is_some() {
                let _ = ReleaseCapture();
                save_state_settings();
            } else {
                let switched = {
                    let mut state = lock_state();
                    if let Some(s) = state.as_mut() {
                        cycle_active_provider(s)
                    } else {
                        false
                    }
                };

                if switched {
                    save_state_settings();
                    render_layered();
                    schedule_countdown_timer();
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 as u16;
            match id {
                1 => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            active_provider_state_mut(s).session_text = "...".to_string();
                            active_provider_state_mut(s).weekly_text = "...".to_string();
                        }
                    }
                    render_layered();
                    let sh = SendHwnd::from_hwnd(hwnd);
                    std::thread::spawn(move || {
                        do_poll(sh);
                    });
                }
                IDM_VERSION_ACTION => {
                    let (install_channel, release) = {
                        let state = lock_state();
                        match state.as_ref() {
                            Some(s) => (
                                s.install_channel,
                                match &s.update_status {
                                    UpdateStatus::Available(release) => Some(release.clone()),
                                    _ => None,
                                },
                            ),
                            None => (InstallChannel::Portable, None),
                        }
                    };

                    match install_channel {
                        InstallChannel::Winget => begin_winget_update(hwnd),
                        InstallChannel::Portable => {
                            if let Some(release) = release {
                                begin_update_apply(hwnd, release);
                            } else {
                                begin_update_check(hwnd);
                            }
                        }
                    }
                }
                2 => {
                    remove_tray_icon();
                    let hook = {
                        let state = lock_state();
                        state.as_ref().and_then(|s| s.win_event_hook)
                    };
                    if let Some(h) = hook {
                        native_interop::unhook_win_event(h);
                    }
                    PostQuitMessage(0);
                }
                IDM_RESET_POSITION => {
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.tray_offset = 0;
                        }
                    }
                    save_state_settings();
                    position_at_taskbar();
                }
                IDM_PROVIDER_CLAUDE | IDM_PROVIDER_CODEX | IDM_PROVIDER_OPENCODE => {
                    let new_provider = match id {
                        IDM_PROVIDER_CODEX => ProviderKind::Codex,
                        IDM_PROVIDER_OPENCODE => ProviderKind::OpenCode,
                        _ => ProviderKind::Claude,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.active_provider = new_provider;
                        }
                    }
                    save_state_settings();
                    render_layered();
                    schedule_countdown_timer();
                }
                IDM_START_WITH_WINDOWS => {
                    set_startup_enabled(!is_startup_enabled());
                }
                IDM_EXPORT_HISTORY_REPORT => {
                    let history_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    let token = if history_sync.prefer_remote_reports {
                        archive_token_from_sources().or_else(|| {
                            prompt_for_text(
                                "GitHub Token",
                                "Personal access token for GitHub report download. It is used for this export only and is not stored.",
                                "",
                            )
                        })
                    } else {
                        archive_token_from_sources()
                    };

                    match history::write_report(&history_sync, token.as_deref()) {
                        Ok(path) => show_info_message(
                            hwnd,
                            "History",
                            &format!("HTML usage report written to {}", path.display()),
                        ),
                        Err(error) => show_error_message(hwnd, "History", &error),
                    }
                }
                IDM_OPEN_APP_DATA_FOLDER => {
                    if let Err(error) = open_or_create_folder(&app_data_folder_path()) {
                        show_error_message(hwnd, "History", &error);
                    }
                }
                IDM_OPEN_LOCAL_HISTORY_FOLDER => {
                    if let Err(error) = open_or_create_folder(&local_history_folder_path()) {
                        show_error_message(hwnd, "History", &error);
                    }
                }
                IDM_OPEN_REPORTS_FOLDER => {
                    if let Err(error) = open_or_create_folder(&reports_folder_path()) {
                        show_error_message(hwnd, "History", &error);
                    }
                }
                IDM_OPEN_CLAUDE_SOURCE_FOLDER => {
                    match claude_source_folder_path() {
                        Some(path) => {
                            if let Err(error) = open_existing_folder(&path) {
                                show_error_message(hwnd, "History", &error);
                            }
                        }
                        None => show_error_message(
                            hwnd,
                            "History",
                            "Unable to determine the Claude source folder.",
                        ),
                    }
                }
                IDM_OPEN_CODEX_SOURCE_FOLDER => {
                    match codex_source_folder_path() {
                        Some(path) => {
                            if let Err(error) = open_existing_folder(&path) {
                                show_error_message(hwnd, "History", &error);
                            }
                        }
                        None => show_error_message(
                            hwnd,
                            "History",
                            "Unable to determine the Codex source folder.",
                        ),
                    }
                }
                IDM_ARCHIVE_HISTORY_GITHUB => {
                    let history_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    };
                    let token = archive_token_from_sources().or_else(|| {
                        prompt_for_text(
                            "GitHub Token",
                            "Personal access token for GitHub upload. It is used for this archive operation only and is not stored.",
                            "",
                        )
                    });

                    if let Some(token) = token {
                        let send_hwnd = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            let history_sync = history_sync.unwrap_or_default();
                            match history::archive_history(&history_sync, &token) {
                                Ok(result) => {
                                    let mut message = format!(
                                        "Archived history to {}. Cleaned {} local report(s).",
                                        result.repo_url, result.cleaned_reports
                                    );
                                    if result.uploaded_conversation_files > 0 {
                                        message.push_str(&format!(
                                            "\n\nUploaded {} raw conversation log file(s).",
                                            result.uploaded_conversation_files
                                        ));
                                    }
                                    if let Some(warning) = result.workflow_warning {
                                        message.push_str(
                                            "\n\nHistory files were uploaded, but the optional workflow dispatch failed:\n",
                                        );
                                        message.push_str(&warning);
                                    }
                                    show_info_message(send_hwnd.to_hwnd(), "History", &message)
                                }
                                Err(error) => {
                                    show_error_message(send_hwnd.to_hwnd(), "History", &error)
                                }
                            }
                        });
                    }
                }
                IDM_FETCH_HISTORY_GITHUB => {
                    let history_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    let token = archive_token_from_sources().or_else(|| {
                        prompt_for_text(
                            "GitHub Token",
                            "Personal access token for GitHub download. It is used for this fetch operation only and is not stored.",
                            "",
                        )
                    });

                    if let Some(token) = token {
                        let send_hwnd = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            match history::fetch_history_from_github(&history_sync, &token) {
                                Ok(result) => show_info_message(
                                    send_hwnd.to_hwnd(),
                                    "History",
                                    &format!(
                                        "Fetched GitHub history from {}. {}",
                                        result.repo_url, result.message
                                    ),
                                ),
                                Err(error) => {
                                    show_error_message(send_hwnd.to_hwnd(), "History", &error)
                                }
                            }
                        });
                    }
                }
                IDM_SET_ARCHIVE_REPO => {
                    let current_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    let default_value = effective_archive_repo_url(current_sync.repo_url.as_deref());
                    if let Some(value) = prompt_for_text(
                        "Archive Repo",
                        "GitHub repo in owner/repo, https://github.com/owner/repo.git, or git@github.com:owner/repo.git form. Leave blank to use the default archive repo.",
                        current_sync.repo_url.as_deref().unwrap_or(&default_value),
                    ) {
                        let trimmed = value.trim();
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.history_sync.repo_url = if trimmed.is_empty()
                                    || trimmed.eq_ignore_ascii_case(history::DEFAULT_ARCHIVE_REPO_URL)
                                {
                                    None
                                } else {
                                    Some(trimmed.to_string())
                                };
                            }
                        }
                        save_state_settings();
                    }
                }
                IDM_SET_GITHUB_TOKEN => {
                    if let Some(token) = prompt_for_text(
                        "GitHub Token",
                        "GitHub personal access token to save in Windows Credential Manager. It is stored encrypted for this Windows user and not written to settings.json.\n\nClassic tokens start with ghp_ and fine-grained tokens start with github_pat_.",
                        "",
                    ) {
                        if let Err(error) = history::validate_github_token_format(&token) {
                            show_error_message(hwnd, "GitHub Token", &error);
                        } else {
                            let send_hwnd = SendHwnd::from_hwnd(hwnd);
                            let token_clone = token.clone();
                            std::thread::spawn(move || {
                                match history::validate_github_token_live(&token_clone) {
                                    Ok(username) => {
                                        match secret_store::save_github_pat(&token_clone) {
                                            Ok(()) => show_info_message(
                                                send_hwnd.to_hwnd(),
                                                "GitHub Token",
                                                &format!("Token verified (user: {username}) and saved in Windows Credential Manager."),
                                            ),
                                            Err(error) => show_error_message(send_hwnd.to_hwnd(), "GitHub Token", &error),
                                        }
                                    }
                                    Err(error) => show_error_message(
                                        send_hwnd.to_hwnd(),
                                        "GitHub Token",
                                        &format!("Token validation failed: {error}"),
                                    ),
                                }
                            });
                        }
                    }
                }
                IDM_CLEAR_GITHUB_TOKEN => match secret_store::clear_github_pat() {
                    Ok(()) => show_info_message(
                        hwnd,
                        "GitHub Token",
                        "Removed saved GitHub token from Windows Credential Manager.",
                    ),
                    Err(error) => show_error_message(hwnd, "GitHub Token", &error),
                },
                IDM_INSTALL_WORKFLOW_GITHUB => {
                    let history_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    let token = archive_token_from_sources().or_else(|| {
                        prompt_for_text(
                            "GitHub Token",
                            "Personal access token for workflow installation. It is used only for this upload and is not stored. Modifying .github/workflows requires workflow-related GitHub token permissions.",
                            "",
                        )
                    });

                    if let Some(token) = token {
                        let send_hwnd = SendHwnd::from_hwnd(hwnd);
                        std::thread::spawn(move || {
                            match history::install_sample_workflow_to_github(&history_sync, &token) {
                                Ok(result) => show_info_message(
                                    send_hwnd.to_hwnd(),
                                    "History",
                                    &format!(
                                        "Installed sample workflow to {}",
                                        result.location
                                    ),
                                ),
                                Err(error) => {
                                    show_error_message(send_hwnd.to_hwnd(), "History", &error)
                                }
                            }
                        });
                    }
                }
                IDM_INSTALL_WORKFLOW_LOCAL => {
                    let current_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    if let Some(local_repo_path) = resolve_local_repo_path(&current_sync) {
                        match history::install_sample_workflow_to_local_repo(
                            std::path::Path::new(&local_repo_path),
                            current_sync.workflow_file.as_deref(),
                        ) {
                            Ok(workflow_path) => {
                                {
                                    let mut state = lock_state();
                                    if let Some(s) = state.as_mut() {
                                        s.history_sync.local_repo_path = Some(local_repo_path);
                                    }
                                }
                                save_state_settings();
                                show_info_message(
                                    hwnd,
                                    "History",
                                    &format!(
                                        "Installed sample workflow locally at {}",
                                        workflow_path.display()
                                    ),
                                );
                            }
                            Err(error) => show_error_message(hwnd, "History", &error),
                        }
                    }
                }
                IDM_OPEN_LOCAL_WORKFLOWS => {
                    let current_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    if let Some(local_repo_path) = resolve_local_repo_path(&current_sync) {
                        match history::local_workflows_dir(std::path::Path::new(&local_repo_path)) {
                            Ok(workflows_dir) => {
                                if let Err(error) = std::fs::create_dir_all(&workflows_dir) {
                                    show_error_message(
                                        hwnd,
                                        "History",
                                        &format!(
                                            "Unable to create local workflows directory {}: {error}",
                                            workflows_dir.display()
                                        ),
                                    );
                                } else if let Err(error) = open_in_explorer(&workflows_dir) {
                                    show_error_message(hwnd, "History", &error);
                                } else {
                                    {
                                        let mut state = lock_state();
                                        if let Some(s) = state.as_mut() {
                                            s.history_sync.local_repo_path = Some(local_repo_path);
                                        }
                                    }
                                    save_state_settings();
                                }
                            }
                            Err(error) => show_error_message(hwnd, "History", &error),
                        }
                    }
                }
                IDM_SETUP_HISTORY_SYNC => {
                    let current_sync = {
                        let state = lock_state();
                        state.as_ref().map(|s| s.history_sync.clone())
                    }
                    .unwrap_or_default();
                    if let Some(new_sync) = run_sync_setup(hwnd, &current_sync) {
                        {
                            let mut state = lock_state();
                            if let Some(s) = state.as_mut() {
                                s.history_sync = new_sync;
                            }
                        }
                        save_state_settings();
                    }
                }
                IDM_TOGGLE_CONVERSATION_SYNC => {
                    let enabled = {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.history_sync.upload_conversations =
                                !s.history_sync.upload_conversations;
                            s.history_sync.upload_conversations
                        } else {
                            false
                        }
                    };
                    save_state_settings();
                    if enabled {
                        show_info_message(
                            hwnd,
                            "GitHub Sync",
                            "Raw conversation syncing is enabled. Push To GitHub will upload .claude and .codex session logs to the configured archive repo.",
                        );
                    }
                }
                IDM_FREQ_1MIN | IDM_FREQ_5MIN | IDM_FREQ_15MIN | IDM_FREQ_1HOUR => {
                    let new_interval = match id {
                        IDM_FREQ_1MIN => POLL_1_MIN,
                        IDM_FREQ_5MIN => POLL_5_MIN,
                        IDM_FREQ_15MIN => POLL_15_MIN,
                        IDM_FREQ_1HOUR => POLL_1_HOUR,
                        _ => POLL_15_MIN,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            s.poll_interval_ms = new_interval;
                        }
                    }
                    save_state_settings();
                    // Reset the poll timer with the new interval
                    SetTimer(hwnd, TIMER_POLL, new_interval, None);
                }
                IDM_LANG_SYSTEM | IDM_LANG_ENGLISH | IDM_LANG_SPANISH | IDM_LANG_FRENCH
                | IDM_LANG_GERMAN | IDM_LANG_JAPANESE => {
                    let language_override = match id {
                        IDM_LANG_SYSTEM => None,
                        IDM_LANG_ENGLISH => Some(LanguageId::English),
                        IDM_LANG_SPANISH => Some(LanguageId::Spanish),
                        IDM_LANG_FRENCH => Some(LanguageId::French),
                        IDM_LANG_GERMAN => Some(LanguageId::German),
                        IDM_LANG_JAPANESE => Some(LanguageId::Japanese),
                        _ => None,
                    };
                    {
                        let mut state = lock_state();
                        if let Some(s) = state.as_mut() {
                            apply_language_to_state(s, language_override);
                        }
                    }
                    save_state_settings();
                    render_layered();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon();
            let hook = {
                let state = lock_state();
                state.as_ref().and_then(|s| s.win_event_hook)
            };
            if let Some(h) = hook {
                native_interop::unhook_win_event(h);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let (
            current_interval,
            strings,
            language,
            language_override,
            install_channel,
            update_status,
            active_provider,
            history_sync,
            claude_state,
            codex_state,
            opencode_state,
        ) = {
            let state = lock_state();
            match state.as_ref() {
                Some(s) => (
                    s.poll_interval_ms,
                    s.language.strings(),
                    s.language,
                    s.language_override,
                    s.install_channel,
                    s.update_status.clone(),
                    s.active_provider,
                    s.history_sync.clone(),
                    s.claude.clone(),
                    s.codex.clone(),
                    s.opencode.clone(),
                ),
                None => (
                    POLL_15_MIN,
                    LanguageId::English.strings(),
                    LanguageId::English,
                    None,
                    InstallChannel::Portable,
                    UpdateStatus::Idle,
                    ProviderKind::Claude,
                    HistorySyncSettings::default(),
                    ProviderState::default(),
                    ProviderState::default(),
                    ProviderState::default(),
                ),
            }
        };

        let Ok(menu) = CreatePopupMenu() else {
            diagnose::log("unable to create context menu");
            return;
        };

        let refresh_str = native_interop::wide_str(strings.refresh);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            1,
            PCWSTR::from_raw(refresh_str.as_ptr()),
        );

        let Ok(providers_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create providers context menu");
            return;
        };
        for (provider, id) in [
            (ProviderKind::Claude, IDM_PROVIDER_CLAUDE),
            (ProviderKind::Codex, IDM_PROVIDER_CODEX),
            (ProviderKind::OpenCode, IDM_PROVIDER_OPENCODE),
        ] {
            let label = native_interop::wide_str(provider_name(provider));
            let flags = if active_provider == provider {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                providers_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label.as_ptr()),
            );
        }

        let providers_label = native_interop::wide_str("Providers");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            providers_menu.0 as usize,
            PCWSTR::from_raw(providers_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        append_disabled_menu_line(
            menu,
            &provider_summary_line(ProviderKind::Claude, &claude_state),
        );
        append_disabled_menu_line(
            menu,
            &provider_summary_line(ProviderKind::Codex, &codex_state),
        );
        append_disabled_menu_line(
            menu,
            &provider_summary_line(ProviderKind::OpenCode, &opencode_state),
        );
        if let Some(data) = claude_state.data.as_ref() {
            append_disabled_menu_line(menu, &provider_raw_usage_line(ProviderKind::Claude, data));
        }
        if let Some(data) = codex_state.data.as_ref() {
            append_disabled_menu_line(menu, &provider_raw_usage_line(ProviderKind::Codex, data));
        }
        if let Some(data) = opencode_state.data.as_ref() {
            append_disabled_menu_line(menu, &provider_raw_usage_line(ProviderKind::OpenCode, data));
            for line in opencode_detail_lines(data) {
                append_disabled_menu_line(menu, &line);
            }
            if let Some(source) = data
                .source_label
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                append_disabled_menu_line(menu, &format!("OpenCode source: {source}"));
            }
        }

        if let Some(data) = codex_state.data.as_ref() {
            if let Some(plan) = data.plan_name.as_deref().filter(|value| !value.is_empty()) {
                append_disabled_menu_line(menu, &format!("Codex plan: {plan}"));
            }

            if let Some(source) = data
                .source_label
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                append_disabled_menu_line(menu, &format!("Codex source: {source}"));
            }

            if let Some(updated_at) = data.updated_at {
                append_disabled_menu_line(
                    menu,
                    &format!("Codex updated: {}", relative_time_text(updated_at)),
                );
            }

            if let Some(review) = data.review.as_ref() {
                append_disabled_menu_line(
                    menu,
                    &format!("Code review: {}", poller::format_line(review, strings)),
                );
            }

            if let Some(credits) = credits_text(data) {
                append_disabled_menu_line(menu, &credits);
            }
        }

        if let Some(activity) = codex_state.activity.as_ref() {
            for line in activity_lines(activity) {
                append_disabled_menu_line(menu, &line);
            }
        }

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let Ok(history_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create history context menu");
            return;
        };
        let open_app_data_str = native_interop::wide_str("Open App Data Folder");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_APP_DATA_FOLDER as usize,
            PCWSTR::from_raw(open_app_data_str.as_ptr()),
        );
        let open_local_history_str = native_interop::wide_str("Open Local History Folder");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_LOCAL_HISTORY_FOLDER as usize,
            PCWSTR::from_raw(open_local_history_str.as_ptr()),
        );
        let open_reports_str = native_interop::wide_str("Open Reports Folder");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_REPORTS_FOLDER as usize,
            PCWSTR::from_raw(open_reports_str.as_ptr()),
        );
        let open_claude_source_str = native_interop::wide_str("Open Claude Source Folder");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_CLAUDE_SOURCE_FOLDER as usize,
            PCWSTR::from_raw(open_claude_source_str.as_ptr()),
        );
        let open_codex_source_str = native_interop::wide_str("Open Codex Source Folder");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_CODEX_SOURCE_FOLDER as usize,
            PCWSTR::from_raw(open_codex_source_str.as_ptr()),
        );
        let _ = AppendMenuW(history_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let export_history_str = native_interop::wide_str("Export HTML Report");
        let _ = AppendMenuW(
            history_menu,
            MENU_ITEM_FLAGS(0),
            IDM_EXPORT_HISTORY_REPORT as usize,
            PCWSTR::from_raw(export_history_str.as_ptr()),
        );
        let _ = AppendMenuW(history_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let Ok(sync_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create sync context menu");
            return;
        };
        append_disabled_menu_line(sync_menu, &archive_repo_line(&history_sync));
        append_disabled_menu_line(sync_menu, &archive_branch_line(&history_sync));
        append_disabled_menu_line(sync_menu, &local_repo_line(&history_sync));
        append_disabled_menu_line(sync_menu, &github_token_line());
        append_disabled_menu_line(sync_menu, &sync_mode_line(&history_sync));
        append_disabled_menu_line(sync_menu, &sync_scopes_line(&history_sync));
        append_disabled_menu_line(sync_menu, &sync_report_source_line(&history_sync));
        let _ = AppendMenuW(sync_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let setup_sync_str = native_interop::wide_str("Setup...");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_SETUP_HISTORY_SYNC as usize,
            PCWSTR::from_raw(setup_sync_str.as_ptr()),
        );
        let sync_conversations_str = native_interop::wide_str("Sync Raw Conversations");
        let _ = AppendMenuW(
            sync_menu,
            if history_sync.upload_conversations {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            },
            IDM_TOGGLE_CONVERSATION_SYNC as usize,
            PCWSTR::from_raw(sync_conversations_str.as_ptr()),
        );
        let archive_repo_str = native_interop::wide_str("Set Repo...");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_SET_ARCHIVE_REPO as usize,
            PCWSTR::from_raw(archive_repo_str.as_ptr()),
        );
        let set_github_token_str = native_interop::wide_str("Set Saved Token...");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_SET_GITHUB_TOKEN as usize,
            PCWSTR::from_raw(set_github_token_str.as_ptr()),
        );
        let clear_github_token_str = native_interop::wide_str("Clear Saved Token");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_CLEAR_GITHUB_TOKEN as usize,
            PCWSTR::from_raw(clear_github_token_str.as_ptr()),
        );
        let _ = AppendMenuW(sync_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let archive_history_str = native_interop::wide_str("Push To GitHub");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_ARCHIVE_HISTORY_GITHUB as usize,
            PCWSTR::from_raw(archive_history_str.as_ptr()),
        );
        let fetch_history_str = native_interop::wide_str("Pull From GitHub");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_FETCH_HISTORY_GITHUB as usize,
            PCWSTR::from_raw(fetch_history_str.as_ptr()),
        );
        let _ = AppendMenuW(sync_menu, MF_SEPARATOR, 0, PCWSTR::null());
        let install_workflow_github_str = native_interop::wide_str("Install Workflow To GitHub");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_INSTALL_WORKFLOW_GITHUB as usize,
            PCWSTR::from_raw(install_workflow_github_str.as_ptr()),
        );
        let install_workflow_local_str = native_interop::wide_str("Install Workflow Locally");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_INSTALL_WORKFLOW_LOCAL as usize,
            PCWSTR::from_raw(install_workflow_local_str.as_ptr()),
        );
        let open_local_workflows_str = native_interop::wide_str("Open Local Workflows Folder");
        let _ = AppendMenuW(
            sync_menu,
            MENU_ITEM_FLAGS(0),
            IDM_OPEN_LOCAL_WORKFLOWS as usize,
            PCWSTR::from_raw(open_local_workflows_str.as_ptr()),
        );

        let sync_label = native_interop::wide_str("GitHub Sync");
        let _ = AppendMenuW(
            history_menu,
            MF_POPUP,
            sync_menu.0 as usize,
            PCWSTR::from_raw(sync_label.as_ptr()),
        );

        let history_label = native_interop::wide_str("History");
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            history_menu.0 as usize,
            PCWSTR::from_raw(history_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        // Update Frequency submenu
        let Ok(freq_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create frequency context menu");
            return;
        };
        let freq_items: [(u16, u32, &str); 4] = [
            (IDM_FREQ_1MIN, POLL_1_MIN, strings.one_minute),
            (IDM_FREQ_5MIN, POLL_5_MIN, strings.five_minutes),
            (IDM_FREQ_15MIN, POLL_15_MIN, strings.fifteen_minutes),
            (IDM_FREQ_1HOUR, POLL_1_HOUR, strings.one_hour),
        ];
        for (id, interval, label) in freq_items {
            let label_str = native_interop::wide_str(label);
            let flags = if interval == current_interval {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                freq_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let freq_label = native_interop::wide_str(strings.update_frequency);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            freq_menu.0 as usize,
            PCWSTR::from_raw(freq_label.as_ptr()),
        );

        // Settings submenu
        let Ok(settings_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create settings context menu");
            return;
        };

        let startup_str = native_interop::wide_str(strings.start_with_windows);
        let startup_flags = if is_startup_enabled() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            startup_flags,
            IDM_START_WITH_WINDOWS as usize,
            PCWSTR::from_raw(startup_str.as_ptr()),
        );

        let reset_pos_str = native_interop::wide_str(strings.reset_position);
        let _ = AppendMenuW(
            settings_menu,
            MENU_ITEM_FLAGS(0),
            IDM_RESET_POSITION as usize,
            PCWSTR::from_raw(reset_pos_str.as_ptr()),
        );

        let Ok(language_menu) = CreatePopupMenu() else {
            diagnose::log("unable to create language context menu");
            return;
        };
        let system_label = native_interop::wide_str(strings.system_default);
        let system_flags = if language_override.is_none() {
            MF_CHECKED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            language_menu,
            system_flags,
            IDM_LANG_SYSTEM as usize,
            PCWSTR::from_raw(system_label.as_ptr()),
        );

        for language in LanguageId::ALL {
            let id = match language {
                LanguageId::English => IDM_LANG_ENGLISH,
                LanguageId::Spanish => IDM_LANG_SPANISH,
                LanguageId::French => IDM_LANG_FRENCH,
                LanguageId::German => IDM_LANG_GERMAN,
                LanguageId::Japanese => IDM_LANG_JAPANESE,
            };
            let label_str = native_interop::wide_str(language.native_name());
            let flags = if language_override == Some(language) {
                MF_CHECKED
            } else {
                MENU_ITEM_FLAGS(0)
            };
            let _ = AppendMenuW(
                language_menu,
                flags,
                id as usize,
                PCWSTR::from_raw(label_str.as_ptr()),
            );
        }

        let language_label = native_interop::wide_str(strings.language);
        let _ = AppendMenuW(
            settings_menu,
            MF_POPUP,
            language_menu.0 as usize,
            PCWSTR::from_raw(language_label.as_ptr()),
        );

        let _ = AppendMenuW(settings_menu, MF_SEPARATOR, 0, PCWSTR::null());

        let version_label =
            version_action_label(strings, language, install_channel, &update_status);
        let version_str = native_interop::wide_str(&version_label);
        let version_flags = if install_channel == InstallChannel::Portable
            && matches!(
                update_status,
                UpdateStatus::Checking | UpdateStatus::Applying
            ) {
            MF_GRAYED
        } else {
            MENU_ITEM_FLAGS(0)
        };
        let _ = AppendMenuW(
            settings_menu,
            version_flags,
            IDM_VERSION_ACTION as usize,
            PCWSTR::from_raw(version_str.as_ptr()),
        );

        let settings_label = native_interop::wide_str(strings.settings);
        let _ = AppendMenuW(
            menu,
            MF_POPUP,
            settings_menu.0 as usize,
            PCWSTR::from_raw(settings_label.as_ptr()),
        );

        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

        let exit_str = native_interop::wide_str(strings.exit);
        let _ = AppendMenuW(
            menu,
            MENU_ITEM_FLAGS(0),
            2,
            PCWSTR::from_raw(exit_str.as_ptr()),
        );

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

/// Paint for non-embedded fallback (normal WM_PAINT path)
fn paint(hdc: HDC, hwnd: HWND) {
    let (is_dark, strings, active_provider, session_pct, session_text, weekly_pct, weekly_text) = {
        let state = lock_state();
        match state.as_ref() {
            Some(s) => (
                s.is_dark,
                s.language.strings(),
                s.active_provider,
                active_provider_state(s).session_percent,
                active_provider_state(s).session_text.clone(),
                active_provider_state(s).weekly_percent,
                active_provider_state(s).weekly_text.clone(),
            ),
            None => return,
        }
    };

    let accent = provider_accent(active_provider);
    let track = if is_dark {
        Color::from_hex("#444444")
    } else {
        Color::from_hex("#AAAAAA")
    };
    let text_color = if is_dark {
        Color::from_hex("#888888")
    } else {
        Color::from_hex("#404040")
    };
    let bg_color = if is_dark {
        Color::from_hex("#1C1C1C")
    } else {
        Color::from_hex("#F3F3F3")
    };

    unsafe {
        let mut client_rect = RECT::default();
        let _ = GetClientRect(hwnd, &mut client_rect);
        let width = client_rect.right - client_rect.left;
        let height = client_rect.bottom - client_rect.top;

        if width <= 0 || height <= 0 {
            return;
        }

        let mem_dc = CreateCompatibleDC(hdc);
        let mem_bmp = CreateCompatibleBitmap(hdc, width, height);
        let old_bmp = SelectObject(mem_dc, mem_bmp);

        paint_content(
            mem_dc,
            PaintContentArgs {
                width,
                height,
                is_dark,
                bg: &bg_color,
                text_color: &text_color,
                accent: &accent,
                track: &track,
                strings,
                active_provider,
                session_pct,
                session_text: &session_text,
                weekly_pct,
                weekly_text: &weekly_text,
            },
        );

        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);

        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(mem_bmp);
        let _ = DeleteDC(mem_dc);
    }
}

struct RowRender<'a> {
    label: &'a str,
    percent: f64,
    text: &'a str,
}

struct RowColors<'a> {
    accent: &'a Color,
    track: &'a Color,
}

fn draw_row(hdc: HDC, x: i32, y: i32, row: RowRender<'_>, colors: RowColors<'_>) {
    let RowRender {
        label,
        percent,
        text,
    } = row;
    let RowColors { accent, track } = colors;
    let seg_w = sc(SEGMENT_W);
    let seg_h = sc(SEGMENT_H);
    let seg_gap = sc(SEGMENT_GAP);
    let corner_r = sc(CORNER_RADIUS);

    unsafe {
        let mut label_wide: Vec<u16> = label.encode_utf16().collect();
        let mut label_rect = RECT {
            left: x,
            top: y,
            right: x + sc(LABEL_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut label_wide,
            &mut label_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );

        let bar_x = x + sc(LABEL_WIDTH) + sc(LABEL_RIGHT_MARGIN);
        let percent_clamped = percent.clamp(0.0, 100.0);

        for i in 0..SEGMENT_COUNT {
            let seg_x = bar_x + i * (seg_w + seg_gap);
            let seg_start = (i as f64) * 10.0;
            let seg_end = seg_start + 10.0;

            let seg_rect = RECT {
                left: seg_x,
                top: y,
                right: seg_x + seg_w,
                bottom: y + seg_h,
            };

            if percent_clamped >= seg_end {
                draw_rounded_rect(hdc, &seg_rect, accent, corner_r);
            } else if percent_clamped <= seg_start {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
            } else {
                draw_rounded_rect(hdc, &seg_rect, track, corner_r);
                let fraction = (percent_clamped - seg_start) / 10.0;
                let fill_width = (seg_w as f64 * fraction) as i32;
                if fill_width > 0 {
                    let fill_rect = RECT {
                        left: seg_x,
                        top: y,
                        right: seg_x + fill_width,
                        bottom: y + seg_h,
                    };
                    let rgn = CreateRoundRectRgn(
                        seg_rect.left,
                        seg_rect.top,
                        seg_rect.right + 1,
                        seg_rect.bottom + 1,
                        corner_r * 2,
                        corner_r * 2,
                    );
                    let _ = SelectClipRgn(hdc, rgn);
                    let brush = CreateSolidBrush(COLORREF(accent.to_colorref()));
                    FillRect(hdc, &fill_rect, brush);
                    let _ = DeleteObject(brush);
                    let _ = SelectClipRgn(hdc, HRGN::default());
                    let _ = DeleteObject(rgn);
                }
            }
        }

        let text_x = bar_x + SEGMENT_COUNT * (seg_w + seg_gap) - seg_gap + sc(BAR_RIGHT_MARGIN);
        let mut text_wide: Vec<u16> = text.encode_utf16().collect();
        let mut text_rect = RECT {
            left: text_x,
            top: y,
            right: text_x + sc(TEXT_WIDTH),
            bottom: y + seg_h,
        };
        let _ = DrawTextW(
            hdc,
            &mut text_wide,
            &mut text_rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE,
        );
    }
}

fn draw_provider_badge(hdc: HDC, rect: &RECT, provider: ProviderKind, accent: &Color) {
    let _ = accent;
    let (fill, border, text) = provider_badge_palette(provider);
    draw_rounded_rect(hdc, rect, &border, sc(4));

    let inner = RECT {
        left: rect.left + sc(1),
        top: rect.top + sc(1),
        right: rect.right - sc(1),
        bottom: rect.bottom - sc(1),
    };
    draw_rounded_rect(hdc, &inner, &fill, sc(3));

    unsafe {
        let font_name = native_interop::wide_str("Segoe UI");
        let font = CreateFontW(
            sc(-12),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_TT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_name.as_ptr()),
        );
        let old_font = SelectObject(hdc, font);
        let old_bk_mode = SetBkMode(hdc, TRANSPARENT);
        let _ = SetTextColor(hdc, COLORREF(text.to_colorref()));

        let mut text: Vec<u16> = provider.short_label().encode_utf16().collect();
        let mut text_rect = inner;
        let _ = DrawTextW(
            hdc,
            &mut text,
            &mut text_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );

        SelectObject(hdc, old_font);
        let _ = SetBkMode(hdc, BACKGROUND_MODE(old_bk_mode as u32));
        let _ = DeleteObject(font);
    }
}

fn draw_rounded_rect(hdc: HDC, rect: &RECT, color: &Color, radius: i32) {
    unsafe {
        let brush = CreateSolidBrush(COLORREF(color.to_colorref()));
        let rgn = CreateRoundRectRgn(
            rect.left,
            rect.top,
            rect.right + 1,
            rect.bottom + 1,
            radius * 2,
            radius * 2,
        );
        let _ = FillRgn(hdc, rgn, brush);
        let _ = DeleteObject(rgn);
        let _ = DeleteObject(brush);
    }
}
