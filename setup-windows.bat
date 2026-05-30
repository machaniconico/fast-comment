@echo off
REM ASCII-only: cmd.exe parses .bat with OEM codepage (CP932 on JP Windows).
REM Non-ASCII bytes corrupt batch parsing. Keep this file ASCII + CRLF only.
setlocal
cd /d "%~dp0"

echo ============================================
echo   fast-comment  Windows setup helper
echo   Installs Rust and MSVC C++ build tools
echo ============================================
echo.

REM =========================================================
REM  Check for admin elevation (VS Build Tools requires it)
REM =========================================================
net session >nul 2>nul
if errorlevel 1 (
  echo [WARN] This script is NOT running as Administrator.
  echo        Visual Studio Build Tools installation requires elevation.
  echo        If the MSVC install step fails, re-run this script as Administrator.
  echo.
)

REM =========================================================
REM  Detect winget
REM =========================================================
where winget >nul 2>nul
if errorlevel 1 (
  set "HAS_WINGET=0"
  echo [INFO] winget not found. Rust will be installed via rustup-init.exe.
  echo        MSVC Build Tools must be installed manually ^(URL shown below^).
  echo.
) else (
  set "HAS_WINGET=1"
  for /f "delims=" %%V in ('winget --version 2^>nul') do echo [OK] winget %%V
)

REM =========================================================
REM  Rust / cargo -- check if already present
REM =========================================================
if defined CARGO_HOME (
  if exist "%CARGO_HOME%\bin\cargo.exe" set "PATH=%CARGO_HOME%\bin;%PATH%"
) else (
  if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
    set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
  )
)

cargo --version >nul 2>nul
if not errorlevel 1 (
  for /f "delims=" %%V in ('cargo --version 2^>nul') do echo [OK] Rust already installed: %%V
  echo        Nothing to do. Run build-windows.bat to build the project.
  echo.
  goto :done
)

echo [INFO] cargo not found. Rust toolchain needs to be installed.
echo.

REM =========================================================
REM  Prompt user
REM =========================================================
echo  This script will install:
echo    - Rust toolchain ^(rustup + stable-msvc^)
echo    - Microsoft Visual C++ Build Tools 2022 with Windows 11 SDK
echo.
echo  Note: A UAC ^(admin^) prompt may appear for the MSVC installation.
echo        After installation, sign out and back in ^(or reboot^) to refresh PATH,
echo        then run build-windows.bat.
echo.
choice /c YN /n /m "Install Rust and C++ build tools automatically? [Y/N] "
if errorlevel 2 (
  echo.
  echo [INFO] Installation cancelled. Install manually:
  echo          Rust  : https://rustup.rs
  echo          MSVC  : https://aka.ms/vs/17/release/vs_BuildTools.exe
  echo.
  goto :done
)
echo.

REM =========================================================
REM  Install Rust
REM =========================================================
echo [1/2] Installing Rust via winget...
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

REM =========================================================
REM  Install MSVC C++ Build Tools
REM =========================================================
echo [2/2] Installing Microsoft Visual C++ Build Tools 2022...
echo        ^(A UAC elevation prompt will appear^)
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

REM =========================================================
REM  Attempt PATH refresh for this session
REM =========================================================
echo.
echo [INFO] Attempting to refresh PATH for this session...
if exist "%USERPROFILE%\.cargo\bin\cargo.exe" (
  set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
  cargo --version >nul 2>nul
  if not errorlevel 1 (
    for /f "delims=" %%V in ('cargo --version 2^>nul') do echo [OK] cargo is now available in this session: %%V
  ) else (
    echo [INFO] cargo not yet runnable in this session ^(expected after fresh install^).
  )
) else (
  echo [INFO] cargo.exe not found at %USERPROFILE%\.cargo\bin yet.
)

echo.
echo ============================================
echo   NEXT STEPS
echo ============================================
echo.
echo   1. Sign out of Windows and back in ^(or reboot^) to apply PATH changes.
echo   2. Run build-windows.bat to compile and package fast-comment.
echo.
echo   Verification commands ^(run in a new Command Prompt after sign-in^):
echo     cargo --version
echo     rustc --version
echo     rustup toolchain list    ^(look for stable-msvc^)
echo     pnpm tauri info          ^(checks all Tauri build dependencies^)
echo.

:done
pause
exit /b 0

:fail
echo.
echo [FAILED] Fix the error above, then re-run this script.
echo.
pause
exit /b 1
