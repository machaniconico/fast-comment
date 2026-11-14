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
- **接続**: tokio-tungstenite (Twitch IRC-WS), tonic (YouTube公式streamList gRPC), reqwest (YouTube InnerTube HTTPフォールバック)
- **OBS配信サーバ**: axum (HTTP + WebSocket) + tower-http (静的テンプレ配信)
- **ビルド/実行ターゲット**: **Windows**（WSLでは編集のみ、ビルドはWindows側）

## 2. 全体アーキテクチャ

```
[Twitch IRC-WS] ┐
                ├─> Source trait ──> 正規化(ChatMessage) ──> Bus(tokio broadcast)
[YouTube streamList / InnerTube] ┘                             │
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

### 3.1 配信開始/終了イベント (`LiveEvent`、X 自動告知の入力)

`ChatMessage` とは別の side channel(`mpsc`、容量 64)。UI/OBS へは流さない(JSON 化しない)。

```rust
LiveStream { platform, channel_identifier /*設定の identifier そのまま*/, stream_id, title: Option<String>,
             url, started_at_ms: Option<u64> }
LiveEvent  = Started(LiveStream) | Ended { platform, channel_identifier }
```

- 送出は `try_send` のみ。満杯・切断なら warn を出して破棄し、Source は止まらない(`sources::send_live_event`)。`SourceManager::with_live_tx` で渡す。
- `stream_id`(配信 1 回を表す ID)を確定できない時は `Started` を送らない(重複防止ができないため)。
- 配信 ID / 開始時刻 / URL:
  - YouTube(チャンネル指定行のみ): videoId / `microformat>playerMicroformatRenderer>liveBroadcastDetails>startTimestamp`(推測。`youtubeOverrides.paths.liveResolveStartTimestampPaths` で改行区切り上書き)/ `https://www.youtube.com/watch?v={id}`。対象 videoId と `isLive` / `isLiveNow` が同じ動画オブジェクト(`videoDetails` / 同じ player response の `liveBroadcastDetails`)に属すると確認できない場合(待機枠、別動画だけが `isLive`、HTML 近傍文字列のみ)は送らない(チャット接続の判定とは別)。新しい videoId を接続した時に `Started`、未配信確定時に `Ended`(初回確認を含む)。
  - Twitch: Helix `streams[0].id` / `started_at` / `https://www.twitch.tv/{login}`。配信 ID が変わった時だけ `Started`、未配信の確認(初回を含む。未配信が続く間は再送しない)で `Ended`。検知は `twitchOauth` / `twitchClientId`+`twitchClientSecret` に加え、`twitchClientId` だけでも Device Code ログイン(`twitch_auth.rs`)のトークンで動く(未ログインの間は何もしない)。
  - ニコ生: embedded-data の `/program/nicoliveProgramId`(取れず設定値が `lv数字` ならそれ)/ `/program/beginTime`(epoch 秒または ms。推測)/ `https://live.nicovideo.jp/watch/{lv}`。ポインタは `niconicoOverrides.endpoints` の `programIdPointer` / `beginTimePointer` で上書き可。watch セッション開始で `Started`。`Ended` は番組が未配信と確認できた時だけ(視聴ページの `/program/status` が `ON_AIR` 以外、または watch WS の disconnect 理由が `END_PROGRAM`)。HTTP タイムアウト・解析失敗・WS エラー・正常 Close など配信状態を確認できない切断では送らない(再接続で `Started` が再送されるため、受け側は配信 ID で重複を弾く)。
- 受信側(`lib.rs`)は現状デバッグログに出すだけ。投稿処理は後続 PR。

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
- **official_stream.rs**: `credentials.youtubeApiKey` が設定済みなら、YouTube Data API v3の
  `liveChatMessages.streamList` (gRPC server streaming) を最優先する。`videos.list` で
  `activeLiveChatId` を解決し、`nextPageToken` を引き継いで長時間接続する。
  - Jewelsによる`giftEvent`はメンバーシップギフトと分け、`gift_name`を種類名として静的にコメント欄へ表示する（アニメーション再現は対象外）。種類名が欠落した場合は代替説明文、通知本文の順で劣化する。
  - APIキー未設定、認証・quota・接続エラー、継続トークンなし終了時はInnerTubeへ自動フォールバック。
  - 公式→InnerTube切替時は直近8192件のYouTubeメッセージIDで重複表示を抑止する。
  - 公開 `streamList` には匿名リアクションが無いため、公式コメント受信中も reaction-only InnerTube sidecar を並行し、公式終了時は停止して通常フォールバックとの二重pollを防ぐ。
  - APIキー変更保存時は接続中のYouTube Sourceを再起動し、新しい受信方式を即時反映する。
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
    | `reactionMutationsPath` | リアクション mutation 配列パス(`>`区切り) | `frameworkUpdates>entityBatchUpdate>mutations` |
    | `reactionBucketPath` | mutation 内の bucket 配列パス(`>`区切り) | `payload>emojiFountainDataEntity>reactionBuckets` |
    | `moderationDeleteActionKeys` | 1 コメント削除アクション名(改行区切り、値の `targetItemId`) | `markChatItemAsDeletedAction` / `removeChatItemAction` |
    | `moderationAuthorActionKeys` | 著者単位削除アクション名(改行区切り、値の `externalChannelId`) | `markChatItemsByAuthorAsDeletedAction` / `removeChatItemByAuthorAction` |
- **parser.rs**: 寛容パース(アダプタの核)
  - `serde_json::Value` をパス探索で辿る。固い struct deserialize はしない。
  - ヘルパ `dig(value, &["a","b",0,"c"])` で Option を返す。途中欠落でも None で安全に劣化。
  - 対応アクション: `addChatItemAction` → `liveChatTextMessageRenderer`(通常), `liveChatPaidMessageRenderer`(SuperChat), `liveChatMembershipItemRenderer`(メンバー), `liveChatPaidStickerRenderer`(ステッカー)
  - Jewelsギフトは専用renderer名・`giftDetails`/`jewelsAmount`・汎用通知本文を寛容に判定し、メンバーシップギフトを誤分類しない。
  - 著者バッジ(`authorBadges`)から member/moderator/owner を Roles へ。
  - `runs[]` を Fragment(text|emote) に変換(`emoji` は Emote)。
  - **解析できなかったアクションは `logs/yt-unparsed.jsonl` に1行追記**(原因究明用)。
  - パーサにバージョンタグを持たせ、将来の差し替えを容易に。

