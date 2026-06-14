# fast-comment

**わんコメ(OneComme)相当**の機能を持つ、より軽量・低遅延なマルチプラットフォーム・コメントビューワー。
Twitch / YouTube のライブコメントを1つの画面に集約し、OBS オーバーレイ表示・読み上げ(TTS)・モデレーションまでをカバーします。

> 重い処理（接続・解析・正規化・バッファリング・配信）はすべて Rust 側で行い、UI は描画に専念する設計です。高負荷時でも滑らかに動きます。

## 主な機能

- **マルチプラットフォーム集約** — Twitch(IRC-WS) / YouTube(InnerTube) のコメントを単一の統一モデルに正規化して一覧表示
- **URL自動判別でチャンネル追加** — 配信URLを貼るだけでプラットフォーム/チャンネルを判別して追加
- **低遅延・高負荷耐性** — バックプレッシャ + フレーム単位バッチ送出 + UI仮想スクロール
- **YouTube仕様変更に強い** — 寛容パース + `config.json` の `youtubeOverrides` で再ビルド無しにホットパッチ
- **OBS オーバーレイ** — 内蔵HTTP/WebSocketサーバでコメントをリアルタイム配信。9種類のテンプレート(default / simple / bubble / ticker / ranking / goals / donation / timer / danmaku)・フォント倍率・最大行数・表示時間・背景透過・表示位置などを設定可能
- **弾幕オーバーレイ** — ニコ生風に画面を流れるコメント。デスクトップ用の透過・クリックスルー・最前面オーバーレイ窓と、OBSブラウザソース用 `danmaku` テンプレの両方に対応
- **読み上げ(TTS) 3バックエンド** — 棒読みちゃん / VOICEVOX / ブラウザ Web Speech をアダプタ化。速度・音程・音量・声質、名前読み上げ/URL省略/絵文字除去/最大文字数などを細かく調整可能。棒読みちゃんは自動起動にも対応
- **投げ銭の別表示** — SuperChat / Bits / メンバーシップを通常コメントと分けて表示（アプリ内タブ / OBS `?only=gift`）。既定OFF
- **参加型配信の管理** — キーワード(既定「参加」)での参加登録、先着/ランダム抽選、専用タブ。既定OFF
- **モデレーション(ローカル)** — NGワード/NGユーザー(正規表現)・ハイライトルール。MVPはローカル非表示/グレー化のみ（実BAN/削除はOAuth必須でP6予定）
- **コマンドパレット** — `Ctrl+K` でアクション実行・設定ジャンプ・コメント検索
- **コメントピン留め / キーワード通知音** — 重要コメントの固定表示、一致コメント到着時の効果音通知
- **起動時の更新チェック** — GitHub Releases の最新タグを semver 比較し、新版があれば通知バナーを表示

## 技術スタック

| 領域 | 採用 |
|---|---|
| シェル | Tauri 2.x (Rust) |
| UI | Svelte 5 + Vite + TypeScript |
| 非同期 | tokio |
| 接続 | tokio-tungstenite (Twitch IRC-WS) / reqwest (YouTube InnerTube) |
| OBS配信サーバ | axum (HTTP + WebSocket) + tower-http |

## アーキテクチャ

```
[Twitch IRC-WS] ┐
                ├─> Source trait ──> 正規化(ChatMessage) ──> Bus(tokio broadcast)
[YouTube InnerTube] ┘                                          │
                                                  ┌────────────┼─────────────┐
                                                  ▼            ▼             ▼
                                          Tauri IPC(UI)   axum WS(OBS)    TTS dispatch
                                          rAFバッチ描画   テンプレ配信     bouyomi/voicevox/webspeech
```

- **Source 層** (`src-tauri/src/sources/`): 接続元を trait で抽象化。Twitch の PING には必ず PONG を返し、YouTube は固い deserialize を避けた寛容パース
- **Bus 層** (`src-tauri/src/bus.rs`): tokio broadcast で UI / OBS / TTS へ配信
- **設定永続化** (`src-tauri/src/config.rs`): アプリ設定はバックエンドの `config.json` を正本として永続化する。新フィールドは `serde(default)` で後方互換。一部のUI/ウィンドウ局所設定(テーマ・弾幕表示・最前面ピン・コマンド履歴など)は各ウィンドウの `localStorage` に保持する

## ビルド

> **重要**: ビルドは **Windows 側** で行います。WSL は編集専用です（WSL で `tauri build` すると Linux バイナリになります）。

### 前提

- Windows 10/11
- Node.js（pnpm 推奨）
- Rust ツールチェーン + MSVC ビルドツール（`build-windows.bat` が不足分を自動インストール）

### 手順

```bat
:: 依存導入からビルドまでを単一エントリで実行
build-windows.bat
```

`build-windows.bat` は setup を統合した単一エントリです。Rust / MSVC が無ければ自動インストールし、そのままビルドまで進みます。ビルド成功後は既定で出力フォルダ(`src-tauri\target\release\bundle`)を開きます。生成されたインストーラー(`*-setup.exe`)を起動したまま再ビルドすると NSIS が `os error 5 (access denied)` で失敗するため、自動起動は既定 OFF です。自動起動させたい場合は `set FC_AUTORUN_SETUP=1` を実行してからスクリプトを起動してください。

開発時のフロントエンド検証のみ WSL でも可能です:

```bash
npm run check   # svelte-check（型/Svelte検証）
npm run build   # vite build
```

## 設定

設定はアプリ内の設定画面から行えます。永続化先はユーザーデータディレクトリ配下の `config.json` です。YouTube の仕様変更には `youtubeOverrides`（API キー / clientVersion / 抽出パス上書き）で再ビルド無しに対応できます。

## ロードマップ

- [x] P0 足場（設定/モデル/Tauri・Svelte雛形/ビルド導線）
- [x] P1 Twitch接続コア（仮想化一覧 + rAF）
- [x] P2 YouTube（InnerTube + 寛容パーサ + overrides）
- [x] P3 OBS（axum WS + テンプレート）
- [x] P4 TTS（3バックエンド + ルーティング）
- [x] P5 モデレーション + 設定UI仕上げ
- [ ] P6 OAuth実モデレーション / テンプレ編集UI / niconico等の追加Source

## ライセンス

[MIT](./LICENSE)
