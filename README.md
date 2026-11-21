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
- **運営コメント投稿** — Twitch / YouTube Liveへ配信者・運営アカウントとして投稿。YouTubeは公式Data API + PKCE認証を使い、更新トークンをOS資格情報ストアへ保管
- **モデレーション(ローカル)** — NGワード/NGユーザー(正規表現)・ハイライトルール。ローカル非表示/グレー化に加え、YouTube は配信側のコメント削除・タイムアウト・BAN（P6 Phase 1、要Google接続）。Twitch も同様に配信側のコメント削除・タイムアウト・BAN・BAN解除に対応（Device Code ログイン、要 Twitch クライアントID）。公式画面や他モデレーターが行った削除・タイムアウト・BAN もアプリ表示へ反映（P6 Phase 3）。同梱の OBS テンプレ（default / simple / bubble / ticker / donation / danmaku）からも、削除・BAN・ローカル非表示したコメントを取り除きます（`/ws?modEvents=1`）
- **X への配信開始告知** — 「自分の配信」の印を付けたチャンネルの配信開始で X に告知。既定は投稿画面を開くだけ(無料)。自分の X 開発者アプリで接続すれば公式 API で完全自動にもできる(有料)。既定OFF
- **コマンドパレット** — `Ctrl+K` でアクション実行・設定ジャンプ・コメント検索
- **コメントピン留め / キーワード通知音** — 重要コメントの固定表示、一致コメント到着時の効果音通知
- **自動更新** — 起動時に GitHub Releases の署名付き更新(`latest.json`)を確認し、新版があれば「更新して再起動」バナーを表示。設定画面の「アプリの更新」から手動確認も可能

## 技術スタック

| 領域 | 採用 |
|---|---|
| シェル | Tauri 2.x (Rust) |
| UI | Svelte 5 + Vite + TypeScript |
| 非同期 | tokio |
| 接続 | tokio-tungstenite (Twitch IRC-WS) / reqwest (YouTube InnerTube・Data API) |
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

ローカルビルドでは updater 用の署名(`.sig`)を作りません。署名用の秘密鍵は GitHub Secrets にだけあり、環境変数 `TAURI_SIGNING_PRIVATE_KEY` が無い時は `src-tauri\tauri.nosign.conf.json` を重ねてビルドします。

### リリース(GitHub Actions)

`src-tauri/tauri.conf.json`・`src-tauri/Cargo.toml`・`package.json` の `version` を揃え、`.github/release-notes/vX.Y.Z.md` を用意してから `vX.Y.Z` の tag を push します。`.github/workflows/release.yml` が Windows ランナーで NSIS インストーラ(setup.exe)を作り、updater 用の署名と `latest.json` を付けて **draft** の Release を作ります。中身を確かめてから公開してください(`gh release edit vX.Y.Z --draft=false`)。公開するまで既存ユーザーへの自動更新は始まりません。tag とアプリのバージョンが違う時はワークフローが失敗します。

## 実機スモーク確認

Windows でリリースビルド(`build-windows.bat`)を作ったあと、ログインや課金なしで主要経路を自動確認できます。

```bat
:: OBS(health/templates/ws/stats)と CDP 経由のコメント注入・モデレーションを確認
pnpm smoke

:: 既に CDP 付きで起動しているアプリへ接続(更新確認はスキップ)
pnpm smoke --attach --skip-updater
```

`--download-installer` を付けると更新用インストーラーの minisign 署名まで検証します。ポートは `--obs-port` / `--cdp-port` で変更できます。

## OBS テンプレートのカスタマイズ

内蔵テンプレートは読み取り専用です。設定画面のテンプレートエディタで、複製して編集するか、zip を取り込みます。