### 4.3 X (Twitter) ライブ配信 (`x.rs`)
- ログイン不要で公開ブロードキャストのライブチャットを受信する。読み取り専用で、コメント投稿は非対応。
- **本文の取得経路は `GET https://api.x.com/live-chat?broadcastId={id}`(認証ヘッダ不要)の NDJSON ストリーム一択**(2026-08 実測)。X Web UI 自体がこのエンドポイントでチャット欄を描画している。旧 Periscope chatapi(chatnow WS / history / accessChatPublic / chatToken)には本文が流れないため一切使わない。
- 手順: ①`POST https://api.x.com/1.1/guest/activate.json`(X Web の公開 Bearer)→ guest_token ②`GET https://x.com/i/api/1.1/broadcasts/show.json?ids={broadcastId}` → state / twitter_user_id / 配信者名 / タイトル / total_watching(視聴者数) ③live-chat ストリームへ接続し行単位で受信 ④show.json を30秒間隔で再取得し viewers 更新と配信終了(state ENDED/TIMED_OUT)検出
- NDJSON 行: `{"userId":"...","chatType":1,"message":"...","ts":"<ナノ秒>","isBackfill":true}`。chatType 1 のみ本文(39 はモデレーション系メタで無視、種別はカウンタに残す)。接続直後は配信開始以降の過去チャットが isBackfill 付きで全量バックフィルされる → 直近20件だけ `skip_tts` を立てて emit し、超過分は捨てる(TTS 暴発防止)。再接続時の再送は `ts+userId` の dedup で吸収。
- 行に username は含まれず、ゲストで解決する API も無い(実測: users/lookup=404 / GraphQL=403,404 / intent=SPA シェル)。配信者(`twitter_user_id` 一致。`user_id` は Periscope ID なので使わない)は show.json の名前で表示し、他は既定で「ユーザー<ID下4桁>」。NG 等の同定は author.id=userId で機能する。
- **ユーザー名解決(オプション)**: `credentials.xAuthToken`(auth_token cookie)と `credentials.xCsrfToken`(ct0)の両方が設定されている場合、受信ループとは別の `name_resolver_task` が GraphQL `liveAtomsUserQuery` をバッチ(最大50 ID/回)で叩き、userId→表示名を解決して差し替える(mpsc FIFO 経由なので表示順は不変)。cookie 失効(401/403)は解決だけを止め匿名表示へフォールバック、チャット受信は継続。cookie 変更時は `update_config` が X チャンネルを自動で張り直す。queryId 込みの URL は `xOverrides.endpoints.userQueryUrl` で差し替え可。
- YouTube と同じく固い struct deserialize はせず `serde_json::Value` のパス探索で寛容にパースし、欠落は None/既定値へ劣化。
- `identifier` は broadcast URL(`https://x.com/i/broadcasts/{id}` / twitter.com 同形)または生の broadcast ID。`extract_broadcast_id()` で正規化し、`ChatMessage.channel` には正規化済み ID を入れる。
- **Bearer とエンドポイント URL は `config.rs` の `xOverrides` から上書き可能**(再ビルド不要)。`xOverrides.bearerToken` と `xOverrides.endpoints`(キー: `guestActivateUrl` / `broadcastShowUrl` / `liveChatUrl` / `userQueryUrl` / `userByScreenNameUrl` / `avatarContentUrl`)。未指定/空は既定値。
- **アカウントモード(自動検知、`sources/x_live.rs` + `x.rs` の `run_account`)**: `identifier` が `@handle` / `https://x.com/{handle}` / `twitter.com/{handle}`(末尾 `/` `?query` `/live` 可。`i` `home` `search` 等の予約パスは除外、handle は `[A-Za-z0-9_]{1,15}`)ならアカウントモード。UI の自動判別は x.com/twitter.com の URL、またはプラットフォームで X を選んだ時の `@handle`(裸の `@handle` は YouTube 優先)。サーバ側の `handle` 単体(`@`・URL 無し)は従来どおり配信 ID 扱い。**ログイン cookie(`xAuthToken`+`xCsrfToken`)が必須**: 未設定なら接続せず、チップに「X の自動検知にはログイン cookie(認証情報)が必要です」を出して cancel まで待機する。手順: ①GraphQL `UserByScreenName`(endpoints `userByScreenNameUrl`)で handle → rest_id(ソース寿命中キャッシュ、失敗は待機して再試行)②`GET fleets/v1/avatar_content?user_ids={uid}&only_spaces=false`(`avatarContentUrl`)の `users[uid].spaces.live_content.livevideo` が RUNNING なら ID 候補(`audiospace` は無視)③既存 show.json で RUNNING かつ `twitter_user_id`==uid を確認して接続④終了(ENDED 等)で検知へ戻り、次は別 ID も受け入れる。結果は Live / NotLive / Unknown の3値で、Unknown(通信・解釈失敗)を「配信なし」とは扱わない。間隔は既定 30 秒+0〜3 秒ジッター(`xOverrides.livePollSecs`、下限 15)。レート制限は `x-rate-limit-remaining/reset` ヘッダ(remaining==0 または 429)で reset(+ジッター)まで待ち、ヘッダ無し 429 は指数バックオフ(上限 15 分)。401/403 は検知を止めて 5 分待機し、チップに「X のログイン cookie が無効です」を出す(cookie 変更時は `update_config` が張り直す)。`ChatMessage.channel` と状態表示キーは配信をまたいで `@{handle 小文字}`(`x_channel_name()`)。dedup・guest_token は配信をまたいで保持。cookie 値はログに出さない。
- 金額系イベント(投げ銭等)の概念が無いため `MessageKind::Normal` のみ。Roles は broadcaster だけ判定。
- 再接続は他 Source と同じ指数バックオフ。HTTP フロー失敗(配信未開始/終了)も不安定扱いでバックオフを伸ばし続け、配信開始待ちポーリングを兼ねる。ストリーム無受信90秒は half-open とみなして張り直す。

