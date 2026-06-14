# fast-comment 詳細仕様 (SPEC)

わんコメ(OneComme)相当の機能を持ち、より軽量・低遅延なマルチプラットフォーム・コメントビューワー。

## 0. 設計原則

1. **重い処理は全部 Rust 側**（接続・解析・正規化・バッファリング・配信）。Web UI は描画に専念する。
2. **高負荷時も滑らか** — バックプレッシャ(bounded channel)・フレーム単位バッチ送出・UI仮想スクロール。
3. **YouTube 仕様変更に強い** — 寛容パース + 外部設定でホットパッチ + 未解析ペイロードのログ保存。
4. **アダプタパターン** — Source(接続元)・TTS(読み上げ先) は trait で抽象化し、増やしやすく壊れにくくする。

## 1. 技術スタック

- **シェル**: Tauri 2.x (Rust)
- **UI**: Svelte 5 + Vite + TypeScript
- **Rust 非同期**: tokio
- **接続**: tokio-tungstenite (Twitch IRC-WS), reqwest (YouTube InnerTube HTTP)
- **OBS配信サーバ**: axum (HTTP + WebSocket) + tower-http (静的テンプレ配信)
- **ビルド/実行ターゲット**: **Windows**（WSLでは編集のみ、ビルドはWindows側）

## 2. 全体アーキテクチャ

```
[Twitch IRC-WS] ┐
                ├─> Source trait ──> 正規化(ChatMessage) ──> Bus(tokio broadcast)
[YouTube InnerTube] ┘                                          │
                                                  ┌────────────┼─────────────┐
                                                  ▼            ▼             ▼
                                          Tauri IPC(UI)   axum WS(OBS)    TTS dispatch
                                          rAFバッチ描画   テンプレ配信     bouyomi/voicevox/webspeech
```

## 3. 統一データモデル (`model.rs`)

```rust
ChatMessage {
  id: String,                 // 内部一意ID (uuid)
  platform: Platform,         // Twitch | YouTube
  channel: String,            // 配信チャンネル識別子
  author: Author,
  fragments: Vec<Fragment>,   // text | emote 混在
  kind: MessageKind,          // Normal | SuperChat | Membership | Bits | System
  amount: Option<Amount>,     // SuperChat/Bits 等の金額
  timestamp_ms: i64,
  raw: Option<serde_json::Value>, // デバッグ用原データ(任意)
}

Author { id, name, display_color: Option<String>, badges: Vec<Badge>, roles: Roles }
Roles  { broadcaster, moderator, member, subscriber, vip }  // bool フラグ
Badge  { kind: String, label: String, image_url: Option<String> }
Fragment = Text(String) | Emote { id, name, url }
Amount { value: f64, currency: String, raw_text: String }
MessageKind = Normal | SuperChat | Membership | Bits | System
Platform = Twitch | YouTube
```

JSON は `serde(rename_all = "camelCase")` で UI/OBS に流す。

## 4. Source 層 (`sources/`)

### trait
```rust
#[async_trait] // または手書き
trait Source {
  async fn run(&self, tx: broadcast::Sender<ChatMessage>, ctrl: CancelToken) -> Result<()>;
}
```
各 Source は自前で再接続(指数バックオフ)を行う。

### 4.1 Twitch (`twitch.rs`)
- 接続: `wss://irc-ws.chat.twitch.tv:443`
- 認証: 読み取り専用は `PASS SCHMOOPIIE` / `NICK justinfan<rand>` の匿名で可。
- 必須 CAP: `CAP REQ :twitch.tv/tags twitch.tv/commands`
- `JOIN #channel`
- `PRIVMSG` をパース: IRCv3 tags から `display-name, color, badges, emotes, bits, id, tmi-sent-ts` を抽出。
- `PING` には `PONG` で応答(必須・切断防止)。
- バッジ: `broadcaster/1, moderator/1, subscriber/N, vip/1` を Roles にマップ。
- Bits: `bits` タグありなら `MessageKind::Bits` + Amount。
- emotes タグ(`id:start-end,...`)から本文を Fragment 分割。

