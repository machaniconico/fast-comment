@echo off
REM ASCII-only on purpose: cmd.exe parses .bat bytes with the OEM codepage
REM (CP932 on Japanese Windows), so non-ASCII text corrupts batch parsing.
setlocal
cd /d "%~dp0"

echo ============================================
echo   fast-comment  Windows build
echo ============================================
echo.

REM --- Prerequisites ---------------------------------------------------
where node >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Node.js not found. Install from https://nodejs.org
  goto :fail
)

where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Rust/cargo not found. Install from https://rustup.rs
  goto :fail
)

REM tauri.conf.json beforeBuildCommand uses pnpm, so pnpm is required.
where pnpm >nul 2>nul
if errorlevel 1 (
  echo pnpm not found. Trying to enable it via corepack...
  corepack enable pnpm >nul 2>nul
  where pnpm >nul 2>nul
  if errorlevel 1 (
    echo [ERROR] Could not provide pnpm. Run one of:
    echo         corepack enable pnpm
    echo         npm install -g pnpm
    goto :fail
  )
)

REM --- Install ---------------------------------------------------------
echo [1/2] Installing dependencies: pnpm install
call pnpm install
if errorlevel 1 (
  echo [ERROR] pnpm install failed.
  goto :fail
)
echo.

REM --- Build -----------------------------------------------------------
echo [2/2] Building: pnpm tauri build  ^(takes a few minutes^)
call pnpm tauri build
if errorlevel 1 (
  echo [ERROR] tauri build failed. Check the log above.
  goto :fail
)

echo.
echo ============================================
echo   BUILD SUCCEEDED
echo ============================================
echo  NSIS installer : src-tauri\target\release\bundle\nsis\   ^(*-setup.exe^)
echo  MSI installer  : src-tauri\target\release\bundle\msi\    ^(*.msi^)
echo  Portable exe   : src-tauri\target\release\fast-comment.exe
echo.
echo  The NSIS installer creates a desktop shortcut on install.
echo.
choice /c YN /n /m "Open the output folder (bundle)? [Y/N] "
if not errorlevel 2 start "" explorer "src-tauri\target\release\bundle"
echo.
pause
exit /b 0

:fail
echo.
pause
exit /b 1