### 4.4 ニコニコ生放送 (`niconico.rs`)
- ログイン不要の視聴ページ経由で NDGR メッセージサーバー(2024年8月以降の新コメントサーバー)へ接続する。読み取り専用で、コメント投稿は非対応。
- 手順: ①`GET https://live.nicovideo.jp/watch/{lvId}` → HTML 内 `<script id="embedded-data" data-props="...">` の JSON から `site.relive.webSocketUrl` を抽出(`program.status != ON_AIR` は即エラー=配信待ちポーリング) ②watch WebSocket へ接続し `startWatching` 送信(映像座席は要求しない) ③`seat` の keepIntervalSec 間隔で `keepSeat` 送信、アプリレベル `ping` には `pong`+`keepSeat` 応答 ④`messageServer` の viewUri(NDGR view API) を `?at=now` → `ReadyForNext.at` で追いかけ、`MessageSegment.uri` の ChunkedMessage ストリームからコメント受信
- NDGR は **length-delimited Protobuf over HTTP chunked**。定義は n-air-app/nicolive-comment-protobuf のうち必要フィールドのみを prost derive で手書き(`niconico.rs` 内 `ndgr` モジュール)。未知フィールド/oneof variant は prost が自動スキップ = 寛容パース方針。拾うのは chat(1)/gift(8)/overflowed_chat(20) のみ。
- 匿名(184)コメントは `hashed_user_id` を author.id に、先頭7文字を表示名に。生ID コメントは `raw_user_id`。コテハン(`name`)があれば優先。broadcaster 判定は embedded-data の `program.supplier.programProviderId` と raw_user_id の一致。
- ギフトは `MessageKind::Gift` + `Amount{value: point, currency: "pt"}`。
- watch WS の `statistics` から視聴者数を取得し、チップへ live 状態と共に反映(YoutubeMetadataUpdate 経由)。
- `identifier` は番組 URL(`live.nicovideo.jp/watch/lvN` / `nico.ms/lvN`)または生の lv 番組 ID。`extract_live_id()` で正規化し、`ChatMessage.channel` には正規化済み lv ID を入れる。
- **視聴ページ base URL は `config.rs` の `niconicoOverrides.endpoints`(キー: `watchPageBaseUrl`)から上書き可能**(再ビルド不要)。
- 再接続は他 Source と同じ指数バックオフ。NDGR 購読が死んだらセッション全体を張り直す。サーバー `disconnect`/`error` メッセージも同様。

## 5. Bus 層 (`bus.rs`)

- 内部: `tokio::sync::broadcast`（容量上限あり、lag は drop 容認=最新優先）。
- **UI向け**: Tauri の `app_handle.emit("chat", batch)`。**個別送出せず ~16ms(1フレーム) でまとめて配列送出**(IPC往復削減)。
- **OBS向け**: axum WebSocket `/ws`。接続クライアントへ同じ batch を push。`GET /?template=<name>` は `templates/<name>/index.html` を返す（未指定は `default`、`../` 等は拒否）。静的アセットは `/<name>/...` で配信。ポートは設定(既定 11180)で、変更時は新ポートの bind 成功後にサーバを再起動して再 bind する。
- バックプレッシャ: 各クライアントごとに bounded queue、溢れたら古いものから捨てる。
- **モデレーションイベント**(P6 Phase 3): Source → `mpsc`(容量 1024、送信は `try_send` で chat 受信をブロックしない)→ `spawn_moderation_ui_forwarder` が 16ms バッチで `emit("chat-moderation", Vec<ModerationEvent>)`。同じ forwarder で `MessageDeleted` を Moderator の hidden_ids(上限 8192 の FIFO)へ入れ、未読み上げの該当コメントを TTS キューから除く。同じ forwarder から OBS(`/ws?modEvents=1`)へも流す(§9)。

## 6. TTS 層 (`tts/`) — 3バックエンドをアダプタ化

```rust
trait TtsBackend { async fn speak(&self, text: String) -> Result<()>; fn available(&self) -> bool; }
```
- **bouyomi.rs**: 棒読みちゃん。`TCP 127.0.0.1:50001` にバイナリコマンド送信(コマンド0x0001, 速度/音量/声質/トーン/文字コードUTF-8/本文)。
- **voicevox.rs**: VOICEVOX。`POST /audio_query` → `POST /synthesis` でwav取得 → 再生(UIへ送って再生 or rodio)。既定 `http://127.0.0.1:50021`。
- **webspeech**: 実再生はUI側(`speechSynthesis`)。Rustは「読み上げ対象テキスト」イベントをUIへ送るだけ。
- ルーティング: 設定で優先バックエンド選択。優先が `available()==false` なら Web Speech へフォールバック。
- 読み上げ整形: 名前/本文の読み方、URL省略、絵文字除去、長文カット等(設定可)。

## 7. モデレーション (`moderation.rs`) — ローカル処理 + 配信側(P6 Phase 1)

- **NGワード/NGユーザー**(正規表現可) → マッチは UI/OBS に流さない or グレー表示。
- **ハイライトルール**(ユーザー/キーワード)→ flag 付与。
- **ローカル非表示**(手動): UIで個別コメントを隠す。
- **配信側モデレーション(P6 Phase 1・YouTube)**: コメント右クリック / ユーザーカードから、コメント削除(`LCC.` 形式 ID = API キー設定時の公式 stream 受信分のみ)・タイムアウト(1分/10分/1時間/24時間)・永久 BAN(確認ダイアログ)・BAN 解除(同セッションで行った BAN のみ)。既存の Google OAuth(`youtube.force-ssl`)を使う。実行後は該当コメントを減光 +「削除済み」「タイムアウト」「BAN済み」ラベルで残す(UI のみ)。X / niconico は対象外。
- **配信側モデレーション(P6 Phase 2・Twitch)**: 同じメニュー / ユーザーカードで、コメント削除(IRC `id` タグ = `nativeId` が true のメッセージのみ)・タイムアウト・永久 BAN・BAN 解除(**Twitch では常に表示**)。Twitch Helix を使い、認証は Device Code Flow(設定 >「Twitch モデレーション」の [Twitch にログイン] でコード表示、ブラウザは Rust 側で開く)。必要スコープは `moderator:manage:banned_users` `moderator:manage:chat_messages`。前提: Twitch 開発者コンソールの既存アプリで Device Code Grant が使えること(`credentials.twitchClientId` 必須)。スコープ不足時は設定に「再ログイン」警告。エラーは Rust の `ModerationError` をそのままトースト表示。
- **配信側操作の反映(P6 Phase 3)**: 公式 UI・他モデレーターの操作を `ModerationEvent`(`messageDeleted` / `userCleared{durationSecs}` / `chatCleared`、serde tag=`type`)としてアプリへ反映。
  - 共通フィールド: userCleared の `certain`(false = タイムアウト/BAN の別が不明。UI は既存のタイムアウト・BAN 印を上書きせず、印が無ければ「BAN済み」)、userCleared / chatCleared の `beforeMs`(サーバー時刻の適用境界。`timestampMs <= beforeMs` のコメントだけが対象、null = 境界なし。TTS 除外も同じ判定)。
  - Twitch(匿名 IRC のまま): `CLEARMSG` の `target-msg-id` → messageDeleted。`CLEARCHAT` + `target-user-id` → userCleared(`ban-duration` 秒、無ければ永久 BAN=null、非数値/0 は 0 秒=期間不明のタイムアウト、`certain`=true)。対象タグ無しかつ trailing も無し → chatCleared。`target-user-id` 欠落で login だけの形は無視。`beforeMs` は `tmi-sent-ts`(PRIVMSG の `timestampMs` と同じサーバー時刻)。
  - YouTube InnerTube: `parser::parse_moderation_action`。削除系の `targetItemId` → messageDeleted、著者系の `externalChannelId` → userCleared(null、`certain`=false、`beforeMs`=null。InnerTube はタイムアウトと BAN を区別せず時刻も無い)。`replaceChatItemAction` は対象外。アクション名は §4.2 の override キーで差し替え可。
  - YouTube 公式 streamList: `USER_BANNED_EVENT`(type 10、`user_banned_details`=22、`banned_user_details.channel_id`=101、`ban_type` PERMANENT=1/TEMPORARY=2、`ban_duration_seconds`=4。Google 公開 `stream_list.proto` 準拠)→ userCleared(`beforeMs` = `publishedAt`、banType 不明時は `certain`=false)。公開 proto に削除イベントが無いため、公式経路の削除反映は未対応。
  - 公式 `LCC.` ID と InnerTube ID の突き合わせ(経路またぎ)は M0 未実施のため行わない(取りこぼし許容)。
  - UI: `ipc.ts` の rAF flush で chat バッチ適用**後に** moderation バッチを適用(`moderationEvents.ts` の純関数、1 flush で各リスト 1 回走査)。messageDeleted / chatCleared → 「削除済み」、userCleared → 「タイムアウト」/「BAN済み」(Phase 1 と同じ減光 + ラベル)。初見パネル・投げ銭パネル(バッファ由来)にも反映、ピンは外す。バッファに無い ID はその場では無視するが、直近のイベント(最大 512 件、記録から 10 秒で失効)を覚えておき、後続 flush で届いたコメントにも適用する(chat と moderation は別 IPC バッチのため、イベントが先着しうる)。冪等(アプリ発操作のエコーで壊れない)、「削除済み」は既存の BAN/タイムアウト印を上書きしない。

