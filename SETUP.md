# fast-comment セットアップ（Windows 側で実行）

> ⚠️ **ビルド/実行は Windows 側で行います。** WSL で `tauri` を動かすと Linux 用バイナリになり、
> OBS(Windows) で使えません。コードは `D:\workspace\fast-comment`（WSL/Windows 共有）にあるので、
> 編集は WSL 側の Claude、ビルド・起動は Windows ターミナルという分担です。

## 1. 前提ツールのインストール（初回のみ）

PowerShell（管理者推奨）で:

```powershell
# Rust (rustup-init.exe)
winget install Rustlang.Rustup
# または https://rustup.rs/ から rustup-init.exe を実行

# WebView2 ランタイム（Win11/最新Win10は同梱済みのことが多い。無ければ）
winget install Microsoft.EdgeWebView2Runtime

# Node 用パッケージマネージャ pnpm（未導入なら）
npm install -g pnpm
```

Rust 導入後、ターミナルを開き直して確認:

```powershell
rustc --version
cargo --version
```

## 2. 依存インストール（プロジェクト直下で）

```powershell
cd D:\workspace\fast-comment
pnpm install
```

> `node_modules` は実行する OS 側で入れること。Windows で動かすなら Windows で `pnpm install`。

## 3. アイコン生成（初回のみ・ビルドに必須）

Tauri はビルド時に `src-tauri/icons/`（png / .ico / .icns）を要求します。
ソース PNG `src-tauri\app-icon.png`（1024x1024、リポジトリ同梱）から全形式を一括生成:

```powershell
pnpm tauri icon src-tauri\app-icon.png
```

`src-tauri\icons\` に `32x32.png` `128x128.png` `128x128@2x.png` `icon.ico` `icon.icns` が生成されます。
独自ロゴに差し替えたい場合は `app-icon.png` を上書きしてから再実行。
（dev 起動だけなら無くても動く場合がありますが、配布ビルドには必須）

## 4. 開発起動

```powershell
pnpm tauri dev
```

初回は Rust の依存ビルドで数分かかります。2回目以降は高速。

## 5. 配布ビルド

```powershell
pnpm tauri build
```

`src-tauri/target/release/bundle/` に成果物（.exe / .msi）。

## 6. OBS での使い方

1. fast-comment を起動し、Twitch チャンネル名 / YouTube の videoId（または配信URL）を登録。
2. アプリ内に表示される OBS 用 URL（既定 `http://127.0.0.1:11180/?template=default`）をコピー。
3. OBS の「ソース追加 → ブラウザ」にそのURLを貼る。

## トラブルシュート

- `cargo` が見つからない → ターミナル再起動。PATH に `%USERPROFILE%\.cargo\bin`。
- WebView2 エラー → 上記ランタイムを導入。
- YouTube が映らない → InnerTube 仕様変更の可能性。`logs/yt-unparsed.jsonl` を確認し、
  `config.json` の `youtubeOverrides`（apiKey / clientVersion / 抽出パス）で調整（再ビルド不要）。