### 4.2 YouTube (`youtube/`) — 仕様変更耐性が最重要
- **innertube.rs**: リクエスト組み立て
  - 手順: ①live配信URL/videoIdから初期HTMLを取得 → `ytInitialData` と INNERTUBE_API_KEY, client version, 初期 continuation を抽出
  - ②`POST https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?key=<API_KEY>` に context+continuation で繰り返しポーリング
  - ③レスポンスの `timeoutMs` に従ってポーリング間隔調整 / 次 continuation 取得
  - **API_KEY・clientVersion・抽出パスは `config.rs` の `youtube_overrides` から読み、再ビルド無しで差し替え可能**にする
  - `youtubeOverrides.paths`(キー名→文字列)で上書き可能な抽出ポイント。未指定/空キーは既定値にフォールバック(空 paths なら現行挙動と完全一致):

    | キー | 対象 | 既定値 |
    |---|---|---|
    | `apiKeyMarker` | API_KEY 抽出マーカー | `"INNERTUBE_API_KEY":"` |
    | `apiKeyMarkerAlt` | 同・代替 | `"innertubeApiKey":"` |
    | `clientVersionMarker` | clientVersion 抽出マーカー | `"INNERTUBE_CONTEXT_CLIENT_VERSION":"` |
    | `clientVersionMarkerAlt` | 同・代替 | `"clientVersion":"` |
    | `initialContinuationMarkers` | 初期 continuation マーカー(改行区切り) | 既定4種 |
    | `actionsPath` | アクション配列パス(`>`区切り) | `continuationContents>liveChatContinuation>actions` |
    | `continuationsPath` | continuation 配列パス(`>`区切り) | `continuationContents>liveChatContinuation>continuations` |
    | `continuationDataKeys` | continuation データキー候補(改行区切り) | `invalidationContinuationData` 他5種 |
- **parser.rs**: 寛容パース(アダプタの核)
  - `serde_json::Value` をパス探索で辿る。固い struct deserialize はしない。
  - ヘルパ `dig(value, &["a","b",0,"c"])` で Option を返す。途中欠落でも None で安全に劣化。
  - 対応アクション: `addChatItemAction` → `liveChatTextMessageRenderer`(通常), `liveChatPaidMessageRenderer`(SuperChat), `liveChatMembershipItemRenderer`(メンバー), `liveChatPaidStickerRenderer`(ステッカー)
  - 著者バッジ(`authorBadges`)から member/moderator/owner を Roles へ。
  - `runs[]` を Fragment(text|emote) に変換(`emoji` は Emote)。
  - **解析できなかったアクションは `logs/yt-unparsed.jsonl` に1行追記**(原因究明用)。
  - パーサにバージョンタグを持たせ、将来の差し替えを容易に。

## 5. Bus 層 (`bus.rs`)

- 内部: `tokio::sync::broadcast`（容量上限あり、lag は drop 容認=最新優先）。
- **UI向け**: Tauri の `app_handle.emit("chat", batch)`。**個別送出せず ~16ms(1フレーム) でまとめて配列送出**(IPC往復削減)。
- **OBS向け**: axum WebSocket `/ws`。接続クライアントへ同じ batch を push。`GET /?template=<name>` は `templates/<name>/index.html` を返す（未指定は `default`、`../` 等は拒否）。静的アセットは `/<name>/...` で配信。ポートは設定(既定 11180)で、変更時は新ポートの bind 成功後にサーバを再起動して再 bind する。
- バックプレッシャ: 各クライアントごとに bounded queue、溢れたら古いものから捨てる。

## 6. TTS 層 (`tts/`) — 3バックエンドをアダプタ化