## 8. UI (`src/`)

- **メインウィンドウ**: 統合コメント一覧
  - **仮想スクロール**(可視範囲のみDOM化) + **リングバッファ**(保持上限、例 既定2000件)
  - **rAFバッチ**: IPCで届いた batch を次フレームでまとめて反映
  - プラットフォーム別フィルタ、検索、SuperChat/Bits/ハイライト強調、個別非表示
  - **検索**: 投稿者名+本文(emote名含む)を結合した文字列への**大文字小文字無視・部分一致**。事前計算した小文字haystackをバッファエントリに保持。検索中はツールバーにマッチ件数を表示。プラットフォームフィルタとANDで合成。
  - **条件フィルタ**(`displayFilter.ts` の純関数、検索欄横の「フィルタ」ポップオーバー): 来訪(全て/初見のみ=`listener.isFirstEver`/再訪のみ)・種別(`MessageKind` 複数)・ロール(複数、いずれか一致)。グループ間 AND・グループ内 OR、空配列=全件。`listener` 無し(system・テスト注入)は来訪が「全て」の時だけ表示。一覧・カラム表示の両方に適用し、既定値の時は従来の fast path を通る。状態は localStorage(読込時に検証、不正値は既定)。
  - **初見タブ**(`FirstCommentPanel.svelte`, `ui.showFirstCommentPanel`、既定 OFF): セッション中の初見コメント直近 50 件を新しい順に保持(リングバッファとは別リスト、一覧クリアで消去)。
  - **ショートカット**: `Ctrl`/`Cmd`+`F` で検索欄にフォーカス(開いているパレット等は閉じる)。`Ctrl`/`Cmd` + `+`(JIS の `;` 含む・テンキー)/`-`/`0` で UI ズーム(webview `setZoom`、50〜200%・10% 刻み、localStorage に保存し起動時に復元)。IME 変換中(`isComposing`)は無視。
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
  - **精密追突防止スケジューラ**: 各レーンは直近投入コメントの `{t, w, s}`(投入時刻/幅/速度 px秒)を保持し、新規コメントの速度と相対比較して「頭被り(`A=(prev.w+gap)/prev.s`)」と「追突=速い後続が遅い先行を追い越す(`B=(base+(sNew-prev.s)*dur)/sNew`)」の両方を防ぐ最小投入間隔 `max(A,B)` を満たすレーン(slack 最大)を選ぶ。高負荷で全レーンが埋まると最も早く空くレーンに相乗り(劣化許容)。デスクトップ窓と OBS テンプレで同一アルゴリズム(パリティ必須)。
  - **表示領域(area)**: コメントを流す縦帯を `full`(全体)/`top`(上半分)/`bottom`(下半分)に制限可能(中央のゲーム画面を空ける)。デスクトップは弾幕設定の「表示領域」セレクト、OBS は `?area=` で指定(不正/未指定は `full`)。
  - **端フェード**: 右端の出現(0→6%)と左端の消失(94→100%)を opacity アニメ(`danmaku-fade`)で柔らかく。移動アニメ(`danmaku-fly`)の linear タイミングは不変で、撤去は `animationName==='danmaku-fly'` 側のみ(2アニメでの二重/早期撤去を防ぐ)。全体不透明度(`?opacity=`/`--opacity`)と要素フェードは乗算合成。
  - **連投まとめ(×N)**: 同一本文(名前前置前の素の本文 `core` をキー)の連投を `COALESCE_WINDOW_MS`(1500ms)内なら1つの弾幕に集約し `×N` カウント表示(『草』『w』『888』のスパム抑制)。流れるコメント(非gift/非system)のみ対象。集約は既存要素のテキスト書き換えのみで `lanePrev`(追突防止状態)は触らない。撤去/間引き時に集約マップから掃除(OBSは `size<32` ガード)。デスクトップは設定トグル、OBS は `?coalesce=0` で無効化(既定 ON)。
  - **投げ銭固定強調弾幕**: SuperChat/Bits/メンバーは流さず画面上部に `PIN_SEC`(8秒)固定表示してフェード撤去(ニコ生 `ue` コマンド風・見逃し防止)。同時 `PIN_MAX`(6件)・最古から間引き・最新を上。半透明角丸ボックス+種別色。デスクトップは設定トグル(タイマは `onDestroy` で `clearTimeout` しリーク防止)、OBS は `?pin=0` で無効化(既定 ON、OFF時は従来通り流す)。
