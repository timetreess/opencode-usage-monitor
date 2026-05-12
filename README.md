# OpenCode + Claude Code + Codex Usage Monitor

A lightweight Windows taskbar widget for people using OpenCode, Claude Code, OpenAI Codex, or any combination of them.

It sits in your taskbar and shows OpenCode token/cost totals from the local SQLite database, while preserving the existing Claude Code and Codex usage windows.

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

![Screenshot](.github/screenshot.png)

## What You Get

- OpenCode tracking with **5h**, **today**, and **month** input/output/reasoning/cache token totals and estimated cost
- Claude Code tracking with **5h** and **7d** usage bars
- Codex tracking with **5h** and **7d** usage bars
- A tray icon tooltip with active-provider usage details
- Left-click to switch between OpenCode, Claude, and Codex
- Right-click provider summaries including OpenCode monthly token totals, Codex plan, code review, recent prompt/session counts, and last prompt preview
- A small native widget that lives directly in the Windows taskbar
- Right-click options for refresh, update frequency, language, startup, and updates

## Who This Is For

This app is for Windows users who already have **OpenCode**, **Claude Code**, **Codex**, or any combination installed and signed in.

It works best if you want OpenCode local token/cost totals always visible, plus the original Claude/Codex limit display.

## Requirements

- Windows 10 or Windows 11
- OpenCode v1.2+ with `%LOCALAPPDATA%\opencode\opencode.db` for OpenCode tracking
- Claude Code (CLI or App) installed and authenticated for Claude tracking
- Codex CLI installed and authenticated for Codex tracking

If you use Claude Code through WSL, that is supported too. The monitor can read your Claude Code credentials from Windows or from your WSL environment.
Codex usage is read from your local `%USERPROFILE%\.codex` data on Windows.
OpenCode usage is read locally from `%LOCALAPPDATA%\opencode\opencode.db` using a read-only SQLite connection. If the schema cannot be recognized, run with `--diagnose` and inspect the diagnostic log for discovered tables/columns.

## Install

For now, download the latest `opencode-usage-monitor.exe` from the [Releases](../../releases) page and run it.

WinGet support is on the way and currently waiting on final approval. When that is live, installation will be a one-liner.

Planned command:

```powershell
winget install CodeZeno.OpencodeUsageMonitor
```

## Use

Run the app and it will appear in your taskbar.

- Drag the left divider to move it
- Left-click anywhere except the divider to switch the active provider
- Right-click for refresh, provider selection, update frequency, provider summaries, start with Windows, reset position, language, updates, and exit

Settings are saved to:

```text
%APPDATA%\OpencodeUsageMonitor\settings.json
```

OpenCode daily token snapshots are saved to:

```text
%APPDATA%\OpencodeUsageMonitor\log\YYYY-MM-DD.md
%APPDATA%\OpencodeUsageMonitor\log\YYYY-MM-DD.json
```

Use the right-click **Start with Windows** menu item to toggle startup. The app stores that preference under the current user's Windows Run registry key as `OpencodeUsageMonitor`.

For day-to-day development, prefer `cargo dev-check`. It validates the code without
linking a new Windows executable, which avoids unnecessary churn in `target\` and
keeps endpoint-security scans focused on the builds you actually run.

```powershell
cargo dev-check
```

If you specifically want release-profile validation without creating
`target\release\opencode-usage-monitor.exe`, use:

```powershell
cargo release-check
```

When you need to launch the app locally, use the debug build:

```powershell
cargo dev-run
```

To build the release executable:

```powershell
cargo release-build
```

If your organization uses endpoint-security tooling, use the normal IT-approved
process for source review, code signing, or allowlisting trusted release artifacts
instead of disabling or bypassing security controls.

## Account Support

This app works with the same account types that Claude Code itself supports.

As of **March 19, 2026**, Anthropic's Claude Code setup documentation says:

- **Supported:** Pro, Max, Teams, Enterprise, and Console accounts
- **Not supported:** the free Claude.ai plan

If Anthropic changes Claude Code availability in the future, this app should follow whatever Claude Code supports, as long as the usage data remains exposed through the same authenticated endpoints.

## Privacy And Security

This project is **open source**, so you can inspect exactly what it does.

What the app reads:

- Your local Claude Code OAuth credentials from `~/.claude/.credentials.json`
- If needed, the same credentials file inside an installed WSL distro
- Your local Codex auth file from `~/.codex/auth.json`
- Your local Codex prompt history from `~/.codex/history.jsonl` for recent-activity summaries
- Your local OpenCode SQLite database from `%LOCALAPPDATA%\opencode\opencode.db` or `opencode-*.db`

What the app sends over the network:

- Requests to Anthropic's Claude endpoints to read your usage and rate-limit information
- Requests to OpenAI's Codex usage endpoint to read your Codex limits
- If needed, a local `codex app-server` probe to read Codex rate limits from the installed CLI
- Requests to GitHub only if you use the app's update check / self-update feature
- No network requests are made for OpenCode tracking

What the app stores locally:

- Widget position
- Polling frequency
- Language preference

What it does **not** do:

- It does not send your credentials to any other server
- It does not use a separate backend service
- It does not collect analytics or telemetry
- It does not upload your project files

Notes:

- If your Claude Code token is expired, the app may ask the local Claude CLI to refresh it in the background
- Portable installs can update themselves by downloading the latest release from this repository

## How It Works

The monitor:

1. Opens the OpenCode SQLite database read-only and autodiscovers token/timestamp data in message tables
2. Finds your Claude Code login credentials
3. Finds your Codex login state and recent local history
4. Reads current Claude/Codex usage from Anthropic and OpenAI
5. Falls back to the local Codex CLI rate-limit RPC if the Codex web usage endpoint is unavailable
6. Shows the active provider directly in the Windows taskbar and tray tooltip
7. Refreshes periodically in the background

If the newer usage endpoint is unavailable, it can fall back to reading the rate-limit headers returned by Claude's Messages API.

## Open Source

This project is licensed under MIT.

If you want to inspect the behavior or audit the code, everything is in this repository.