```rust
trait TtsBackend { async fn speak(&self, text: String) -> Result<()>; fn available(&self) -> bool; }
```
- **bouyomi.rs**: 棒読みちゃん。`TCP 127.0.0.1:50001` にバイナリコマンド送信(コマンド0x0001, 速度/音量/声質/トーン/文字コードUTF-8/本文)。
- **voicevox.rs**: VOICEVOX。`POST /audio_query` → `POST /synthesis` でwav取得 → 再生(UIへ送って再生 or rodio)。既定 `http://127.0.0.1:50021`。
- **webspeech**: 実再生はUI側(`speechSynthesis`)。Rustは「読み上げ対象テキスト」イベントをUIへ送るだけ。
- ルーティング: 設定で優先バックエンド選択。優先が `available()==false` なら Web Speech へフォールバック。
- 読み上げ整形: 名前/本文の読み方、URL省略、絵文字除去、長文カット等(設定可)。

## 7. モデレーション (`moderation.rs`) — MVPは認証不要範囲

- **NGワード/NGユーザー**(正規表現可) → マッチは UI/OBS に流さない or グレー表示。
- **ハイライトルール**(ユーザー/キーワード)→ flag 付与。
- **ローカル非表示**(手動): UIで個別コメントを隠す。
- ⚠️ 実BAN/タイムアウト(Twitch)・コメ削除(YouTube)は各OAuth必須 → **フェーズ2**。MVPではローカル処理のみ。

## 8. UI (`src/`)

- **メインウィンドウ**: 統合コメント一覧
  - **仮想スクロール**(可視範囲のみDOM化) + **リングバッファ**(保持上限、例 既定2000件)
  - **rAFバッチ**: IPCで届いた batch を次フレームでまとめて反映
  - プラットフォーム別フィルタ、検索、SuperChat/Bits/ハイライト強調、個別非表示
  - **検索**: 投稿者名+本文(emote名含む)を結合した文字列への**大文字小文字無視・部分一致**。事前計算した小文字haystackをバッファエントリに保持。検索中はツールバーにマッチ件数を表示。プラットフォームフィルタとANDで合成。
  - **オートスクロール**: 最下部にいるときのみ新着で追従。新着判定は単調増加の受信総数(`receivedCount`)で行い、フィルタ/検索の可視件数変化やバッファ飽和に影響されない。
  - **コマンドパレット(`Ctrl`/`Cmd`+`K`)**: 機能をキーワードで呼び出す。アクション(フィルタ切替/一覧クリア/タブ切替)・設定セクションへのジャンプ(チャンネル/TTS/OBS/モデレーション/通知)・「『<入力>』でコメント検索」フォールバック。Arrow/Enter/Esc操作・部分一致(日本語+ローマ字キーワード)。UI view状態は `ui.svelte.ts`(singleton)に集約。
  - **投げ銭サマリー**: SuperChat/Bits を通貨別に合計金額・件数、メンバーシップ件数をヘッダーに集計表示。store側でセッション累積(バッファ退避後も保持、一覧クリアでリセット)。bits は通貨キー `bits` に正規化。
  - **ピン留め**: 重要コメントを上部固定ストリップに表示。ピンは ChatMessage 実体を別保持しバッファ退避後も残る。最大5件FIFO。各行にピン/解除ボタン。
  - **キーワード通知**: ハイライト一致コメント到着で効果音(Web Audio)+控えめな画面フラッシュ。ON/OFF・音量は `config.ui.notifySound`/`notifyVolume` に永続化。store の単調 `highlightCount` を `Notifier.svelte` が監視(初回非発火)。
- **設定画面**: チャンネル追加(Twitch名 / YouTube videoId or URL)、テンプレ選択、TTS設定、NG/ハイライト編集、OBSサーバURLコピー、通知(効果音ON/OFF+音量)。各セクションはコマンドパレットからアンカースクロールで到達可能(`id="settings-*"`)。
- **テンプレ編集** (`TemplateEditor.svelte`, 実装済み): 設定画面からテンプレ選択 + CSS編集。ライブプレビュー強化は将来。
- IPC: `listen("chat", ...)` でバッチ受信。型は `src/lib/types.ts`(Rust model のミラー)。受信は ipc.ts の rAF バッチ flush(最小化/オクルージョンで rAF が止まっても 250ms タイムアウトで必ず flush)。