- **コメント投稿** (`CommentComposer.svelte`, `sources/twitch_send.rs`, `sources/youtube_send.rs`): 自分でコメントを送信。Twitch は IRC、YouTube は公式Data API `liveChatMessages.insert` で送信する。YouTube認証はシステムブラウザ + loopback redirect + Authorization Code/PKCE、scopeは`youtube.force-ssl`。refresh tokenはOS資格情報ストア、access tokenは期限付きメモリキャッシュに保持し、受信ホットパスへ処理を追加しない。配信中の`liveChatId`は`liveBroadcasts.list`で解決して60秒キャッシュする。
- **参加型配信の管理** (`Participation.svelte`, `Raffle.svelte`): キーワード(既定「参加」)での参加登録、先着/ランダム抽選、専用タブ。既定 OFF。
- **投げ銭パネル** (`DonationPanel.svelte`): SuperChat/Bits/メンバーを通常コメントと分けて表示(アプリ内タブ / OBS `?only=gift`)。既定 OFF。
- **配信振り返りダッシュボード** (`Dashboard.svelte`, `Sparkline.svelte`): コメント数・視聴者推移などの集計表示。
- **タイマー/ゴール/エフェクト/マイルストーン** (`Timer.svelte`, `GoalsBar.svelte`, `Effects.svelte`, `Milestone.svelte`): 配信演出系。OBS テンプレ `timer`/`goals` と連動。
  - Goals はコメント、視聴者、高評価、リアクションを表示できる。リアクションは YouTube 絵文字リアクション累計で、YouTube のみ有効(`reactionsAvailable=false` で自動非表示)。
  - アプリ内エフェクト有効時は、YouTube匿名リアクションの絵文字別増分を専用16msバッチIPCで受け、右端寄りに浮上・フェード表示する。初回pollはrolling windowの再送分なので、再接続時の重複を避けるため統計・演出とも更新しない。描画数は画面幅別の上限を設け、`prefers-reduced-motion` では演出を省略する。コメント/TTS/OBSには混ぜない。
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

- 共通クエリパラメータ: `ws`(接続先), `channel`(フィルタ), `only=gift`(投げ銭のみ) 等。テンプレ間で `max` の意味が異なる点に注意 — 通常overlayでは「表示行数の上限」、`danmaku` では「同時アニメDOM数の上限(MAX_ACTIVE)」。`danmaku` 専用: `dur`(横断秒数)/`size`(文字px)/`opacity`/`name`(名前前置)/`outline`(縁取り)/`area`(`full`/`top`/`bottom` 表示縦帯)/`coalesce`(連投まとめ ×N、`0`で無効)/`pin`(投げ銭を上部固定強調、`0`で無効)。弾幕URL生成(`Settings.svelte` の `withDanmaku()`)は弾幕に無関係なパラメータ(`max`/`ttl`/`font`/`bg`/`pos`/`icon`)を allowlist で除外する。
- システム種別(`kind==='system'` 接続通知等)は配信画面に流さない(全テンプレで除外)。
- **削除・BAN の反映(P6 3b)**: `/ws?modEvents=1`(`1` / `true` のみ opt-in。`0`・値なし・未指定は opt-out で購読もしない)のクライアントにだけ、同じソケットで chat 配列の後に `{"moderation":[ModerationEvent…]}` を 16ms tick 単位で送る(channel フィルタも適用)。opt-out の改造テンプレには一切届かない。
  - 送るイベント: 配信側(§7 Phase 3)、アプリ発の `moderate_delete_message` / `moderate_ban_user` 成功時(moderation forwarder 経由で UI・TTS と同じ経路。BAN は `certain`=true・`beforeMs`=null)、ローカル非表示 `hide_message`(`platform`/`channel` 付きの時のみ messageDeleted を OBS へだけ。UI へは送らない)。解除(unhide / unban)は OBS へ戻さない。
  - 同梱テンプレ `default` / `simple` / `bubble` / `ticker` / `donation` / `danmaku` が対応し、該当コメントを DOM から完全に除く(減光しない)。messageDeleted は ID、userCleared は author + `beforeMs`(`certain` に関係なく除く)、chatCleared は `beforeMs` 以前。`donation` の合計、`danmaku` の集約弾幕(×N)は要素ごと除く・合計は戻さない。
  - `ranking` / `goals` / `timer` は集計表示で本文を出さないため対象外(接続 URL も opt-in しない)。
  - 後着対策: 各テンプレが直近イベント(最大 512 件、記録から 10 秒、`performance.now()`)を覚え、後から届いたコメントを表示しない。テンプレは単体の静的サイトなので共有ファイルにせず、同一ブロック(`fc-moderation:begin`〜`end`)を各 `app.js` に複製し、`templates/obsModeration.test.mjs` が一致と挙動を検証する。

## 10. 設定永続化 (`config.rs`)

- 保存先: Tauri の app config dir に `config.json`。
- 内容: channels[], obs{port}, tts{backend, options}, moderation{ngWords[], ngUsers[], highlights[]}, ui{maxBuffer, notifySound, notifyVolume}, youtubeOverrides{apiKey?, clientVersion?, paths?}, credentials{youtubeApiKey?, ...}。
  - `credentials.youtubeApiKey`(空文字既定): 公式YouTube streamList用。空ならInnerTubeのみ。
  - `ui.notifySound`(bool 既定false) / `ui.notifyVolume`(f32 0.0〜1.0 既定0.5): キーワード通知の効果音設定。serde default でキー欠落の旧 config も後方互換。
  - `obs`: ポートに加え `maxRows` / `fontScalePct` / `ttlMs` / `bgOpacityPct` / `position` 等の見た目設定。範囲外値が入らないよう **Rust 側でも `normalize()` で clamp**(`maxRows` 1..=1000, `fontScalePct` 50..=200, `bgOpacityPct` 0..=100, `ttlMs` 500..=600000, `position`∈{top,bottom})。`normalize()` は config ロード時と `update_config` 適用時の両方で呼ぶ。上限定数は Rust 側に1箇所(`MAX_OBS_ROWS`)を置き UI/テンプレと値を揃える。
  - `channels[].ownStream`(bool 既定false): 「自分の配信」の印(X 自動告知の対象)。旧 config は欠落で false。
  - `xPost{enabled=false, mode="intent"|"api"="intent", intentOpen="button"|"auto"="button", combine=true, template, delaySecs=60, staleMinutes=15, cooldownMinutes=60, dailyLimit=5, callbackPort=47821}`: X 自動告知。`template` 既定は `【配信開始】{title}\n{url}\n{date} {time}`。全項目 serde default。`normalize_all()` で clamp(`delaySecs` 0..=600, `staleMinutes` 0..=180, `cooldownMinutes` 10..=1440, `dailyLimit` 1..=20、`mode` / `intentOpen` は不正値なら既定)。
  - `credentials.xPostClientId`(任意): API モード用の X OAuth 2.0 Client ID。未設定なら JSON に出さない。設定画面の「X に接続」(`connect_x_post`)が `xPost.callbackPort` と一緒にこの 2 項目だけを保存する。
  - `xOverrides.endpoints` の `postAuthorizeUrl` / `postTokenUrl` / `postTweetsUrl` / `postUsersMeUrl`: API モードの X エンドポイントの上書き(起動時に読む。変えたらアプリを再起動)。
  - X 投稿の認証は `config.json` に置かない: OS 資格情報ストア(`secret_store`)のエントリ `x-post-oauth-token` に `{ clientId, refreshToken, userId, username }`。access token はメモリのみ。
  - 台帳 `<config_dir>/x-autopost-ledger.json`(§13)。
