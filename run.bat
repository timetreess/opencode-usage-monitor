@echo off
setlocal

set "ROOT=%~dp0"
set "EXE=%ROOT%target\release\opencode-usage-monitor.exe"
set "CARGO=cargo"

powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "if (Get-Process -Name 'opencode-usage-monitor' -ErrorAction SilentlyContinue) { exit 0 } exit 1" >nul 2>nul
if not errorlevel 1 (
    echo OpenCode Usage Monitor is already running.
    echo Check the Windows taskbar or tray area.
    ping -n 4 127.0.0.1 >nul
    exit /b 0
)

where cargo >nul 2>nul
if errorlevel 1 (
    set "CARGO=%USERPROFILE%\.cargo\bin\cargo.exe"
)

if not exist "%EXE%" (
    echo Release executable not found. Building it now...
    pushd "%ROOT%" >nul
    "%CARGO%" release-build
    if errorlevel 1 (
        popd >nul
        echo.
        echo Build failed. Press any key to close this window.
        pause >nul
        exit /b 1
    )
    popd >nul
)

start "" "%EXE%"
echo OpenCode Usage Monitor started.
ping -n 3 127.0.0.1 >nul