### 8.1 拡張UI機能(実装済み)

MVP(§8)に加えて以下が出荷済み。いずれも `config.ui` 等で ON/OFF・パラメータ調整可、未使用時は既定 OFF のものが多い。

- **弾幕オーバーレイ** (`DanmakuOverlay.svelte`, `danmaku.ts`): ニコ生風に画面を横切る流れるコメント。
  - **デスクトップ窓**: 専用の透過・クリックスルー(`set_ignore_cursor_events`)・最前面(`set_always_on_top`)ウィンドウ。capability は `src-tauri/capabilities/danmaku.json`(`windows: ["danmaku"]`)。ゲーム配信画面の上に直接重ねる用途。
  - **OBSテンプレ**: `templates/danmaku/`(§9)。OBSブラウザソース用。
  - レーン(行)割当で重なりを回避。文字幅は canvas 計測、不可時はCJK/半角の概算でフォールバック。`system` 種別は流さない。設定は localStorage(`fc.danmaku`)に永続化し、Tauri イベントで即時反映。
- **コメント投稿** (`CommentComposer.svelte`, `sources/twitch_send.rs`): 自分でコメントを送信。Twitch は IRC で送信(実機ビルド検証済みは要確認)。**YouTube 投稿は未実装(スタブ)** — UI 上は選択不可/注意表示にする。
- **参加型配信の管理** (`Participation.svelte`, `Raffle.svelte`): キーワード(既定「参加」)での参加登録、先着/ランダム抽選、専用タブ。既定 OFF。
- **投げ銭パネル** (`DonationPanel.svelte`): SuperChat/Bits/メンバーを通常コメントと分けて表示(アプリ内タブ / OBS `?only=gift`)。既定 OFF。
- **配信振り返りダッシュボード** (`Dashboard.svelte`, `Sparkline.svelte`): コメント数・視聴者推移などの集計表示。
- **タイマー/ゴール/エフェクト/マイルストーン** (`Timer.svelte`, `GoalsBar.svelte`, `Effects.svelte`, `Milestone.svelte`): 配信演出系。OBS テンプレ `timer`/`goals` と連動。
- **マルチカラム表示** (`MultiColumnView.svelte`): チャンネル/種別ごとの複数列ビュー。
- **設定/モデレーションのポータビリティ** (`ConfigPortability.svelte`, `ModerationPortability.svelte`): 設定・NG/ハイライトのエクスポート/インポート。
- **ウィンドウ最前面ピン**: メインウィンドウを最前面固定するトグル(`core:window:allow-set-always-on-top`)。

## 9. OBS テンプレ (`templates/`)

- 各テンプレは index.html + style.css + app.js の独立した静的サイト。`/ws` に接続し、届いた batch を描画する。ユーザーが CSS を差し替えて見た目をカスタムできる。
- OBS には「ブラウザソース」で `http://127.0.0.1:11180/?template=<name>&ws=ws://127.0.0.1:11180/ws&channel=...` を指定。`template` 未指定時は `default`。OBS ポート変更時は `ws` も同じポートを指す。
- 出荷済みテンプレ(9種):

  | テンプレ | 用途 |
  |---|---|
  | `default` | 下から積む標準コメント表示。フェードアウト等はCSS |
  | `simple` | 最小装飾の軽量版 |
  | `bubble` | 吹き出し風 |
  | `ticker` | 1件ずつ横帯で巡回表示 |
  | `ranking` | 発言数ランキング集計表示 |
  | `goals` | 目標(ゴール)バー |
  | `donation` | 投げ銭(SuperChat/Bits)サマリー |
  | `timer` | カウントダウン/アップタイマー |
  | `danmaku` | ニコ生風に画面を横切る弾幕(後述 §8.1) |