- 起動時ロード、変更時保存。`config.json` をアプリ設定の正本とする。
- **localStorage 併用**: 一部のUI/ウィンドウ局所設定は各ウィンドウの `localStorage` に保持する(`config.json` には載せない)。現状: 弾幕表示設定(`fc.danmaku`)、テーマ、最前面ピン状態、コマンドパレット履歴 等。これらはウィンドウローカルかつ低リスクなため意図的に localStorage 側に置いている。

## 11. フェーズ計画

- **P0 足場**: 設定/モデル/Tauri雛形/Svelte雛形/ビルド導線(SETUP)
- **P1 接続コア**: Twitch source(実動) + Bus(IPC) + UI一覧(仮想化+rAF) → "Twitchが映る"
- **P2 YouTube**: InnerTube + 寛容パーサ + overrides → "YTが映る"
- **P3 OBS**: axum WS + default テンプレ
- **P4 TTS**: 3バックエンド + ルーティング
- **P5 モデレーション + 設定UI仕上げ**
- **P5+ 拡張UI(実装済み, §8.1)**: 弾幕オーバーレイ(デスクトップ窓 + danmaku テンプレ)、コメント投稿(Twitch/YouTube)、参加型/抽選、投げ銭パネル、ダッシュボード、タイマー/ゴール/エフェクト、マルチカラム、設定ポータビリティ、テンプレ編集UI、最前面ピン、追加OBSテンプレ8種
- **P5++ X対応(実装済み, §4.3)**: X (Twitter) ライブ配信のチャット受信(`api.x.com/live-chat` の NDJSON ストリーム、認証不要・読み取り専用・投稿非対応)
- **P5+++ niconico対応(実装済み, §4.4)**: ニコニコ生放送のコメント受信(watch WS + NDGR Protobuf、読み取り専用・投稿非対応)
- **X 自動告知(実装済み, §13)**: Intent モード(配信開始で X の投稿画面を開く。印・バナー・テンプレート・台帳)と API モード(OAuth 2.0 PKCE で接続し `POST /2/tweets` で完全自動。接続 UI・テスト投稿)
- **P6(後)**: OAuth実モデレーション(実BAN/削除)、さらなる追加Source、テンプレ編集のライブプレビュー強化

## 12. 既知の制約・注意

- WSLでは Tauri ビルド不可(Linuxバイナリになる)。ビルドはWindows側。
- YouTube InnerTube は非公式 → 仕様変更リスク。寛容パース+overrides+ログで吸収。
- X `live-chat` ストリームも非公式 → 同リスク。寛容パース+`xOverrides` で吸収。
- 実モデレーションはOAuth必須(P6)。
- YouTube 公式 streamList 経由で受けたコメントの配信側削除はアプリに反映されない(公開 `stream_list.proto` に削除イベントの定義が無い。BAN / タイムアウトは反映される)。
- X 自動告知(Intent)は投稿の成否が分からない(状態は `opened` まで)。同一文面の繰り返しは X の規約に触れうる。Twitch の配信検知は Helix 認証(Client ID)が前提。YouTube は videoId 直接指定の行だと対象外。
- X 自動告知(API)は従量課金で無料枠が無い(2026-11 時点の目安で 1 件 $0.015、URL 入りは $0.20。正は Developer Console)。X の許可はアプリ×ユーザーで 1 つで、同じ Client ID を別 PC・別ツールでログインすると先の接続は切れる。refresh token は約 6 か月で失効し、その後は再ログインが要る。同一文面は X が 403 で拒否する。
- Tauri build にはアイコン(`src-tauri/icons/`)が必要。

## 13. X 自動告知 (`autopost/`)

配信開始を検知して X に告知する機能。**既定オフ**。2 つのモードがある(`xPost.mode`)。

- `intent`(既定): X の投稿画面(Web Intent)を開く。投稿ボタンは本人が押すので、X の API・OAuth・開発者アプリ・クレジットは要らず無料。
- `api`(任意): 利用者自身の X 開発者アプリ(Client ID)で OAuth 接続し、公式 API で完全自動に投稿する。接続済みの時だけ選べる(§13.1)。

判定・台帳・テンプレート・猶予・取消は両モード共通。

