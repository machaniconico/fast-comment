@echo off
REM ASCII-only: cmd.exe parses .bat with OEM codepage (CP932 on JP Windows).
REM Non-ASCII bytes corrupt batch parsing. Keep this file ASCII + CRLF only.
setlocal
cd /d "%~dp0"

echo ============================================
echo   fast-comment  Windows build helper
echo ============================================
echo.

REM =========================================================
REM  Node.js
REM =========================================================
where node >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Node.js not found.
  echo         Install from https://nodejs.org and re-run this script.
  goto :fail
)
for /f "delims=" %%V in ('node --version 2^>nul') do echo [OK] Node.js %%V

REM =========================================================
REM  Rust / cargo
REM  Explorer-launched cmd.exe does not inherit the updated PATH that
REM  rustup-init wrote to the registry. Self-heal for this session.
REM =========================================================
if defined CARGO_HOME (
  if exist "%CARGO_HOME%\bin\cargo.exe" set "PATH=%CARGO_HOME%\bin;%PATH%"
) else (
  if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
    echo [INFO] Adding %USERPROFILE%\.cargo\bin to PATH for this session.
    set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
  )
)

REM Verify cargo actually runs, not just that the file exists.
cargo --version >nul 2>nul
if errorlevel 1 (
  echo [ERROR] cargo not found or not runnable.
  echo.
  echo   Likely causes ^(most common first^):
  echo   1. Rust was just installed but Explorer still has the old PATH.
  echo      FIX: sign out of Windows and back in, then run this script again.
  echo   2. rustup-init did not complete ^(closed early, or MSVC tools missing^).
  echo      FIX: re-run https://rustup.rs and accept all defaults.
  echo   3. Rust was installed as a different user ^(e.g. as Administrator^).
  echo      FIX: re-install rustup as the current user without elevation.
  echo   4. CARGO_HOME points to a non-standard path.
  echo      FIX: run  echo %%CARGO_HOME%%  and look for cargo.exe there.
  echo.
  echo   Run setup-windows.bat to install Rust and MSVC build tools automatically.
  echo.
  goto :fail
)
for /f "delims=" %%V in ('cargo --version 2^>nul') do echo [OK] %%V

REM =========================================================
REM  MSVC build tools (Tauri needs link.exe for the Rust MSVC target)
REM =========================================================
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" set "VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
  "%VSWHERE%" -latest -requires Microsoft.VisualCpp.Tools.HostX64.TargetX64 -find "VC\Tools\MSVC\*\bin\HostX64\x64\link.exe" >nul 2>nul
  if errorlevel 1 (
    echo [WARN] MSVC C++ build tools not detected via vswhere.
    echo        Tauri needs the MSVC linker ^(link.exe^) to build on Windows.
    echo        Install "Desktop development with C++" from:
    echo          https://aka.ms/vs/17/release/vs_BuildTools.exe
    echo        If already installed, reboot and try again.
    echo        Continuing -- cargo build will fail later if the linker is absent.
    echo.
  ) else (
    echo [OK] MSVC C++ build tools found.
  )
) else (
  echo [WARN] vswhere.exe not found -- cannot verify MSVC build tools.
  echo        If the build fails with linker errors, install from:
  echo          https://aka.ms/vs/17/release/vs_BuildTools.exe
  echo.
)

REM =========================================================
REM  pnpm  (required: tauri.conf.json beforeBuildCommand = "pnpm build")
REM =========================================================
where pnpm >nul 2>nul
if errorlevel 1 (
  echo [INFO] pnpm not found. Trying corepack enable pnpm...
  corepack enable pnpm
  where pnpm >nul 2>nul
  if errorlevel 1 (
    echo [ERROR] pnpm still not found after corepack enable.
    echo         Fix with one of:
    echo           corepack enable pnpm
    echo           npm install -g pnpm
    goto :fail
  )
)
for /f "delims=" %%V in ('pnpm --version 2^>nul') do echo [OK] pnpm %%V

echo.
echo ============================================
echo  All prerequisites OK. Starting build.
echo ============================================
echo.

REM --- Step 1: install JS dependencies --------------------------------
echo [1/2] pnpm install
call pnpm install
if errorlevel 1 (
  echo [ERROR] pnpm install failed.
  goto :fail
)
echo.

REM --- Step 2: Tauri build (also runs "pnpm build" via beforeBuildCommand)
echo [2/2] pnpm tauri build  ^(first run compiles all Rust crates; takes several minutes^)
call pnpm tauri build
if errorlevel 1 (
  echo [ERROR] tauri build failed. Review the output above for details.
  goto :fail
)

echo.
echo ============================================
echo   BUILD SUCCEEDED
echo ============================================
echo  NSIS installer : src-tauri\target\release\bundle\nsis\   ^(*-setup.exe^)
echo  MSI  installer : src-tauri\target\release\bundle\msi\    ^(*.msi^)
echo  Portable exe   : src-tauri\target\release\fast-comment.exe
echo.
echo  The NSIS installer creates a desktop shortcut on install.
echo.
choice /c YN /n /m "Open the output folder (bundle)? [Y/N] "
if not errorlevel 2 start "" "explorer.exe" "src-tauri\target\release\bundle"
echo.
pause
exit /b 0

:fail
echo.
echo [FAILED] Fix the error above and run this script again.
pause
exit /b 1