- **複製**: 「複製して編集」を押し、名前(英小文字・数字・`_`・`-`)を入力して「作成」。複製したテンプレートは `custom:<名前>` として一覧に並び、`index.html` / `style.css` / `app.js` を編集できます。OBS には「URLをコピー」で得た URL を使います。
- **zip の取り込み**: 「zip から取り込み」で zip を選び、名前を確認して「取り込む」。確認ダイアログで了承すると取り込みます。取り込めるのは fast-comment 形式(ルートに `index.html` があり、`/ws` に接続する静的サイト)の zip だけで、わんコメのテンプレ zip はそのままでは動きません。上限は zip 本体 100 MiB・展開後合計 100 MiB・500 ファイル・深さ 4 です。許可外の拡張子のファイルは取り込まずに一覧で知らせます。詳細は `.claude/SPEC.md` §9.1。
- **削除**: カスタムテンプレートを選んで「削除」。内蔵は削除できません。この URL を使っている OBS ソースは表示されなくなります。

カスタムテンプレートはアプリの設定フォルダに保存されるので、アプリを更新しても消えません。一方、複製元の内蔵テンプレートの修正は複製には届きません。

> **取り込むテンプレートは信頼できる配布元の物だけにしてください。** テンプレートの JavaScript は配信画面(`127.0.0.1` のブラウザソース)で実行され、コメントを読めます。

## 更新

v0.2.0 以降はアプリ内で更新できます。起動時に新しい版を静かに確認し(オフラインや失敗時は何も表示しません)、見つかると画面上部に通知が出ます。「更新して再起動」でダウンロードとインストールを行い、アプリが再起動します。「後で」で通知を閉じられ、設定画面の「アプリの更新」からいつでも確認できます。更新中は接続中の配信が一度切れます。

