@echo off
chcp 65001 >nul
setlocal enabledelayedexpansion
cd /d "%~dp0"

echo ============================================
echo   fast-comment  Windows ビルド
echo ============================================
echo.

REM --- 前提ツールの確認 -------------------------------------------------
where node >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Node.js が見つかりません。https://nodejs.org からインストールしてください。
  goto :fail
)

where cargo >nul 2>nul
if errorlevel 1 (
  echo [ERROR] Rust(cargo) が見つかりません。https://rustup.rs からインストールしてください。
  goto :fail
)

REM tauri.conf.json の beforeBuildCommand が pnpm を使うため pnpm が必須
where pnpm >nul 2>nul
if errorlevel 1 (
  echo pnpm が見つかりません。corepack で有効化を試みます...
  corepack enable pnpm >nul 2>nul
  where pnpm >nul 2>nul
  if errorlevel 1 (
    echo [ERROR] pnpm を用意できませんでした。次のいずれかを実行してください:
    echo         corepack enable pnpm
    echo         npm install -g pnpm
    goto :fail
  )
)

REM --- 依存関係 ---------------------------------------------------------
echo [1/2] 依存関係をインストール ^(pnpm install^)...
call pnpm install
if errorlevel 1 (
  echo [ERROR] pnpm install に失敗しました。
  goto :fail
)
echo.

REM --- ビルド -----------------------------------------------------------
echo [2/2] Tauri ビルド ^(pnpm tauri build^) ... 数分かかります
call pnpm tauri build
if errorlevel 1 (
  echo [ERROR] tauri build に失敗しました。上のログを確認してください。
  goto :fail
)

echo.
echo ============================================
echo   ビルド成功
echo ============================================
echo  NSIS インストーラ : src-tauri\target\release\bundle\nsis\   ^(*-setup.exe^)
echo  MSI インストーラ  : src-tauri\target\release\bundle\msi\    ^(*.msi^)
echo  ポータブル exe    : src-tauri\target\release\fast-comment.exe
echo.
echo  ※ NSIS インストーラはインストール時にデスクトップへショートカットを作成します。
echo.
choice /c YN /n /m "出力フォルダ(bundle)を開きますか? [Y/N] "
if not errorlevel 2 start "" explorer "src-tauri\target\release\bundle"
echo.
pause
exit /b 0

:fail
echo.
pause
exit /b 1
