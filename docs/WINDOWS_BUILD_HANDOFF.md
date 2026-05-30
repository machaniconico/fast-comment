# Windows ビルド・ハンドオフ・チェックリスト

このドキュメントは、WSL での開発完了後、Windows 側での最終ビルド・検証・デプロイメント向けの手順書です。WSL 環境では `cargo` と `svelte-check` が利用できないため、**すべての最終検証は Windows 側で実施してください**。

> **ワンクリックビルド**: リポジトリ直下の **`build-windows.bat`** をダブルクリックすると、前提ツール確認 → `pnpm install` → `pnpm tauri build` を自動実行します。手動で進めたい場合は以下の手順を参照。
>
> **パッケージマネージャは pnpm を使用**: `tauri.conf.json` の `beforeBuildCommand` が `pnpm build` のため、`tauri build` は内部で pnpm を呼びます。pnpm 未導入なら `corepack enable pnpm`(Node同梱) または `npm i -g pnpm` で用意してください。

---

## 前提条件

以下のツールが Windows 環境にインストール済みであることを確認してください。

| ツール | 最小バージョン | 確認方法 |
|--------|---------------|--------|
| **Rust toolchain (stable)** | 1.77+ | `rustc --version` |
| **Node.js** | 18+ | `node --version` |
| **npm** または **pnpm** | 最新 | `npm --version` または `pnpm --version` |
| **Tauri CLI** | 2.x | `tauri --version` |
| **WebView2 Runtime** | latest | [webview2.dev](https://developer.microsoft.com/en-us/microsoft-edge/webview2/) から確認 |

### インストール手順（未導入の場合）

```powershell
# Rust (rustup)
# https://rustup.rs/ から rustup-init.exe をダウンロード・実行
rustup default stable

# Node.js + npm
# https://nodejs.org/ja/download/ から LTS をダウンロード・インストール

# Tauri CLI (npm経由)
npm install -g @tauri-apps/cli@2

# WebView2 Runtime
# https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-the-webview2-runtime
```

---

## 1. Rust バックエンド検証

### 1.1 ビルド確認

```powershell
cd src-tauri
cargo build
```

**期待結果:**
- 警告 (warning) は許容
- エラーなし (error count = 0)
- `target/debug/fast_comment.exe` (またはライブラリ) が生成される

**トラブルシューティング:**
- `error: linker 'cc.exe' not found` → Visual Studio Build Tools をインストール
- Cargo.lock が古い → `cargo update` を実行

---

### 1.2 ユニットテスト実行

```powershell
cd src-tauri
cargo test --lib
```

**期待結果:**
- 全テストが **PASS** する
- test result: `ok` という表示

**テスト一覧と検証観点:**

| モジュール | テストファイル | テスト名 | 検証観点 |
|-----------|---------------|---------|---------|
| **Config** | `config.rs` | `serde_roundtrip_keeps_camel_case_keys_and_values` | JSON 往復時の camelCase 保持、型の正確性 |
| | | `default_channel_enabled_true` | デフォルト `enabled: true` が効くこと |
| | | `parse_missing_field_ok` | 欠落フィールドで error でなく default を使う |
| **YouTube Parser** | `sources/youtube/parser.rs` | `dig_returns_value_or_none_for_missing_path` | 寛容パース: 途中欠落で None に落ちる |
| | | `extract_actions_filters_only_messages` | アクション抽出で余計なタイプを除外 |
| | | `parse_action_superchat_extracts_amount` | SuperChat の金額抽出確認 |
| | | `parse_runs_converts_emoji_to_emote_fragments` | 絵文字を Fragment に変換 |
| | | `split_currency_extracts_value_and_symbol` | 金額・通貨コード分離 |
| **Twitch IRCv3** | `sources/twitch.rs` | `tagged_bits_extracts_bits_and_kind` | Bits タグから MessageKind::Bits 抽出、金額取得 |
| | | `utf16_emotes_handled_correctly` | UTF-16 絵文字でも位置が正確 |
| | | `normal_privmsg_parsing` | 通常メッセージ: author 情報・Fragment 分割 |
| | | `badges_map_correctly_to_roles` | バッジ(broadcaster/mod/sub) を Roles にマップ |
| | | `pong_response_on_ping` | PING に PONG を返す動作確認 |
| **Moderation** | `moderation.rs` | `ng_word_regex_blocks_matches` | NG ワード(正規表現)が機能 |
| | | `ng_user_blocks_author` | NG ユーザーが完全除外される |
| | | `highlight_marks_flag` | ハイライトルールで flag がつく |
| **OBS Template Injection** | `bus.rs` (テスト確認) | テンプレ変数注入 | base 値、OBS ポート、template 名が正しく埋め込まれる |

**テスト実行結果の確認:**

```powershell
# 詳細出力で確認
cargo test --lib -- --nocapture
```

失敗があれば、出力から原因を特定し、WSL へフィードバック → 修正 → 再 Windows 検証。

---

### 1.3 Lint チェック（任意）

```powershell
cd src-tauri
cargo clippy --all-targets
```

警告は参考情報。本来エラーは `cargo test` で検出済み。

---

## 2. フロントエンド検証

### 2.1 依存関係インストール

```powershell
# リポジトリルートで
npm install
```

**期待結果:**
- `node_modules/` が作成される
- エラーなし

---

### 2.2 型チェック

```powershell
npm run check
```

**期待結果:**
- エラー数 = 0
- `Result: No issues found`

**トラブルシューティング:**
- TypeScript の型エラーが出たら、WSL で修正を指示
- Svelte コンポーネントの型警告は、`.svelte` ファイル内の `<script lang="ts">` を確認

---

### 2.3 ビルド（TypeScript + Vite）

```powershell
npm run build
```

**期待結果:**
- `dist/` ディレクトリが生成される
- エラーなし

**アーティファクト確認:**
- `dist/index.html` が存在
- `dist/assets/index-*.js` が存在

---

## 3. 統合ビルド（Tauri バイナリ生成）

```powershell
npm run tauri build
```

**期待結果:**
- Windows バイナリ (`.exe`) が `src-tauri/target/release/` に生成される
- `fast-comment_0.1.0_x64_en-US.msi` (インストーラ) が生成される

**ビルドに時間がかかります (5～15 分)**

---

### 3.1 既知の留意点：アイコン

**症状:** Tauri ビルド時に「アイコンが見つからない」エラー

```
error: Tauri application icon not found at src-tauri/icons/
```

**対応:**
- アイコンは `src-tauri/icons/` に格納
- `icon.png` (512x512) が必須
- 欠落時は、プロジェクト内の既存アイコンを確認し、ビルド前に配置してください
- ビルド後、`.exe` 実行時にアイコンが表示されていることを確認

---

## 4. スモークテスト（手動確認）

以下は GUI での実行確認です。ビルド成功後、生成された `.exe` を実行してください。

### 4.1 アプリケーション起動

```powershell
# 生成されたバイナリを実行
src-tauri/target/release/fast-comment.exe
```

または、インストーラ経由でインストール後、スタートメニューから起動。

**確認項目:**

- [ ] ウィンドウが開く
- [ ] Tauri 標準タイトルバー・メニューが表示される
- [ ] UI レイアウトが表示される（コメント一覧エリア、設定パネル等）

---

### 4.2 Twitch 接続テスト

**セットアップ:**
1. 設定パネルを開く
2. 「チャンネル追加」から Twitch チャンネル名を入力（例 `twitch_channel_name`）
3. 接続を開始

**確認項目:**

- [ ] コメントが流れてくる（ライブ配信中のチャンネルであれば）
- [ ] コメント表示に遅延がない
- [ ] 著者名、バッジ(mod/sub) が表示される
- [ ] エモートが表示される
- [ ] Bits コメントで金額が表示される

**トラブルシューティング:**
- コメントが来ない → チャンネル名のスペル・ケースを確認
- 切断される → Twitch サーバー側で PING-PONG が正しく機能しているか確認（ログを確認）

---

### 4.3 YouTube 接続テスト

**セットアップ:**
1. 設定パネルを開く
2. 「チャンネル追加」から YouTube ライブ動画 URL または videoId を入力
3. 接続を開始

**確認項目:**

- [ ] コメントが流れてくる（ライブ配信中の動画であれば）
- [ ] 表示遅延がない
- [ ] メンバー、モデレーター バッジが表示される
- [ ] SuperChat（スーパーチャット）が表示される（あれば）
- [ ] 絵文字が表示される

**既知の制約：YouTube パースは寛容設計**

YouTube InnerTube API は非公式 → 仕様が予告なく変更される可能性あり。

- **解析失敗時の挙動:** パースに失敗したアクションは `logs/yt-unparsed.jsonl` に自動ログ
- **リカバリ方法:** 仕様変更検出時は、`config.json` の `youtubeOverrides.paths` セクションで抽出パス・マーカーを変更（再ビルド不要）

例：

```json
{
  "youtubeOverrides": {
    "paths": {
      "actionsPath": "continuationContents>liveChatContinuation>actions",
      "apiKeyMarker": "INNERTUBE_API_KEY"
    }
  }
}
```

詳細は `.claude/SPEC.md` の「YouTube パーサ」セクション参照。

---

### 4.4 OBS WebSocket テンプレ表示

**セットアップ:**
1. 設定パネルで OBS サーバ設定を確認（デフォルト: `127.0.0.1:11180`）
2. Web ブラウザで `http://127.0.0.1:11180/?template=default` を開く
3. テンプレ画面が表示されることを確認

**確認項目:**

- [ ] HTML テンプレが表示される
- [ ] コメントが流れている場合、テンプレ内に表示される
- [ ] フェードアウトアニメーション等の CSS が機能している

**OBS への組み込み：**

OBS Studio で「ブラウザソース」を追加し、以下 URL を指定：

```
http://127.0.0.1:11180/?template=default&ws=ws://127.0.0.1:11180/ws&channel=twitch_channel_name
```

- `template=default` → テンプレ選択（他のテンプレが `templates/` に増える場合は名前を変更）
- `ws` → WebSocket エンドポイント（ポート変更時はここも更新）
- `channel` → 配信情報(任意、テンプレで利用可)

---

### 4.5 TTS 発話テスト（オプション）

**セットアップ:**
1. 設定パネルで TTS バックエンド選択（bouyomi / VOICEVOX / WebSpeech）
2. コメントが来る環境で、TTS が有効か確認

**確認項目:**

- [ ] コメント受信時に音声で読み上げられる
- [ ] 音声設定（速度・音量）が反映される
- [ ] URL 除去・絵文字除去が機能している

**バックエンド別確認:**

| バックエンド | 確認事項 |
|-----------|--------|
| **bouyomi** | 棒読みちゃんが起動してる（TCP 50001 でリッスン） |
| **VOICEVOX** | VOICEVOX Server が起動してる (`http://127.0.0.1:50021`) |
| **WebSpeech** | ブラウザの Web Speech API が機能してる（Chrome 推奨） |

### 4.6 検索 / コマンドパレット（wave9 追加機能）

**検索:**

- [ ] ツールバーの検索ボックスに文字入力 → 投稿者名・本文に部分一致するコメントだけ表示される（大文字小文字無視）
- [ ] 検索中、ツールバーにマッチ件数（`N件`）が表示される
- [ ] プラットフォームフィルタと検索が AND で合成される（例: Twitch + キーワード）
- [ ] 検索クリア・フィルタ切替で意図せず最下部へジャンプしない（オートスクロール誤発火がない）

**コマンドパレット:**

- [ ] `Ctrl`/`Cmd` + `K` でパレットが開く（オーバーレイ中央上）
- [ ] キーワード入力で候補が絞り込まれる（例「twitch」「読み」「obs」）
- [ ] `↑`/`↓` で選択移動、`Enter` で実行、`Esc`／オーバーレイクリックで閉じる
- [ ] 「TTS設定へ」等を選ぶと設定タブへ切替＋該当セクションへスクロールする（コメントタブからのコールド遷移でも tts/obs/moderation に正しくスクロール）
- [ ] 該当コマンドが無いキーワードで「『<入力>』でコメント検索」が出て、選ぶとコメント検索が走る
- [ ] パレットを開いた直後に `Enter` を押しても、カーソル位置の項目が誤実行されない（hover はマウス移動時のみ選択更新）

### 4.7 投げ銭サマリー / ピン留め / 通知（wave10-12 追加機能）

**投げ銭サマリー:**

- [ ] SuperChat/Bits 受信時、ヘッダーに通貨別の合計金額・件数バッジ（💰）が出る
- [ ] メンバーシップ受信時、👑 件数が出る
- [ ] 一覧クリア（✕）で集計がリセットされる／バッファ上限超過後も合計は維持される

**ピン留め:**

- [ ] コメント行ホバーで 📌 ボタンが出て、クリックで上部ストリップに固定表示される
- [ ] ピン済みコメントはスクロールで流れても・バッファ退避後もストリップに残る
- [ ] ストリップの ✕ で解除できる。6件目をピンすると最古が外れる（最大5件）

**キーワード通知:**

- [ ] 設定 → 通知 で「効果音で通知」ON＋音量設定 → 保存
- [ ] ハイライト一致（NG/ハイライト設定のキーワード/ユーザー）コメント到着で効果音が鳴り、画面が一瞬フラッシュする
- [ ] 音量変更が（保存後）即座に反映される。OFF時は鳴らない
- [ ] `Ctrl+K` →「通知」で通知設定セクションへジャンプできる
- [ ] アプリ再起動後も通知設定が保持される（config.json 永続化）

---

## 5. 既知の留意点

### 5.1 WSL ビルド不可の理由

**なぜ Windows で？**

- WSL は Linux カーネルで動作 → `cargo build` は Linux バイナリを生成
- Tauri は Windows ネイティブバイナリ (`.exe`) を要求
- **解決策:** ビルドは Windows 側のみ（WSL は編集用）

---

### 5.2 YouTube パース仕様変更への対応

YouTube InnerTube は変更予告なしで仕様が変わる可能性があります。

**対応フロー:**

1. パース失敗 → `logs/yt-unparsed.jsonl` にログ出力される
2. ログを確認し、新しいパス構造を特定
3. `config.json` の `youtubeOverrides.paths` を更新
4. アプリ再起動（再ビルド不要）

詳細は `.claude/SPEC.md` 「4.2 YouTube」セクション参照。

---

### 5.3 Twitch PING-PONG の重要性

Twitch IRC は定期的に `PING` コマンドを送信します。**必ず `PONG` で応答してください。応答しないと接続が切断されます。**

ソースコード: `src-tauri/src/sources/twitch.rs` の PONG 応答ロジックを確認。

---

### 5.4 IPC バッチ送出（フレーム単位）

UI パフォーマンス最適化のため、コメントは **1フレーム(約16ms)ごとにまとめて送出** します。

- ❌ **NGな例：** 1コメント 1 IPC emit
- ✅ **正しい例：** ~16ms ごとに複数コメントを配列で emit

ソースコード: `src-tauri/src/bus.rs`

---

### 5.5 実モデレーション(BAN/削除)は Phase 6

**MVP の範囲外です。**

- ✅ **MVP (Phase 5 まで):** ローカル非表示、NG ワード、ハイライト
- 🔄 **Phase 6:** 実 BAN(Twitch) / 削除(YouTube) - OAuth 必須

詳細は `.claude/SPEC.md` 「7. モデレーション」セクション参照。

---

## 6. デプロイ・配布

### 6.1 Windows インストーラ

```powershell
# npm run tauri build で自動生成される
src-tauri\target\release\fast-comment_0.1.0_x64_en-US.msi
```

このファイルを配布すれば、ユーザーがダブルクリックでインストール可能。

### 6.2 ポータブル実行ファイル

```powershell
src-tauri\target\release\fast-comment.exe
```

再配置可能な単一ファイル（依存関係は WebView2 Runtime のみ）。

### 6.3 デスクトップショートカット（NSIS インストーラ）

NSIS インストーラ（`*-setup.exe`）はインストール完了時に**自動でデスクトップにショートカットを作成**する。
`src-tauri/nsis-hooks.nsh`（`NSIS_HOOK_POSTINSTALL`）を `tauri.conf.json` の
`bundle.windows.nsis.installerHooks` で配線して実現している。

- [ ] NSIS インストーラ（`*-setup.exe`）でインストール → デスクトップに `fast-comment` ショートカットができる
- [ ] アンインストールでショートカットも消える（標準テンプレートが削除）
- 補足: Tauri 2 には `createDesktopShortcut` 設定キーは無く、フック方式が確実（document-specialist 調査済）。
- 補足: **MSI（WiX）側は未対応**（WiX はカスタム `.wxs` フラグメントが別途必要）。MSI でも必要なら依頼を。

---

## 7. チェックリスト

最終確認用。すべてにチェックを入れてから本番環境へ移行してください。

### ビルドフェーズ

- [ ] Rust toolchain (stable) インストール確認
- [ ] `cargo build` でエラーなし
- [ ] `cargo test --lib` で全テスト PASS
- [ ] `npm install` でエラーなし
- [ ] `npm run check` で型エラーなし
- [ ] `npm run build` で Vite ビルドエラーなし
- [ ] `npm run tauri build` で `.exe` 生成成功
- [ ] `src-tauri/icons/` にアイコンが存在

### スモークテストフェーズ

- [ ] アプリケーション起動 → ウィンドウ表示
- [ ] Twitch 接続テスト → コメント表示・遅延なし
- [ ] YouTube 接続テスト → コメント表示・バッジ・SuperChat 確認
- [ ] OBS テンプレ接続 → `http://127.0.0.1:11180/?template=default` でテンプレ表示
- [ ] TTS 発話テスト（オプション） → 音声で読み上げ

### 本番前確認

- [ ] ロジックエラーがないか（ログ確認）
- [ ] UI/UX が期待通りか
- [ ] パフォーマンス問題がないか（CPU/メモリ）
- [ ] `.msi` インストーラが正常に動作するか（テスト環境で実施推奨）

---

## 8. トラブルシューティング

| 症状 | 原因 | 対応 |
|------|------|------|
| `cargo build` エラー | Rust 未インストール or バージョン古い | `rustup update` を実行 |
| `npm install` 失敗 | Node.js 未インストール or npm キャッシュ破損 | `npm cache clean --force` → 再実行 |
| `svelte-check` エラー | TypeScript 型定義不正 | WSL で型エラーを修正 |
| Tauri ビルド時「アイコンない」 | `src-tauri/icons/` 不足 | アイコンを配置 |
| YouTube コメント来ない | InnerTube 仕様変更 | `logs/yt-unparsed.jsonl` 確認 → `config.json` 更新 |
| Twitch 接続切断 | PONG 応答失敗 | ソースコード確認、ネットワーク疎通確認 |

---

## 9. 参考資料

- **詳細仕様:** `.claude/SPEC.md`
- **プロジェクト制約:** `.claude/CLAUDE.md`
- **セットアップ手順:** `SETUP.md`
- **Rust Cargo ドキュメント:** https://doc.rust-lang.org/cargo/
- **Tauri 2.x ドキュメント:** https://v2.tauri.app/
- **Svelte 5 ドキュメント:** https://svelte.dev/docs
- **YouTube InnerTube 非公式ドキュメント:** https://github.com/yt-dlp/yt-dlp

---

**最終確認日:** 2026-05-30  
**バージョン:** 0.1.0  
**作成:** fast-comment プロジェクト