v0.1.0 には自動更新がないため、[Releases](https://github.com/machaniconico/fast-comment/releases) から v0.2.0 のインストーラーを手動で入れ直してください。コード署名をしていないため、Windows の SmartScreen が警告を出したら「詳細情報」→「実行」で起動できます。

開発時のフロントエンド検証のみ WSL でも可能です:

```bash
npm run check   # svelte-check（型/Svelte検証）
npm run build   # vite build
```

## 設定

設定はアプリ内の設定画面から行えます。永続化先はユーザーデータディレクトリ配下の `config.json` です。YouTube の仕様変更には `youtubeOverrides`（API キー / clientVersion / 抽出パス上書き）で再ビルド無しに対応できます。

Twitch の同時接続数をヘッダーに表示するには、dev.twitch.tv に自分用のアプリを登録し、クライアントID / クライアントシークレットを設定画面に入れます。Twitch には視聴者数を返す無認証 API が無いためで、未設定でもチャット受信は動きます（数字が出ないだけ）。手順は [docs/TWITCH_VIEWER_COUNT.md](./docs/TWITCH_VIEWER_COUNT.md) を参照してください（アプリ内の 設定 → 認証情報 → Twitch にも同じ手順を畳んで置いてあります）。

YouTubeへ運営コメントを投稿する場合は、Google CloudでYouTube Data API v3を有効にし、OAuthクライアントを種類「デスクトップアプリ」で作成します。設定画面にクライアントIDを貼り付けて「Googleに接続」を実行してください。クライアントシークレットは不要で、Googleの更新トークンは`config.json`ではなくOSの資格情報ストアへ保存されます。認証・配信探索・投稿はユーザー操作時だけ動作し、コメント受信ループへの常駐処理追加はありません。

Twitchの配信側モデレーション（コメント削除・タイムアウト・BAN・BAN解除）を使う場合は、設定画面にTwitchクライアントIDを入力して保存し、「Twitch モデレーション」の「Twitch にログイン」を押します。表示されたコードをブラウザで入力して許可します（Device Code ログイン）。必要スコープは `moderator:manage:banned_users` と `moderator:manage:chat_messages` です。Twitch開発者コンソールで、既存アプリのDevice Code Grantが使える必要があります。更新トークンはOSの資格情報ストアへ保存されます。操作対象のチャンネルでモデレーター（または配信者）である必要があります。

### ツイキャスの取得経路

設定画面の「取得経路」で選びます。自動(既定)は非公式で取得し、読めない時はログイン済みなら公式 API へ切り替えます。非公式のみはログインを使いません。公式 API のみは要ログインです。変更は「設定を保存」後、そのチャンネルの次回の接続(再起動・チャンネル再追加)から反映されます。公式 API 経由ではギフトは取れません。仕様の詳細は [.claude/SPEC.md](./.claude/SPEC.md) §4.5 を参照してください。

### X への配信開始告知

設定 →「X への配信開始告知」で有効にし、チャンネル一覧で自分の配信の行に「告知」の印を付けます。

- **投稿画面を開く(既定・無料)**: 配信開始を検知すると、タイトル・URL・日時を入れた X の投稿画面をブラウザで開きます。投稿ボタンは自分で押します。
- **X API で自動投稿(任意・有料)**: 準備は次のとおりです。
  1. X Developer Console でアプリを作ります。種類は Native App(公開クライアント)、権限は Read and write です。
  2. 設定画面に出るコールバック URL(既定 `http://127.0.0.1:47821/callback`)を、アプリの Callback URI にそのまま登録します。ポートを変えたら登録も直します。
  3. クレジットを購入します(無料枠はありません)。
  4. OAuth 2.0 Client ID を設定画面に貼り、「X に接続」を押してブラウザで許可します。「@ユーザー名 として接続中」と出れば完了です。「テスト投稿」で確認できます(実際に公開され、課金されます)。
  5. 「投稿の方法」を「X API で自動投稿」にして保存します(接続済みの時だけ選べます)。

  費用の目安は 2026-11 時点で投稿 1 件 $0.015、URL 入りは 1 件 $0.20 です(告知は URL 入り)。正しい金額は Developer Console で確認してください。更新トークンは `config.json` ではなく OS の資格情報ストアへ保存されます。X の許可はアプリ×ユーザーで 1 つなので、同じ Client ID で別の PC やツールからログインすると先の接続は切れます。毎回同じ文面は X に拒否されるので、テンプレートにタイトルか日時を入れてください。

### ローカル API の使い方

外部ツール(OBS のスクリプト、Bot、n8n など)からコメントを読んだり操作したりするための API です。既定はオフです。

1. 設定画面の「ローカル API」で「有効にする」を入れると、トークンが作られます。
2. 「トークンをコピー」で取り出し、ツール側の `Authorization: Bearer <トークン>` に設定します。
3. ベース URL は `http://127.0.0.1:<OBS サーバのポート>/api/v1` です(設定画面の「URL をコピー」)。

```powershell
curl.exe -H "Authorization: Bearer <token>" http://127.0.0.1:<port>/api/v1/status
```

主なエンドポイントは `GET /status`・`GET /comments`・`GET /events`(SSE)・`POST /test-comments`・`GET /tts` と `POST /tts/pause|resume|skip|clear`・`GET /timer` と `POST /timer/start|pause|resume|reset` です。詳細は `.claude/SPEC.md` §15 を参照してください。

### Webhook の設定

投げ銭・メンバー加入・キーワード一致のコメントを、Discord や自作のサーバへ自動で送れます。設定画面の「Webhook」で送り先(最大 10 件)を追加します。

- **形式**: 「Discord」はチャンネルの Webhook URL(`https://discord.com/api/webhooks/…`)へ埋め込みメッセージを送ります。「汎用 JSON」は任意の URL へ JSON を POST し、署名を付けます。汎用 JSON の `http` は `localhost` とプライベート IP のみ使えます。それ以外は `https` が必要です。
- **きっかけ**: 投げ銭・メンバー加入・キーワード一致。キーワードは 1 行 1 語で、大文字小文字は区別しませんが、全角 / 半角・ひらがな / カタカナは区別します。1 つのコメントが複数に当たっても送るのは 1 回です。
- **テスト送信**: 保存後に「テスト送信」で届くか確認できます(無効・停止中でも送れます)。
- **自動停止**: 401/403・404/410・リダイレクト・連続失敗が続くと、その送り先は自動で停止します。画面に理由が出るので、直してから「再開」を押してください。自動では再開しません。
- 配信中に届いたコメントだけが対象で、アプリを閉じると未送信の分は捨てられます。

#### 汎用 JSON の署名の検証

汎用 JSON の送信には、次のヘッダが付きます。署名シークレットは設定画面の「表示」「コピー」で取り出します。

| ヘッダ | 内容 |
|---|---|
| `X-FastComment-Event-Id` | イベント ID(再送でも同じ。重複排除に使えます) |
| `X-FastComment-Timestamp` | UNIX 秒 |
| `X-FastComment-Signature` | `v1=` + HMAC-SHA256 の 16 進小文字 |

署名の対象は `"<Timestamp>.<リクエスト本文のバイト列>"`、鍵は署名シークレット(文字列そのまま)です。タイムスタンプが古い(例 5 分超)リクエストは拒否してください。

`X-FastComment-Timestamp` は各送信試行の時刻です。本文の `sentAtMs` はキュー投入時の UNIX ミリ秒で、再送でも変わりません。重複排除には、同じく再送で変わらない `eventId` を使えます。

Node.js の例です(本文は JSON に直す前の生のバイト列で検証します。不正な入力は例外にせず `false` を返します)。

```js
import { createHmac, timingSafeEqual } from 'node:crypto';

export function verify(secret, headers, rawBody) {
  const ts = String(headers['x-fastcomment-timestamp'] ?? '');
  const sig = String(headers['x-fastcomment-signature'] ?? '');
  if (!/^[0-9]{1,15}$/.test(ts) || !/^v1=[0-9a-f]{64}$/.test(sig)) return false;
  if (Math.abs(Date.now() / 1000 - Number(ts)) > 300) return false;
  const expected = createHmac('sha256', secret).update(`${ts}.`).update(rawBody).digest();
  return timingSafeEqual(Buffer.from(sig.slice(3), 'hex'), expected);
}
```

Python の例です。

```python
import hashlib, hmac, re, time

def verify(secret: str, headers: dict, raw_body: bytes) -> bool:
    ts = headers.get("X-FastComment-Timestamp") or ""
    sig = headers.get("X-FastComment-Signature") or ""
    if not re.fullmatch(r"[0-9]{1,15}", ts) or not re.fullmatch(r"v1=[0-9a-f]{64}", sig):
        return False
    if abs(time.time() - int(ts)) > 300:
        return False
    expected = hmac.new(secret.encode(), ts.encode() + b"." + raw_body, hashlib.sha256).digest()
    return hmac.compare_digest(bytes.fromhex(sig[3:]), expected)
```

#### 注意

- Discord へは `allowed_mentions` を空にして送るため、コメント内の `@everyone` などで通知は飛びません。Markdown も無効化され、`[文字](URL)` の偽装リンクにもなりません。
- Webhook の URL と署名シークレットは OS の資格情報ストアに保存します。`integrations.json`・`config.json`・ログには書かれず、設定のエクスポートにも含まれません。画面には URL の末尾 4 文字だけが出ます。
- ローカル API のトークンを OBS のソース URL やテンプレートに書かないでください。ローカル API と Webhook の設定はブラウザからは呼べません(`Origin` 付きのリクエストは拒否され、Webhook の設定はアプリの画面からだけ行えます)。

## ロードマップ

- [x] P0 足場（設定/モデル/Tauri・Svelte雛形/ビルド導線）
- [x] P1 Twitch接続コア（仮想化一覧 + rAF）
- [x] P2 YouTube（InnerTube + 寛容パーサ + overrides）
- [x] P3 OBS（axum WS + テンプレート）
- [x] P4 TTS（3バックエンド + ルーティング）
- [x] P5 モデレーション + 設定UI仕上げ
- [ ] P6 OAuth実モデレーション(YouTube / Twitch 実装済み) / テンプレ編集UI / niconico等の追加Source

## ライセンス

[MIT](./LICENSE)
