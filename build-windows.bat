@echo off
REM ASCII-only: cmd.exe parses .bat with OEM codepage (CP932 on JP Windows).
REM Non-ASCII bytes corrupt batch parsing. Keep this file ASCII + CRLF only.
REM Installs missing prerequisites, then builds fast-comment for Windows.
setlocal
cd /d "%~dp0"

echo ============================================
echo   fast-comment  Windows build helper
echo   Installs missing prerequisites, then builds
echo ============================================
echo.

REM =========================================================
REM  Detect winget
REM =========================================================
set "HAS_WINGET=0"
where winget >nul 2>nul
if not errorlevel 1 (
  set "HAS_WINGET=1"
  for /f "delims=" %%V in ('winget --version 2^>nul') do echo [OK] winget %%V
)

REM =========================================================
REM  Node.js  -- required, not auto-installed
REM =========================================================
where node >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Node.js not found.
  echo         Install from https://nodejs.org and re-run this script.
  goto :fail
)
for /f "delims=" %%V in ('node --version 2^>nul') do echo [OK] Node.js %%V

REM =========================================================
REM  Rust / cargo -- PATH self-repair then detect
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

set "NEED_RUST=0"
cargo --version >nul 2>nul
if errorlevel 1 set "NEED_RUST=1"
if "%NEED_RUST%"=="0" (
  for /f "delims=" %%V in ('cargo --version 2^>nul') do echo [OK] %%V
)

REM =========================================================
REM  MSVC build tools -- detect via vswhere
REM =========================================================
set "NEED_MSVC=0"
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" set "VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
  "%VSWHERE%" -latest -requires Microsoft.VisualCpp.Tools.HostX64.TargetX64 -find "VC\Tools\MSVC\*\bin\HostX64\x64\link.exe" >nul 2>nul
  if errorlevel 1 (
    set "NEED_MSVC=1"
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
REM  Prompt to install missing prerequisites
REM =========================================================
if "%NEED_RUST%"=="0" if "%NEED_MSVC%"=="0" goto :prereqs_ok

echo.
echo [INFO] Missing prerequisites detected:
if "%NEED_RUST%"=="1" echo   - Rust toolchain ^(rustup + stable-msvc^)
if "%NEED_MSVC%"=="1" echo   - Microsoft Visual C++ Build Tools 2022
echo.
choice /c YN /n /m "Install missing prerequisites now? [Y/N] "
if errorlevel 2 (
  echo.
  echo [INFO] Installation declined. Install manually:
  echo          Rust  : https://rustup.rs
  echo          MSVC  : https://aka.ms/vs/17/release/vs_BuildTools.exe
  echo.
  goto :fail
)
echo.

REM =========================================================
REM  Install Rust if needed
REM =========================================================
if "%NEED_RUST%"=="0" goto :rust_skip

echo [1/2] Installing Rust toolchain...
echo.
if "%HAS_WINGET%"=="1" (
  winget install -e --id Rustlang.Rustup --accept-source-agreements --accept-package-agreements
  if errorlevel 1 (
    echo [WARN] winget install Rustlang.Rustup returned an error.
    echo        Falling back to rustup-init.exe download...
    goto :rust_fallback
  )
  echo [OK] Rust installed via winget.
  goto :rust_done
)

:rust_fallback
echo [INFO] Downloading rustup-init.exe via PowerShell...
powershell -NoProfile -ExecutionPolicy Bypass -Command "Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile ([System.IO.Path]::Combine($env:TEMP,'rustup-init.exe'))"
if errorlevel 1 (
  echo [ERROR] Failed to download rustup-init.exe.
  echo         Check your internet connection and try again, or install manually:
  echo           https://rustup.rs
  goto :fail
)
echo [INFO] Running rustup-init.exe ^(-y --default-toolchain stable-msvc^)...
"%TEMP%\rustup-init.exe" -y --default-toolchain stable-msvc
if errorlevel 1 (
  echo [ERROR] rustup-init.exe failed. See output above.
  goto :fail
)
echo [OK] Rust installed via rustup-init.exe.

:rust_done
echo.

:rust_skip

REM =========================================================
REM  Install MSVC if needed
REM =========================================================
if "%NEED_MSVC%"=="0" goto :msvc_skip

echo [2/2] Installing Microsoft Visual C++ Build Tools 2022...
echo        ^(A UAC elevation prompt may appear^)
echo.
if "%HAS_WINGET%"=="1" (
  winget install --id Microsoft.VisualStudio.2022.BuildTools --accept-source-agreements --accept-package-agreements --silent --override "--wait --quiet --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.Windows11SDK.26100 --includeRecommended"
  if errorlevel 1 (
    echo [WARN] winget returned a non-zero exit code for VS Build Tools.
    echo        This may be normal if a reboot is pending ^(exit code 3010^).
    echo        If the installer visibly completed, proceed with a reboot.
    echo        Otherwise install manually:
    echo          https://aka.ms/vs/17/release/vs_BuildTools.exe
    echo.
  ) else (
    echo [OK] VS Build Tools installed.
  )
) else (
  echo [INFO] winget not available. Install VS Build Tools manually:
  echo          https://aka.ms/vs/17/release/vs_BuildTools.exe
  echo        Select "Desktop development with C++" and include the Windows SDK.
  echo.
)

:msvc_skip

REM =========================================================
REM  After install: refresh PATH and re-verify cargo
REM =========================================================
if "%NEED_RUST%"=="0" goto :cargo_verify_skip

if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
  set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
)
cargo --version >nul 2>nul
if errorlevel 1 (
  echo.
  echo [INFO] cargo is not yet runnable in this session.
  echo        Sign out / in or reboot, then run this script again.
  echo.
  goto :done
)
for /f "delims=" %%V in ('cargo --version 2^>nul') do echo [OK] cargo is now available: %%V

