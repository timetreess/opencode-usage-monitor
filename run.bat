@echo off
setlocal

set "ROOT=%~dp0"
set "EXE=%ROOT%target\release\opencode-usage-monitor.exe"
set "CARGO=cargo"

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

powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -Command "$processes = @(Get-Process -Name 'opencode-usage-monitor' -ErrorAction SilentlyContinue); if ($processes.Count -gt 0) { Write-Host 'Restarting existing OpenCode Usage Monitor instance...'; $ids = $processes.Id; $processes | Stop-Process -Force -ErrorAction Stop; Wait-Process -Id $ids -Timeout 10 -ErrorAction SilentlyContinue }"
if errorlevel 1 (
    echo Unable to restart the existing OpenCode Usage Monitor process.
    echo Close it from Task Manager and run this script again.
    pause >nul
    exit /b 1
)

start "" "%EXE%"
echo OpenCode Usage Monitor started.
ping -n 3 127.0.0.1 >nul