- 共通クエリパラメータ: `ws`(接続先), `channel`(フィルタ), `only=gift`(投げ銭のみ) 等。テンプレ間で `max` の意味が異なる点に注意 — 通常overlayでは「表示行数の上限」、`danmaku` では「同時アニメDOM数の上限(MAX_ACTIVE)」。弾幕URL生成(`Settings.svelte` の `withDanmaku()`)は弾幕に無関係なパラメータ(`max`/`ttl`/`font`/`bg`/`pos`/`icon`)を allowlist で除外する。
- システム種別(`kind==='system'` 接続通知等)は配信画面に流さない(全テンプレで除外)。

## 10. 設定永続化 (`config.rs`)

- 保存先: Tauri の app config dir に `config.json`。
- 内容: channels[], obs{port}, tts{backend, options}, moderation{ngWords[], ngUsers[], highlights[]}, ui{maxBuffer, notifySound, notifyVolume}, youtubeOverrides{apiKey?, clientVersion?, paths?}。
  - `ui.notifySound`(bool 既定false) / `ui.notifyVolume`(f32 0.0〜1.0 既定0.5): キーワード通知の効果音設定。serde default でキー欠落の旧 config も後方互換。
  - `obs`: ポートに加え `maxRows` / `fontScalePct` / `ttlMs` / `bgOpacityPct` / `position` 等の見た目設定。範囲外値が入らないよう **Rust 側でも `normalize()` で clamp**(`maxRows` 1..=1000, `fontScalePct` 50..=200, `bgOpacityPct` 0..=100, `ttlMs` 500..=600000, `position`∈{top,bottom})。`normalize()` は config ロード時と `update_config` 適用時の両方で呼ぶ。上限定数は Rust 側に1箇所(`MAX_OBS_ROWS`)を置き UI/テンプレと値を揃える。
- 起動時ロード、変更時保存。`config.json` をアプリ設定の正本とする。
- **localStorage 併用**: 一部のUI/ウィンドウ局所設定は各ウィンドウの `localStorage` に保持する(`config.json` には載せない)。現状: 弾幕表示設定(`fc.danmaku`)、テーマ、最前面ピン状態、コマンドパレット履歴 等。これらはウィンドウローカルかつ低リスクなため意図的に localStorage 側に置いている。

## 11. フェーズ計画

- **P0 足場**: 設定/モデル/Tauri雛形/Svelte雛形/ビルド導線(SETUP)
- **P1 接続コア**: Twitch source(実動) + Bus(IPC) + UI一覧(仮想化+rAF) → "Twitchが映る"
- **P2 YouTube**: InnerTube + 寛容パーサ + overrides → "YTが映る"
- **P3 OBS**: axum WS + default テンプレ
- **P4 TTS**: 3バックエンド + ルーティング
- **P5 モデレーション + 設定UI仕上げ**
- **P5+ 拡張UI(実装済み, §8.1)**: 弾幕オーバーレイ(デスクトップ窓 + danmaku テンプレ)、コメント投稿(Twitch)、参加型/抽選、投げ銭パネル、ダッシュボード、タイマー/ゴール/エフェクト、マルチカラム、設定ポータビリティ、テンプレ編集UI、最前面ピン、追加OBSテンプレ8種
- **P6(後)**: OAuth実モデレーション(実BAN/削除)、YouTubeコメント投稿、niconico等の追加Source、テンプレ編集のライブプレビュー強化

## 12. 既知の制約・注意

- WSLでは Tauri ビルド不可(Linuxバイナリになる)。ビルドはWindows側。
- YouTube InnerTube は非公式 → 仕様変更リスク。寛容パース+overrides+ログで吸収。
- 実モデレーションはOAuth必須(P6)。
- Tauri build にはアイコン(`src-tauri/icons/`)が必要。