:cargo_verify_skip

:prereqs_ok

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

REM --- Clean stale installers (prevents NSIS os error 5) -----------------
REM This script auto-launches *-setup.exe after a successful build. If that
REM installer is still open on a re-run, makensis cannot overwrite the
REM running exe and fails with "access denied (os error 5)". Remove old
REM bundle outputs first; if removal fails, the installer/app is still open.
set "BUNDLE_DIR=src-tauri\target\release\bundle"
if exist "%BUNDLE_DIR%\nsis\*-setup.exe" (
  del /q "%BUNDLE_DIR%\nsis\*-setup.exe" >nul 2>nul
  if exist "%BUNDLE_DIR%\nsis\*-setup.exe" (
    echo [ERROR] Cannot delete the previous installer in bundle\nsis\.
    echo         It is probably still running. Close the fast-comment
    echo         installer window / quit the app, then run this script again.
    goto :fail
  )
  echo [OK] Removed previous NSIS installer.
)
if exist "%BUNDLE_DIR%\msi\*.msi" del /q "%BUNDLE_DIR%\msi\*.msi" >nul 2>nul

REM --- Step 2: Tauri build (also runs "pnpm build" via beforeBuildCommand)
echo [2/2] pnpm tauri build  ^(first run compiles all Rust crates; takes several minutes^)
call pnpm tauri build
if errorlevel 1 (
  echo [ERROR] tauri build failed. Review the output above for details.
  echo         Note: if MSVC was just installed, a reboot may be required before building.
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

REM --- Auto-launch the NSIS installer (newest *-setup.exe) -------------
set "NSIS_DIR=src-tauri\target\release\bundle\nsis"
set "SETUP_EXE="
for %%F in ("%NSIS_DIR%\*-setup.exe") do set "SETUP_EXE=%%F"
if defined SETUP_EXE (
  echo  Launching installer: %SETUP_EXE%
  start "" "%SETUP_EXE%"
  REM Installer runs in its own process; close this window automatically.
  exit /b 0
)
echo [WARN] NSIS setup exe not found under %NSIS_DIR%.
echo        Opening the bundle output folder instead.
start "" "explorer.exe" "src-tauri\target\release\bundle"
echo.
pause
exit /b 0

:done
pause
exit /b 0

:fail
echo.
echo [FAILED] Fix the error above and run this script again.
pause
exit /b 1