- **きっかけ**: 「自分の配信」の印(`channels[].ownStream`)が付いた YouTube のチャンネル指定行(`@ハンドル` / `UC…`)・Twitch 行・ニコ生行の `LiveEvent::Started`(§3.1)。YouTube の videoId 直接指定行と X 行は対象外。印と `xPost.enabled` の両方が要る。
- **駆動**: `spawn_autopost` が `LiveEvent`・UI 操作・設定変更を受け、純粋な状態機械 `Coordinator` に渡す。I/O(台帳保存・ブラウザ起動)は返された `Action` を順に実行する側で行い、台帳保存が先、`OpenIntent` が後。保存に失敗したら開かない。
- **判定順**(上から): 無効・印なし → 無視(台帳に書かない) / 台帳に同じ `platform:streamId` → 無視 / 同じ配信が猶予中 → 無視 / 鮮度(開始時刻が `staleMinutes` を超過 → `skipped(stale)`。`staleMinutes=0` はこの実行中に未配信(`Ended`)を観測した配信だけ通す、開始時刻不明で未配信を観測していない → `skipped(unknownStart)`) / 直近の告知から `cooldownMinutes` 以内 → `skipped(cooldown)`(Twitch の回線断で配信 ID が変わっても再告知しない) / 過去 24 時間の告知が `dailyLimit` 以上 → `skipped(dailyLimit)`。通れば `delaySecs` 後に発火する告知を作る。猶予中は同時配信を 1 件にまとめる(`combine=true`。`false` なら配信ごと)。猶予中に `Ended` が来た配信は告知から外し、台帳には書かない。
- **開き方**(`xPost.intentOpen`): `button`(既定)はバナーの「告知を投稿」(`approve_x_autopost`)で開く。`auto` は猶予後に自動で開く。承認・取消はバナーに出ている告知の `id`(`AutopostStatus.id`)を付けて送り、先頭の告知と一致しない時は無視する(二度押しが次の告知に当たらない)。どちらも猶予中・承認待ちはバナーの取消(`cancel_x_autopost`)で止められ、取消は `cancelled` として台帳に残り同じ配信を再告知しない。承認待ちには期限があり、過ぎると `skipped(expired)`。
- **開く URL**: `https://x.com/intent/post?text=<描画済み文面を URL エンコード>` を既定ブラウザで開く(既存の `open_url`)。投稿されたかは分からないので台帳の状態は `opened`。
- **台帳**(at-most-once): `<config_dir>/x-autopost-ledger.json`、`{ version, entries:[{ key, platform, streamId, channel, state, atMs, text?, reason? }] }`、新しい順に 200 件。一時ファイル → fsync → rename で保存。壊れていたら `.corrupt-<時刻>` に退避し、その起動中は自動告知を止めて通知する(状態 `failed` / reason `ledgerFailed`)。`posting` のまま残っていたものは起動時に `uncertain` 扱いで、自動では再実行しない。
- **文面テンプレート**(`xPost.template`、既定 `【配信開始】{title}\n{url}\n{date} {time}`): 変数 `{title}`(先頭配信のタイトル。YouTube > Twitch > ニコ生)、`{url}`(含む全配信の URL を 1 行ずつ)、`{platform}`(` / ` 連結)、`{date}`(`YYYY/MM/DD`)、`{time}`(`HH:MM`)。空行は詰め、未知の `{…}` は残して警告。`{url}` の無いテンプレートは `MissingUrl` で拒否。タイトルは制御文字・改行を空白に、`@` を `＠` に、`$`+英字を `＄` に置換(ハッシュタグは残す)。直近の台帳の文面と完全一致したら末尾に ` {time}` を足し、それでも一致すれば `Duplicate` で開かない。
- **280 字**: 重み付きで数える(U+0000–U+10FF などは 1、日本語・絵文字は 2、`http(s)://` の語は 23)。絵文字の合成列は符号位置ごとに数える安全側の近似。超過時に縮めるのは `{title}` だけ(末尾 `…`)。固定部分と URL だけで超えるなら `TooLong` で開かない。数えるのは Rust のみで、UI は `preview_x_post` の結果(`{ text, weightedLen, warnings }`)を表示するだけ。
- **UI**:
  - 登録チャンネル一覧(`ChannelAdd.svelte`)の各行に「告知」トグル。対象外の行は無効化して理由を title に出す。押すと `set_channel_own_stream` が印だけを保存する(Source は再起動しない)。Twitch 行で Client ID が未設定の時は警告色にして、配信検知が動かず告知されない旨を出す。
  - 告知バナー(`XAutopostBanner.svelte`、メイン画面ヘッダー直下): 猶予中は残り秒と取消、承認待ちは「告知を投稿」と取消(失効までの秒数つき)、`opened` は投稿画面を開いた旨、`failed` / `uncertain` は理由と再試行(`retry_x_autopost`。`canRetry` が true の時だけ出す。ブラウザを開けなかった時は `failed(openFailed)` で台帳も `failed`、テンプレート起因の失敗は再試行で描画し直す)、`skipped` は理由を数秒表示。状態は起動時に `get_x_autopost_status`、以後イベント `x-autopost-status`(状態が変わった時だけ emit)。
  - 設定「X への配信開始告知」(`XPostSettings.svelte`): 有効化、`intentOpen`、`combine`、テンプレート編集と変数の挿入、プレビューと文字数(n/280)、猶予秒・取りこぼし許容分・クールダウン分・1 日の上限。保存は設定画面の保存ボタン。
- **状態**(`x-autopost-status` の `state`): `idle` / `pending`(猶予中) / `awaitingApproval` / `posting` / `opened` / `posted` / `failed` / `uncertain` / `cancelled` / `skipped`。`skipped` / `failed` / `cancelled` は `reason`(`stale` / `unknownStart` / `cooldown` / `dailyLimit` / `notConnected` / `expired` / `ledgerFailed` / `Duplicate` / `TooLong` など)を伴う。
- **注意**: 毎回同じ文面は X の規約に触れうるので、テンプレートにタイトルか日時を入れる。「実質同一」の判断は X 側にあり、この機能は技術的な完全一致を避けるだけ。X の `xAuthToken` cookie は告知に使わない。

### 13.1 API モード(`autopost/oauth.rs`・`autopost/x_api.rs`)

