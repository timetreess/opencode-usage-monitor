# OpenCode Usage Monitor

A lightweight Windows taskbar widget for tracking **OpenCode token usage** alongside Claude Code and OpenAI Codex limits.

This project is a fork of [`CodeZeno/Claude-Code-Usage-Monitor`](https://github.com/CodeZeno/Claude-Code-Usage-Monitor). The fork keeps the original taskbar-widget idea and Claude/Codex support, then extends it for OpenCode users with local SQLite usage tracking, OpenCode-aware provider switching, tray summaries, and daily token snapshots.

![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

![Screenshot](.github/screenshot.png)

## What This Fork Adds

- **OpenCode usage windows**: 5h, today, and month totals from the local OpenCode database
- **Token breakdowns**: input, output, reasoning output, cached input, total tokens, and estimated cost
- **OpenCode daily snapshots** written as Markdown and JSON for later review
- **Three-provider switching** between OpenCode, Claude Code, and Codex
- **Tray tooltip summaries** for the active provider
- **Renamed release artifacts** and metadata for `opencode-usage-monitor.exe`

The original Claude Code and Codex usage views are still available. OpenCode is the default focus of this fork.

## Requirements

- Windows 10 or Windows 11
- OpenCode v1.2+ with a local database under `%LOCALAPPDATA%\opencode\`
- Claude Code installed and authenticated if you want Claude usage windows
- Codex CLI installed and authenticated if you want Codex usage windows

OpenCode tracking is local-only: the app opens `%LOCALAPPDATA%\opencode\opencode.db` or another `opencode-*.db` file read-only and autodiscovers supported token tables/columns.

## Install

Download the latest `opencode-usage-monitor.exe` from the [Releases](../../releases) page and run it.

WinGet support is planned under:

```powershell
winget install CodeZeno.OpencodeUsageMonitor
```

## Use

Run the executable. The widget appears in the Windows taskbar.

- Drag the left divider to move it
- Left-click the widget or tray icon to switch providers
- Right-click for refresh, provider selection, update frequency, summaries, startup, language, updates, and exit
- Use `--diagnose` to write diagnostic logs
- Use `--diagnose-once --diagnose` to run a one-shot OpenCode diagnostic poll

Settings are stored at:

```text
%APPDATA%\OpencodeUsageMonitor\settings.json
```

OpenCode daily snapshots are stored at:

```text
%APPDATA%\OpencodeUsageMonitor\log\YYYY-MM-DD.md
%APPDATA%\OpencodeUsageMonitor\log\YYYY-MM-DD.json
```

## Privacy And Security

This project is open source, and the OpenCode integration is intentionally local-first.

What the app reads:

- OpenCode SQLite data from `%LOCALAPPDATA%\opencode\`
- Claude Code OAuth credentials from `~/.claude/.credentials.json` when Claude tracking is used
- Claude Code credentials inside WSL when needed and available
- Codex auth and history files from `%USERPROFILE%\.codex` when Codex tracking is used

What the app sends over the network:

- No network requests for OpenCode tracking
- Anthropic requests for Claude usage when Claude tracking is used
- OpenAI/Codex requests or local Codex CLI probes when Codex tracking is used
- GitHub requests only for update checks, self-update, or optional history/report sync features

What the app does **not** do:

- It does not upload OpenCode project files
- It does not send credentials to a separate backend
- It does not collect analytics or telemetry
- It does not bypass endpoint-security tools

For managed corporate machines, use the normal IT-approved process for source review, code signing, or allowlisting trusted release artifacts. Do not disable or bypass security controls for local builds.

## Development

This is a Rust/Cargo Windows application.

For day-to-day validation, prefer `cargo dev-check`. It checks the code without linking a new `.exe`, which avoids unnecessary churn in `target\` and reduces repeated endpoint-security scans during development.

```powershell
cargo dev-check
```

To validate with the release profile without creating `target\release\opencode-usage-monitor.exe`:

```powershell
cargo release-check
```

To run locally with a debug build:

```powershell
cargo dev-run
```

To build the release executable:

```powershell
cargo release-build
```

Recommended pre-release checks:

```powershell
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo release-build
cargo package --allow-dirty
```

`cargo package --allow-dirty` is useful while reviewing uncommitted release changes. For an actual release, commit the changes first and run `cargo package` without `--allow-dirty`.

## Release Notes For Maintainers

- GitHub Releases are created by pushing a `v*` tag.
- The release workflow builds `target/release/opencode-usage-monitor.exe`.
- WinGet submission uses `CodeZeno.OpencodeUsageMonitor` when `WINGETCREATE_GITHUB_TOKEN` is configured.
- Portable installs can self-update from the latest GitHub release asset.

## Relationship To The Upstream Project

The upstream project is [`CodeZeno/Claude-Code-Usage-Monitor`](https://github.com/CodeZeno/Claude-Code-Usage-Monitor), which focused on Claude Code and Codex usage visibility.

This fork changes the product direction to OpenCode-first monitoring while preserving the useful provider-limit views from upstream. The code remains MIT licensed.
