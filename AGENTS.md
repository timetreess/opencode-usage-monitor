# AGENTS.md

Guidance for AI agents working in this repository.

## Project Summary

- Project: `opencode-usage-monitor`
- Platform: Windows 10/11
- Language/tooling: Rust 2021 + Cargo
- Output binary: `target/release/opencode-usage-monitor.exe`
- Upstream origin: forked from `https://github.com/CodeZeno/Claude-Code-Usage-Monitor`
- Product direction: OpenCode-first Windows taskbar usage monitor while preserving Claude Code and Codex usage views.

## What The App Does

- Reads OpenCode usage locally from `%LOCALAPPDATA%\opencode\opencode.db` or another `opencode-*.db` file.
- Shows OpenCode 5h/today/month token totals, cost estimates, and daily snapshots.
- Also supports Claude Code and Codex usage windows when those tools are installed and authenticated.
- Stores app settings under `%APPDATA%\OpencodeUsageMonitor\settings.json`.
- Stores OpenCode daily snapshots under `%APPDATA%\OpencodeUsageMonitor\log\`.

## Required Local Tooling

Install the Rust Windows MSVC toolchain before building:

```powershell
rustup toolchain install stable-x86_64-pc-windows-msvc
rustup default stable-x86_64-pc-windows-msvc
rustup component add rustfmt clippy rust-analyzer
```

If the current shell cannot find `cargo`, it may still exist at:

```powershell
%USERPROFILE%\.cargo\bin\cargo.exe
```

Use the full path when needed:

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test
```

## Cargo Aliases

This repo defines aliases in `.cargo/config.toml`:

```powershell
cargo dev-check       # cargo check; does not link/create an exe
cargo release-check   # cargo check --release; release profile without creating target/release/*.exe
cargo dev-run         # cargo run; debug build and run
cargo release-build   # cargo build --release; creates target/release/opencode-usage-monitor.exe
```

Prefer `cargo dev-check` or `cargo release-check` during normal development. They avoid unnecessary `.exe` churn and reduce repeated endpoint-security scans on corporate machines.

## Install And Run

For a local debug run:

```powershell
cargo dev-run
```

For a release build:

```powershell
cargo release-build
```

Run the release executable directly:

```powershell
.\target\release\opencode-usage-monitor.exe
```

Useful diagnostic modes:

```powershell
.\target\release\opencode-usage-monitor.exe --diagnose
.\target\release\opencode-usage-monitor.exe --diagnose-once --diagnose
```

The one-shot diagnostic mode is useful when checking OpenCode database discovery without keeping the taskbar widget running.

## Verification Before Changes Are Considered Done

Run these before claiming a code change is complete:

```powershell
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo release-build
cargo package
```

If there are intentionally uncommitted release changes and `cargo package` blocks on a dirty tree, use this only as an interim check:

```powershell
cargo package --allow-dirty
```

For an actual release, commit first and then run `cargo package` without `--allow-dirty`.

## Release Workflow

- GitHub Actions release workflow: `.github/workflows/release.yml`
- Trigger: push a tag matching `v*`
- Release asset name: `opencode-usage-monitor.exe`
- WinGet distribution: not published for this fork. The release workflow contains a legacy placeholder identifier; register a fork-specific package before enabling submission.
- Portable installs self-update from the latest GitHub release asset.

Before tagging a release:

1. Ensure `git status` is clean.
2. Run the full verification command set above.
3. Confirm `Cargo.toml` version matches the intended tag.
4. Push commits, then push the `v*` tag.

## Important Naming And Paths

Keep these names aligned across code, docs, and release workflow:

- Cargo package name: `opencode-usage-monitor`
- Executable/release asset: `opencode-usage-monitor.exe`
- Windows resource product name: `OpenCode Usage Monitor`
- App data folder: `OpencodeUsageMonitor`
- Startup registry key: `OpencodeUsageMonitor`
- WinGet distribution: not published for this fork. The release workflow contains a legacy placeholder identifier; register a fork-specific package before enabling submission.

Search for old names before release:

```powershell
git grep "claude-code-usage-monitor.exe\|CodeZeno.ClaudeCodeUsageMonitor\|ClaudeCodeUsageMonitor"
```

Some references to Claude Code are expected because Claude Code remains a supported provider. Old executable, package, registry, or app-folder names are not expected.

## Security And Privacy Rules

- Do not add telemetry or a backend service.
- Do not upload OpenCode project files.
- Do not bypass, disable, or evade endpoint-security tools.
- Use `cargo dev-check` / `cargo release-check` to avoid unnecessary executable rebuilds instead.
- For corporate environments, recommend normal IT-approved source review, code signing, or artifact allowlisting.
- Do not commit credentials, tokens, `.env` files, or local app data.

## Notes For AI Agents

- This is a Windows-native app; commands should be PowerShell-compatible.
- Avoid broad refactors unless requested. The Win32 UI code in `src/window.rs` is stateful and easy to regress.
- Match existing naming and path conventions exactly.
- If changing provider behavior, verify all three providers: OpenCode, Claude, and Codex.
- If changing release metadata, check `Cargo.toml`, `.github/workflows/release.yml`, `src/updater.rs`, and `README.md` together.