- **公式 API のみ**。X の `xAuthToken` / `xCsrfToken` cookie は投稿に使わない(`autopost/` に参照が無い)。
- **接続**(`connect_x_post(clientId, callbackPort)`): OAuth 2.0 Authorization Code + PKCE(S256)、公開クライアント(client secret 無し)。認可 URL `https://x.com/i/oauth2/authorize`、スコープ `tweet.read tweet.write users.read offline.access`。コールバックは `http://127.0.0.1:{port}/callback`(`localhost` は使わない。bind も 127.0.0.1 のみ)、ポートは固定で既定 47821(`xPost.callbackPort`)。X は redirect URI を完全一致で照合するので、設定画面に出す URL をそのまま Developer Console に登録する。bind できなければ別ポートを試さず `PortInUse`。新しい接続・`cancel_x_post_login` は進行中の待受を止めてから bind する。`state` 不一致・別パスは 404 で待ち続け、`error=` は失敗。全体 180 秒・1 接続の読み取り 5 秒。受け取った code は即 `POST https://api.x.com/2/oauth2/token`(form、body に `client_id` と `code_verifier`、Authorization ヘッダ無し)で交換し、`GET /2/users/me` で `id` / `username` を取って保存する。設定画面に「@username として接続中」を出す。
- **保存と refresh**: refresh token は `secret_store` のエントリ `x-post-oauth-token`、access token はメモリのみ(期限は `expires_in` − 60 秒)。refresh token は single-use で、refresh のたびに新しい物が返り古い物は使えなくなる。refresh と保存は 1 つの mutex の中で直列に行い、応答を受けたら使う前に新しい refresh token を保存する(最大 3 回)。保存できなければメモリの認証で投稿は続け、`unsavedToken` を立てて設定画面に警告し、次に使う時にまた保存を試す。refresh が 400/401 で拒否されたら keyring を読み直し、別の refresh token が入っていればそれで 1 回だけやり直す。同じなら保存を消して `ReloginRequired`(約 6 か月使わず失効した時もここ)。読み直しに失敗した時は保存を消さず `TokenStore`。通信失敗・5xx は保存を消さず `Network`。切断・再接続との競合は世代番号で判定し、古い世代の結果は保存しない。
- **切断**(`disconnect_x_post`): この PC の keyring とメモリの認証を消すだけ(YouTube と同じ)。X 側の許可は取り消さない(X の設定の「連携しているアプリ」から外せる)。API モードだった設定は切断の成否によらず `intent` へ戻して保存する。
- **モードの保存**: `update_config` は `intent` → `api` の切り替えを、今の Client ID で接続済みの時だけ受け付ける。すでに `api` の設定を保存し直すのは止めない(認証が切れていれば告知は `skipped(notConnected)`)。
- **投稿**(`x_api::post_text`): 発火時に台帳へ `posting` を保存してから `POST https://api.x.com/2/tweets`(`{"text": …}`、Bearer)を 1 回。保存に失敗したら HTTP は呼ばない。猶予の開始時に access token を先に用意し(`PrewarmToken`)、認証切れを投稿前に知る。結果の扱い:

  | 事象 | 自動再試行 | 台帳 / 状態 |
  |---|---|---|
  | 2xx | — | `posted`(`data.id` が無ければ ID 不明の `posted`)。バナーにポストへのリンク |
  | 接続前の失敗(DNS・接続拒否) | 2 秒後・8 秒後の 2 回 | 最終的に `failed(Network)` |
  | 401 | access token を捨てて refresh → 1 回だけ再送 | 再失敗で `failed(ReloginRequired)` |
  | 403(本文の detail に duplicate) | なし | `failed(Duplicate)`。再試行は文面を描画し直す(失敗した文面が台帳に残るので末尾に ` {time}` が付く) |
  | 403(その他) | なし | `failed(Forbidden)` |
  | 402、または 4xx で `title` / `type` / `detail` に `credits` を含む | なし | `failed(InsufficientCredits)`(本文は 2 通り報告がある。ステータスは HTTP の値で見る) |
  | 429 | `x-rate-limit-reset` が 90 秒以内なら待って 1 回 | `failed(RateLimited)` |
  | 5xx、送信後のタイムアウト、応答の読み取り失敗、分類できない送信エラー | なし | `uncertain`(投稿されたか不明) |
  | その他の 4xx | なし | `failed(Other)` |

  再送は利用者の「再試行」(`retry_x_autopost`)で同じ文面を 1 回だけ。自動では送らない。成功は 2xx だけで判定し、本文の内容で成功を失敗にしない。
- **テスト投稿**(`send_x_test_post`): 文面 `fast-comment 連携テスト YYYY/MM/DD HH:MM`(URL なし)。UI は確認ダイアログで文面と「実際に公開され、課金される」旨を出してから呼ぶ。台帳・クールダウン・1 日上限には数えない。
- **状態取得**: `get_x_post_status` → `{ configured, connected, username, callbackUrl, unsavedToken }`。接続・切断の後は駆動タスクに `AuthChanged` を送り、投稿できるかを読み直させる。
- **秘密値**: access / refresh token と認可コードはログ・エラー文・emit・`Debug` に出さない(`XPostError` は固定文のみ)。

## 14. 自動更新(`tauri-plugin-updater` + `tauri-plugin-process`、v0.2.0〜)

- **配布元**: GitHub Releases。`plugins.updater.endpoints` は `https://github.com/machaniconico/fast-comment/releases/latest/download/latest.json` の 1 本。`latest` は draft・prerelease を指さないので、draft のまま置いた版は配られない。
- **署名**: minisign 鍵。公開鍵の中身を `tauri.conf.json` の `plugins.updater.pubkey` に直書き。秘密鍵とパスワードは GitHub Secrets(`TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`)にだけ置き、リポジトリ・ローカルに置かない。鍵を失うと既存インストールへ更新を配れなくなる(pubkey を変えた版は手動で入れ直してもらう)。
- **成果物**: `bundle.createUpdaterArtifacts: true`。CI(`.github/workflows/release.yml`、tag `v*` または手動実行、`windows-latest`)の `tauri-action@v1` が NSIS / MSI と `.sig`、`latest.json` を draft Release に上げる。`updaterJsonPreferNsis: true` で `latest.json` は NSIS の exe を指す。ローカルビルド(`build-windows.bat`)は鍵が無ければ `tauri.nosign.conf.json` を重ねて署名を作らない。
- **バージョン**: `tauri.conf.json` / `Cargo.toml` / `package.json` の `version` と tag(`v` を除く)が一致しないと CI が失敗する。リリースノートは `.github/release-notes/vX.Y.Z.md`(無ければ失敗)で、Release 本文と `latest.json` の `notes` になる。
- **インストール**: Windows は `plugins.updater.windows.installMode = "passive"`(進捗バーのみ・操作不要)。NSIS は `currentUser` インストールなので管理者権限は要らない。Windows ではインストーラ起動時にアプリが終了する。他 OS は `relaunch()`(`process:allow-restart`)で再起動。
- **UI**(`src/lib/appUpdate.svelte.ts`、純粋関数は `appUpdate.ts`):
  - 起動時に `check()`(15 秒タイムアウト)を静かに実行。失敗(オフライン・dev ビルド・`latest.json` 欠落)はログのみで画面に出さない。
  - 署名付き更新が使えない時は従来の `check_for_update`(GitHub Releases API の最新 tag と semver 比較、`update.rs`)へ落とし、リリースページへのリンクだけ出す。
  - 新版があれば画面上部に控えめなバナー(新旧バージョン、「更新して再起動」「後で」)。ダウンロード中は進捗(%・バイト数、Content-Length 無しならバイト数のみ)。
  - 設定「アプリの更新」に現在のバージョンと「更新を確認」ボタン。手動確認は「最新版です」・エラー文も表示し、閉じたバナーを再表示する。
- **permission**: main ウィンドウの `capabilities/default.json` に `updater:default` と `process:allow-restart`。弾幕ウィンドウには付けない。
- **v0.1.0 からの移行**: v0.1.0 は updater 無しなので自動更新できない。v0.2.0 を手動で入れてもらい、以降が自動更新の対象。
