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

### 1.3 ツイキャス認証 (`sources/twitcasting_auth.rs`、PR1)

- 保存したトークンは、コメント取得の公式 API ポーリング(§4.5)にだけ使う。取得経路は `twitcasting.mode`(`auto` 既定 / `unofficial` / `official`)で決まる。`auto` は非公式経路で始め、ログイン済みで非公式が連続して読めなければ公式へ切り替える。`unofficial` はトークンを一切使わない。`official` は公式 API だけで、未ログインでも非公式へは落とさない(「ツイキャスにログインしてください」を出して待つ)。公式 API の送信先(Bearer を載せる先)は設定で変えられない。
- 公式 API v2 の Implicit (`response_type=token`) のみ。ClientSecret は扱わない。公開 ClientID は `DEFAULT_CLIENT_ID` (未登録のため空文字) + 任意の上書き `credentials.twitcastingClientId`。空白だけの上書きは既定へ戻す。
- ログイン中だけ `127.0.0.1:<port>` と `[::1]:<port>` を bind(`<port>` は `twitcasting.callbackPort`、既定 39822 = `CALLBACK_PORT`。WSL の mirrored モードが Windows 側の 44620〜48715 を確保して bind できないため範囲外にした)。IPv6 無効(アドレス使用不可)のみ IPv4 だけに劣化し、いずれかが使用中なら開始しない(`PortInUse`。エラー文で WSL の確保範囲と、ポートを変えて開発者ページのコールバック URL を登録し直す案内を出す)。コールバックは `http://localhost:<port>/twitcasting/callback`(status の `callbackUrl`)。180 秒・取消・次のログインで終了し、コマンドが返るまでにポートを解放する。
- 固定 HTML は `history.replaceState` で URL のフラグメントとクエリを消す。ページとサーバーの両方でキーと値をパーセントデコードし、`+` はリテラルとして保持し、同じキーは最後の値を採用する。ページはフラグメントの `access_token` / `expires_in` / `state` の採用分だけを再エンコードせず同一オリジンへ POST する。拒否 (`result=denied` または OAuth の `error`) はクエリまたはフラグメントから固定の `result=denied` と、存在する場合だけ同じ場所の `state` を送る。POST は Host / Origin 検査後に state 省略または一致で拒否を確定し、不一致（空も含む）は待受を続ける。GET クエリの拒否は state が一致した時だけ確定する。state を省略した拒否の GET は待受を続け、ページのスクリプトが同一オリジンへ拒否を POST した時点で確定する。不一致の state を含む拒否の GET には中立の終了ページを返す。
- Host と、指定された Origin を 3 種のループバックに限定する。POST 本文は 4096 バイト以下、32 バイト乱数の state を定数時間で比較する。トークンは可視 ASCII で、**新規保存のトークン単体は最大 1024 バイト**。実際に保存できる長さは、ClientID、アカウント名、JSON エスケープなどを含めた記録全体の検査で決まる。state が正しく 1025〜2048 バイトの可視 ASCII トークンは `TokenStore` でログインを終了する。空、制御文字を含む値、2048 バイト超は不正なコールバックとして待受を続ける。既存の保存値は従来の構文上限で復元する。成功は一回限り。全応答に no-store / no-referrer / nosniff と、外部リソース禁止・`frame-ancestors 'none'` の CSP を付ける。
- 保存前に `/verify_credentials` を Bearer、`X-Api-Version: 2.0`、JSON Accept で呼び、`app.client_id` を照合する。トークン・state・拒否の詳細はログ、エラー文、Debug、emit、状態 JSON に出さない。
- 資格情報ストア (`integrations::SecretStore`、service `fast-comment`、キー `twitcasting-oauth-token`) に `{clientId, accessToken, userId, screenId, name, expiresAtMs, obtainedAtMs}` を保存する。検証 API の結果を反映し、ブラウザへ成功を返す前に、外側の JSON、エスケープ、保存時刻の最大桁数まで含めた UTF-16 の記録全体を検査する。Windows の上限 2560 バイトを超える場合は、ブラウザへ HTTP 400、アプリへ `TokenStore` を返し、ストアへのアクセスを省く。`obtainedAtMs` は grant 受領時刻。`expires_in` は省略時のみ期限不明とし、指定時は 1〜315360000 秒の整数を受け付ける。不正な期限や保存時にすでに期限切れのトークンではログインを失敗させる。復元時に `expiresAtMs` が `obtainedAtMs` から 10 年を超える値、または JS の Date 範囲外の値は破損として扱う。
- ストア操作はプロセスごとに単一のワーカーと待機枠 4 件の FIFO で実行する。Windows で同じユーザーの別ログオンセッションが同時に使う場合も、ローカルアプリデータの `twitcasting-credentials.lock` に対する OS のファイルロックで、ツイキャスのマーカー検査・更新、資格情報の読込、照合から削除まで、ログインの保存から確定または巻き戻しまでを直列化する。排他ファイルは置換せず、プロセス終了時に OS がロックを解放する。ロック待ちには操作ごとの `read_timeout` の絶対期限を使い、ログインでは全体期限も適用する。ストアのキューとブロッキングタスクの開始待ちを含む残り時間まで待ち、他セッションのロック保持が期限まで続いた場合は `lockTimedOut` を返す。
- ストア操作の登録時に待機枠が満杯なら `TokenStore` を返す。保存待ち（キュー・排他・旧値取得・put）は `read_timeout` で `TokenStore` を返し、ログインの取消・全体期限にも従う。待ちを終えた書き込みは、その値が実際に保存されていれば後続の操作より前に元に戻す。旧値が読めなかった時は、書き込みが残っている場合だけ削除する。巻き戻しは失敗が返った場合に最大 3 回試し、すべて失敗した場合はメモリを未ログインにする。開始済みのストア操作は完了まで後続を待たせる。ログアウトはマーカーの追加、ストアの削除待ち、マーカーの解除を合わせて `read_timeout` まで待つ。マーカー I/O とファイルロック取得はブロッキングタスク上で実行し、その間メモリとログインのロックを解放する。期限後も開始済みのログアウトは継続し、重複して登録しない。
- `LoggedOut` / `Loading` / `LoadFailed` / `Ready` / `ReloginRequired` を watch 通知する。ログアウト保留・理由・読込の進み具合だけが変わった時も通知する。起動時は `Integrations::open` の完了後、状態を登録する前に `loading` にし、登録後に `initialize` の future を実行する。マーカー検査、必要な照合、復元は 1 回の `read_timeout` を共有する。起動時に別の記録を見つけた場合は、照合時に読んだ値を復元に使う。期限後の値や照合時に解析できなかった値は `loadFailed` にし、再読込できるようにする。期限判定には検査中の経過時間を含める。マーカー検査中はログインを `TokenStore` で拒否し、画面のログインボタンも `loading` 中は無効にする。通常の保存値の読込も失敗または時間切れで `loadFailed` になる。
- 期限切れ (`now >= expiresAtMs`)、ClientID 不一致、401 通知で再ログインが必要になる。既知の期限はタイマーでも通知する。Bearer は Ready かつ期限内かつ ClientID 一致時だけ取得でき、API リクエストの 401 は取得時の世代が一致する場合だけ現在のログインを失効させる。`user_id()` は Ready 時に返す。失効では保存値を維持し、再読込で状態を変えない。
- 明示ログアウトではメモリ上の認証をすぐ解除し、知っている記録を削除対象の集合に追加する。対象が未確定なら、受付時刻を決めた初回の受付に限り、別セッションのマーカーに他の識別子があっても、ストアから読めた現在値を対象に加える。初回の受付がロック待ちの時間切れやキュー満杯などでストアを読む前に終わっても、再試行は初回扱いにせず、下の受付時刻で判定する(再試行の時点の現在値は、受付後に別セッションが保存したログインかもしれないため)。外側の JSON または記録を解析できない場合は、生の値を識別して照合する。生の値も読めなければ、初回の受付に限り資格情報を直接削除できる。明示操作自身のマーカー読込だけでは対象は未確定のまま。起動時・再読込で見つけた保留マーカーでは識別子で対象を確定する。空のストアを含め、読込または直接削除が成功した時点で対象を確定する。確定後の再試行では既存の識別子を照合する。
- 同じ保留中のログアウトにつき直接削除は初回の受付の一度だけ。直接削除にも失敗した後、再試行でまた読めなければ保留を維持し、理由 `deleteFailed` を返す。マーカーにもアクセスできなければ `markerAndDeleteFailed` を返す。再試行で記録が読めたら、既存の識別子で照合し、`obtainedAtMs` が最初の明示ログアウトの受付時刻以前(同時刻を含む)の記録を削除対象に加える。受付時刻は同じ PC の壁時計の Unix epoch ミリ秒としてメモリに保持する。`obtainedAtMs` はグラントの受信時刻(ログイン開始時の壁時計に、そこから受信までの経過を足した値)で、記録の保存はトークン検証などの後になる。そのため既知の限界として、別セッション B がグラントを受信してから保存するまでの間に、このセッションがログアウトを受け付け、しかも初回の受付がストアを読む前に失敗した場合、B の記録は受付時刻以前になり、再試行で消える(`<` で比べても同じ。失敗が重なった時だけの経路で、B は再ログインで戻せる)。同時刻を消す側に含めるのは、ログアウトした記録が次回起動で復元されるのを防ぐため。壁時計が 1970 年より前で 0 を返す場合、受付時刻は 0 になり、記録の `obtainedAtMs` はログイン開始からの経過だけになるので、取得時刻ではセッションを区別できない。この場合の再試行は読めた記録をすべて削除対象にする(ログアウトした資格情報を残すより、別セッションに再ログインさせる方を選ぶ)。受付時刻より後の記録は、既存の削除対象に一致しない限り保持する。再試行で読めた解析できない値は、直接削除を試したかに関わらず削除する(どのセッションも解析できる記録を保存するため、後から保存された別のログインではない)。照合で残した別のログインは `otherLoginKept`、起動時・再読込や対象確定後の照合で残した解析不能な記録は `recordUnrecognized` で知らせる。直接削除後は `unreadableRecordDeleted` を表示する。削除の完了は `logoutsCompleted` の増加で知らせ、理由には最後の試行の結果を出す。
- マーカー `twitcasting-logout-pending` は 1 行につき 1 件の削除対象を保存する。識別子は正規化した記録 JSON の SHA-256 の小文字 16 進とし、解析不能な記録には `raw:`、外側の JSON には `envelope:` を付ける。旧形式の 0 バイトの通常ファイルは任意の現在値 1 件を表し、他の識別子との和集合では `*` として保存する。空白のみ・通常ファイル以外・64 KiB 超は破損扱いとし、起動時の削除は保留する。起動時はロック内で前回終了時に残ったマーカーの一時ファイルを掃除する。
- マーカーの更新ではロック内で読み直して集合の和を取り、一時ファイルを同期してから rename で公開する。起動時の再試行では削除直前にマーカーを再確認し、再確認に失敗しても判明済みの識別子はメモリに保持する。資格情報ストアの現在値が対象の集合に含まれる場合だけ削除し、ストアに無いと確認できた識別子だけをマーカーから外す。最後の識別子が消えたらマーカーを取り除く。明示操作では破損マーカーや空ディレクトリ、リンクを取り除ける。
- `logoutPending` と理由はストアとの照合とマーカーの解除結果から決める。`logoutsCompleted` は、実際の削除とマーカーの解除が揃った時に増える。削除後に解除だけ失敗した時は削除済みをメモリに保持し、再試行で解除できた時に一度だけ画面へ削除完了を通知する。
- 安定した設定ディレクトリを取得できない時は復元を保留し、ログアウトは失敗として返す。マーカー検査が時間切れなら理由 `checkTimedOut` で再検査でき、アクセスエラーなら `markerInaccessible` を返す。どちらもアプリ再起動の選択肢を出す。マーカーの記録と資格情報の削除が両方失敗した時は `markerAndDeleteFailed`、設定ディレクトリまたはロックを置くローカルデータフォルダーを取得できない時は `markerUnavailable` を返す。保留時は `logoutPending` と必要に応じて固定の `logoutIssue` を返す。設定画面から再試行できる。
- 設定には状態、アカウント、期限（残り 14 日以下は警告）、ログイン / 取消 / ログアウトと再試行、ClientID 上書きとコールバック URL を表示する。状態の取得に失敗した時は理由と再読込ボタンを表示する。`get_twitcasting_status` / `reload_twitcasting_status` / `connect_twitcasting` / `cancel_twitcasting_login` / `disconnect_twitcasting` を使い、接続時は ClientID と待受ポートを即時保存する。接続終了後に保存済みの ClientID と待受ポートを読み直す。状態の `clientId` が保存済みの上書き値（未設定は空文字）と一致する時だけ表示する。ツイキャス側の許可解除は連携アプリ設定で行う。
- 残るリスク。Implicit のフラグメント付き URL が一瞬ブラウザ履歴に載る。待受終了後のコールバックでは接続拒否画面の URL 欄にトークンが残る場合がある。遅れた書き込みの巻き戻し前にアプリが終了するか、巻き戻しが 3 回失敗すると、取り消したトークンが次の起動で復元されうる。旧版や資格情報マネージャーなど、同じファイルロックを使わない更新は、OS ストアの照合と削除の間に入りうる。旧形式の任意マーカーが残っている間は、現在値を対象にし、新しいログインの保存を保留する。明示ログアウトで削除とマーカーの永続化が両方失敗した場合や、生の値を読めず対象を記録できなかった場合は、削除が成功する前に終了すると旧値が次回起動で復元されうる。時刻による照合は壁時計の変更やミリ秒単位の丸め、受付前に取得され受付後に保存された grant により、実際の資格情報の保存順とずれる場合がある。公式 API でのコメント受信は §4.5「公式 API 経路と切替」。

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

- **単一インスタンス**: `tauri-plugin-single-instance` を Builder の最初に登録する(デスクトップ)。2 つ目の起動は既存のメインウィンドウを表示・前面化して終了する。メインが閉じられ弾幕ウィンドウだけが残っている時は、`tauri.conf.json` の `main` 設定からメインを作り直す。プラグインの排他は Windows のログオンセッション単位。ツイキャスの資格情報とログアウトマーカーは同じユーザーの別ログオンセッションをまたいで §1.3 のファイルロックで保護する。

## 3. 統一データモデル (`model.rs`)

```rust
ChatMessage {
  id: String,                 // 内部一意ID (uuid)
  platform: Platform,         // Twitch | Youtube | X | Niconico | Twitcasting
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
Platform = Twitch | Youtube | X | Niconico | Twitcasting
```

JSON は `serde(rename_all = "camelCase")` で UI/OBS に流す。

### 3.1 配信開始/終了イベント (`LiveEvent`、X 自動告知の入力)

`ChatMessage` とは別の side channel(`mpsc`、容量 64)。UI/OBS へは流さない(JSON 化しない)。

```rust
LiveStream { platform, channel_identifier /*設定の identifier そのまま。ツイキャスだけは抽出した screen_id の小文字(URL・大文字の保存値も同じキーに揃え、原文のクエリ・フラグメントを持ち出さない)*/, stream_id, title: Option<String>,
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

### 4.5 ツイキャス (`sources/twitcasting.rs`、非公式経路。ログイン不要)
- 既定の取得経路。認証もログイン cookie も使わない(§1.3 の公式ログインとは独立)。非公式のため予告なく壊れうる。取得経路は `twitcasting.mode` で選ぶ(下の「公式 API 経路と切替」)。
- `identifier`: `extract_screen_id()` で正規化する。許可ホストは `twitcasting.tv` / `www.` / `ssl.` / `en.` / `ja.`(ポート・認証情報付きは不可)、接頭辞は `c:` `g:` `f:` `ig:`、本体は英数字と `_` の 32 文字以下。URL は `/movie/…` 等の後続パス可、`%3A` だけデコードする。`ChatMessage.channel` は小文字化した ID。解釈できなければ接続せず、チップに「ツイキャスの配信者 ID を解釈できません」を出して cancel まで待機する。
- 手順(1 セッション):
  1. `GET <userPageBaseUrl><screen_id>`(既定 `https://twitcasting.tv/`)の HTML から movie_id と `data-is-onlive` を読む。HTML コメント(閉じ忘れは末尾まで)は読み飛ばす。`data-is-onlive` は `true` / `false`(ASCII 大小無視)だけ受理し、それ以外の値はページ全体を Unreadable とする。タグの `data-broadcaster-id` と `data-broadcaster-screen-name` は、値の前後空白と ASCII の大文字小文字を無視して比較し、どちらかが一致すれば本人とする。他の配信者のタグは読み飛ばし、配信者属性が他人のものしかないページは Unreadable とする。リダイレクトは追わず、一般ブラウザの User-Agent を使う。タイムアウトは HTTP 15 秒。
  2. movie_id が前回と違う時だけ重複排除を空にし、`GET <commentHistoryUrl><screen_id>/userajax.php?c=listall&m=<movie_id>&k=0&f=0&n=20`(既定 `https://twitcasting.tv/`)で直近 20 件を取る。`type` が comment / gift のものを古い順・`type:id` 重複なしで流し、`skip_tts=true`(読み上げない)。
  3. `POST <eventPubsubUrl>`(既定 `https://twitcasting.tv/eventpubsuburl.php`、フォーム `movie_id` と `__n`=現在 ms)の JSON `url` を得る。
  4. `Started` を送り、`<url>&gift=1` へ WS 接続する(接続タイムアウト 20 秒)。接続成功でチップを配信中にする。以降のテキストフレームは JSON 配列で、要素ごとに正規化する(配列でない・JSON でないフレーム、未知の `type` は debug ログを出して捨てる)。Ping には Pong、Close は切断扱い。キャンセル時は Close を送る(待ちは 2 秒まで)。
- 重複排除: キーは `<type>:<id>`、直近 4096 件。listall と WS の重なりを落とす。同じ movie_id の WS 再接続では listall を取り直さない。
- 状態遷移(セッションの結果と次の動き):

| 結果 | 条件 | 動き |
|---|---|---|
| Offline | 視聴ページが未配信(`data-is-onlive=false`、または movie_id が無く `false` が明示) | チップを待機にし `Ended` を送る。30 秒 ±20% で HTML を再取得 |
| Unreadable | HTTP 200 だが movie_id も未配信の明示も読めない(他配信のカードしか無い場合を含む)、または読み取った is-onlive が未知値 | **`Ended` を送らず、チップも変えない**。30 秒固定で再確認。ログは初回 warn、連続時 debug |
| NotFound | 視聴ページが 3xx / 404(存在しないユーザー) | チップに「ツイキャスのユーザーが見つかりません」。5 分後に再試行 |
| Unavailable | eventpubsuburl の `url` が無い・null・空(限定配信など) | チップに「この配信は非公開のため取得できません」。30 秒 ±20% で再試行。連続時は debug |
| Recheck | eventpubsuburl が 3xx / 4xx | 30 秒固定で HTML から確認し直す |
| Disconnected | 通信失敗、HTML・eventpubsuburl の 5xx や本文不正、WS 切断・受信エラー、90 秒無フレーム | チップを待機にする(`Ended` は送らない)。指数バックオフ 1→30 秒で張り直す。バックオフは、フレームを受けたか接続 30 秒以上の後に戻す |

- 配信開始/終了(§3.1):
  - `Started` は eventpubsuburl の `url` を得た時に送る。`stream_id` は movie_id、`url` は `https://twitcasting.tv/<screen_id>/movie/<movie_id>`、title・開始時刻は無し。アプリ全体の Source で共有する直近 256 件の movie_id(送信に成功したもの)と照合し、含まれていれば送らない(照合から記録までは排他ロックで直列化)。キューへの送信成功後に記録し、上限を超えたら最も古い記録を捨てる。記録は Ended や Source の再生成後も保持する。キューが満杯なら次の接続試行で再送できる。
  - `Ended` は Offline の時だけ送る。Unreadable・Unavailable・Recheck・Disconnected・NotFound では送らない(配信状態を確認できていないため)。
- 正規化(`ChatMessage`):
  - comment: `id`=`tc-<id>`、`channel`=正規化済み ID、本文=`message`(空白だけは捨てる)。`author.id`=`author.id`(`screenName` ではない)、表示名=`author.name`(空なら `author.screenName`、それも無ければ `author.id`)、アイコン=`author.profileImage`(`//` 始まりは `https:` を補い、`https://` 以外は捨てる)。`roles.broadcaster` は非匿名で `author.id` が配信者 ID と一致(大文字小文字無視)した時。
  - gift: `kind=Gift`、`id`=`tc-gift-<id>`。送り手は `sender`(無ければ `author`)。本文は `item.name` と `score_items[0]` の `title`+`text` を半角空白で連結(どちらも無ければ「ギフト」)。金額換算はせず `amount` は付けない。他人の配信のギフトも取れる。
  - 時刻は `createdAt`(10 桁以下は秒、それ以外はミリ秒)。読めなければ受信時刻。
  - 匿名: `isAnonymous` が真、または `author.id` が `c:tw` + 数字 1 桁以上(`c:twinkle` は実ユーザー)か `twitcastinguest` なら、`author.id` を固定の `tc:anonymous` に置き換え、バッジ `anonymous`(「匿名」)を付ける(同じ ID を複数人が共有するため。ユーザー単位の NG・色分けに使わせない)。
  - 視聴者数・タイトルは取らない(チップは live の真偽と notice のみ更新)。
- 対応しない物: 公式経路でのギフト取得、合言葉・限定配信(Unavailable の文言を出すのみ)、コメント投稿、モデレーション、金額換算。
- 公式 API 経路と切替(`sources/twitcasting_official.rs` / `twitcasting_mode.rs`):
  - `twitcasting.mode`(既定 `auto`。未知値・型違いは `auto`)はチャンネルの Source を起動する時に読む。稼働中の Source は作り直さないので、変更は再起動・チャンネル再追加(次回の接続)から反映される。設定画面の「取得経路」で選び、保存は他の項目と同じ全体保存(`baseRevision` 照合、`commit_config`)だけを通す。
  - `unofficial`: 非公式だけ。トークンを使わず、公式 API へは一切切り替えない。
  - `official`: 公式 API だけ。非公式へは落とさない。未ログインなら「ツイキャスにログインしてください」を出して待ち、ログインしたら再起動なしで始める。
  - `auto`: 非公式で始める。切替の引き金に数えるのは Unreadable だけ。
    - 数える物: 視聴ページ HTML が読めない(上の Unreadable)、eventpubsuburl が 2xx なのに本文が不正(`url` の欠落は限定配信の Unavailable で数えない)、WS で最初の配列フレームより前に非配列フレームが来た(1 接続につき最大 1 回)。
    - 数えず、カウンタも戻さない物(Neutral。ネットワーク断・未配信は数えない): ネットワーク断・HTTP 3xx / 4xx / 5xx・NotFound・Unavailable・配列を受けた後の非配列フレーム・WS 切断。
    - 明示的な未配信ページと、WS の最初の配列フレームはカウンタを 0 に戻す。
    - 連続 3 回で、ログイン済みなら公式へ切り替える(info ログ 1 行、チャンネル名だけ)。未ログインなら「非公式経路が使えません。ログインすると公式 API で取得できます」を出して非公式を続け、ログインを検知したら再起動なしで公式へ切り替える。
    - 公式へ切り替えた後は戻らない(その Source が止まるまで公式のまま)。非公式からやり直すのは再起動・チャンネル再追加の時だけ(mode の変更も、保存後に再起動かチャンネル再追加をした時から効く)。
  - 公式ポーリング: `/users/:id` で存在確認、`/users/:id/current_live` で配信中か判定(未配信なら待機として再確認)、配信中は `/movies/:id/comments` を `slice_id`(確定カーソル)+ `offset`(0,50,100…、`limit=50`)で空ページまで最大 5 ページ取る。短いページでは止めない。カーソルはページ取得が正常に終わった後だけ進め、途中のエラーではその回は何も流さず同じカーソルから再試行する。非数値 ID はカーソルを動かさず重複集合(直近 4096 件)だけで落とす。movie が変われば全部リセットする。切替直後・途中参加は新しい 20 件だけを古い順に `skip_tts=true` で流す。
  - レート予算: 全チャンネルで共有する 1 本(54 回/分、ローリング)。`X-RateLimit-Remaining` が 2 以下、429、または 403 でエラーコード 2000 の時は Reset + 1 秒(1〜120 秒に丸め、Reset が無ければ 60 秒)まで全チャンネルを止める。停止期限は延ばす方向にしか更新しない。配信中のコメント poll 間隔は max(4 秒, 配信中の行数 × 2 秒)で、下限は 4 秒(1 チャンネルで予算待ちが無ければ表示遅延は最大約 4 秒。行数や予算待ちに応じて延びる)。
  - 認証エラー: 現世代のトークンへの 401 は「ツイキャスに再ログインしてください」を出して再ログインを待つ(`auto` でも非公式へは落とさない。再ログインで再開、カーソルは保持)。遅れて届いた古い世代の 401 では止まらない。その他: 2001(アプリ無効)は専用の注記で `not_found_retry` 間隔に落とし、2002(合言葉付き)は「この配信は非公開のため取得できません」でコメント取得だけ止める。5xx・ネットワーク断は指数バックオフでカーソルを保つ。
  - 公式経路中はギフトを取らない(公式のギフトは自分の配信に限られ、非公式の方が広く取れる)。視聴者数・タイトルは公式の応答から反映する。
  - 切替では、非公式で流した直近 256 件の `ChatMessage.id` を Rust 側で持ち越し、公式の最初のバッチから落とす(UI は変えない)。開始告知(`Started`)の movie_id 照合と `channel` 名は切替後も同じ。
  - 未検証(実機で確かめていない): 未配信時の `current_live` が 404 か。`slice_id` の境界・50 件超の向き・短いページの頻度。コメント ID が単調増加か。2000 応答への Reset ヘッダの有無。合言葉付き配信が 2002 か。**非公式 WS / listall のコメント ID と公式の `comment.id` が同じ番号体系か(違えば持ち越しが効かず、切替直後に最大 20 件が重複し得る)**。構造が読めない実際の HTML(ログイン壁・チャレンジ)が Unreadable に入るか。レートの実際の制限単位と、多チャンネル時に予算が伸ばす遅延の大きさ。
- ログ: WS URL にトークンが入るので、ログ・エラー文にはホスト名だけを出す(`SecretWsUrl`)。`tungstenite` と `tokio_tungstenite` およびその配下(`::` 区切り)のターゲットの DEBUG / TRACE は、`EnvFilter` ではなく出力レイヤーに付けた上限フィルタで落とす(INFO 以上は通す)。`RUST_LOG` が許す範囲を超えて引き上げない(`off` / `warn` なら WS の INFO も出ない)。HTTP 取得・本文の読み取り失敗ログでは `reqwest::Error` から URL を除く。
- 壊れうる箇所と吸収:

| 箇所 | 壊れ方 | 吸収 |
|---|---|---|
| 視聴ページの HTML | movie_id・`data-is-onlive` の属性名が変わる | `twitcastingOverrides.patterns`。読めない間は Unreadable で 30 秒ごとに再確認(Ended は出さない) |
| `eventpubsuburl.php` | URL・パラメータ変更、`url` の形が変わる | `twitcastingOverrides.endpoints.eventPubsubUrl`。形が違えば Disconnected(バックオフ)、`url` 欠落は Unavailable |
| listall(`userajax.php`) | 履歴が取れない | 履歴だけ諦めて WS に進む(warn ログ)。`endpoints.commentHistoryUrl` で差し替え |
| WS フレーム | 配列でなくなる・フィールド名変更 | 寛容に捨てる(debug ログ)。`serde_json::Value` のパス探索で、欠落は None に劣化 |

- **`config.json` の `twitcastingOverrides`(§10)で、再ビルド無しに上書きできる**。値の空文字・未設定は既定値。

## 5. Bus 層 (`bus.rs`)

- 内部: `tokio::sync::broadcast`（容量上限あり、lag は drop 容認=最新優先）。
- **UI向け**: Tauri の `app_handle.emit("chat", batch)`。**個別送出せず ~16ms(1フレーム) でまとめて配列送出**(IPC往復削減)。
- **OBS向け**: axum WebSocket `/ws`。接続クライアントへ同じ batch を push。`GET /?template=<name>` は `templates/<name>/index.html` を返す（未指定は `default`、`../` 等は拒否）。静的アセットは `/<name>/...` で配信。ポートは設定(既定 11180)で、変更時は新ポートの bind 成功後にサーバを再起動して再 bind する。
- バックプレッシャ: 各クライアントごとに bounded queue、溢れたら古いものから捨てる。
- **モデレーションイベント**(P6 Phase 3): Source → `mpsc`(容量 1024、送信は `try_send` で chat 受信をブロックしない)→ `spawn_moderation_ui_forwarder` が 16ms バッチで `emit("chat-moderation", Vec<ModerationEvent>)`。同じ forwarder で `MessageDeleted` を Moderator の hidden_ids(上限 8192 の FIFO)へ入れ、未読み上げの該当コメントを TTS キューから除く。同じ forwarder から OBS(`/ws?modEvents=1`)へも流す(§9)。

- **ローカル API**: `Bus::obs_app(..., extra)` で、パスが `/api` か `/api/` で始まる要求を既存 OBS ルータの CORS レイヤより外側のミドルウェアで丸ごと API ルータへ振り分ける(末尾スラッシュ等で旧 fallback に落とさない)。API ルータは独自の fallback を持つ。内蔵テンプレ名 `api` は予約。既存ルートは Host 検査・認証を追加しない(§15)。

## 6. TTS 層 (`tts/`) — 3バックエンドをアダプタ化

```rust
trait TtsBackend { async fn speak(&self, text: String) -> Result<()>; fn available(&self) -> bool; }
```
- **bouyomi.rs**: 棒読みちゃん。`TCP 127.0.0.1:50001` にバイナリコマンド送信(コマンド0x0001, 速度/音量/声質/トーン/文字コードUTF-8/本文)。
- **voicevox.rs**: VOICEVOX。`POST /audio_query` → `POST /synthesis` でwav取得 → 再生(UIへ送って再生 or rodio)。既定 `http://127.0.0.1:50021`。
- **webspeech**: 実再生はUI側(`speechSynthesis`)。Rustは「読み上げ対象テキスト」イベントをUIへ送るだけ。
- ルーティング: 設定で優先バックエンド選択。優先が `available()==false` なら Web Speech へフォールバック。
- 読み上げ整形: 名前/本文の読み方、URL省略、絵文字除去、長文カット等(設定可)。
- 枠ごとの上書き(`channels[].overrides`、データ形式は §10): 判定順は 全体の `backend`(親スイッチ。枠は読み上げを開けない)→ リスナーのミュートと `skip_tts` → 枠の `ttsEnabled=false` → 既存のフィルタ(`should_speak`)。枠の設定は全体を狭めるだけ。エンジンは枠で選べない。
  - 声は項目ごとに リスナー > 枠 > 全体 で解決する(`effective_voice`)。枠の声は全体で選んでいるエンジンの項目だけが効く。他のエンジンの項目は保存するが使わない。速度・音程はリスナーと全体だけ。
  - 声はキューに入れた時点で確定する(`stamp_channel_voice`)。保存後にキューに残っている分は古い声で読む。
  - UI は枠チップの右クリック「この枠の設定…」のダイアログだけ(選択肢は「全体に従う」「この枠は読まない」と声の項目)。読まない枠のチップには「読まない」の印を出す。
  - 枠を読み上げ OFF にすると、その枠の未読み上げ分をキューから捨てる(`purge_disabled_channel_tts`)。「この枠の設定」ダイアログの保存(`set_channel_overrides`)でも、設定画面の全体保存(`update_config`)でも同じ。

## 7. モデレーション (`moderation.rs`) — ローカル処理 + 配信側(P6 Phase 1)

- **NGワード/NGユーザー**(正規表現可) → マッチは UI/OBS に流さない or グレー表示。
- **ハイライトルール**(ユーザー/キーワード)→ flag 付与。
- **ローカル非表示**(手動): UIで個別コメントを隠す。
- **配信側モデレーション(P6 Phase 1・YouTube)**: コメント右クリック / ユーザーカードから、コメント削除(`LCC.` 形式 ID = API キー設定時の公式 stream 受信分のみ)・タイムアウト(1分/10分/1時間/24時間)・永久 BAN(確認ダイアログ)・BAN 解除(同セッションで行った BAN のみ)。既存の Google OAuth(`youtube.force-ssl`)を使う。実行後は該当コメントを減光 +「削除済み」「タイムアウト」「BAN済み」ラベルで残す(UI のみ)。X / niconico は対象外。
- **配信側モデレーション(P6 Phase 2・Twitch)**: 同じメニュー / ユーザーカードで、コメント削除(IRC `id` タグ = `nativeId` が true のメッセージのみ)・タイムアウト・永久 BAN・BAN 解除(**Twitch では常に表示**)。Twitch Helix を使い、認証は Device Code Flow(設定 >「Twitch モデレーション」の [Twitch にログイン] でコード表示、ブラウザは Rust 側で開く)。必要スコープは `moderator:manage:banned_users` `moderator:manage:chat_messages`。前提: Twitch 開発者コンソールの既存アプリで Device Code Grant が使えること(`credentials.twitchClientId` 必須)。新規ログインと再ログインではこれらに `channel:manage:polls` を加えて要求する。認証状態の `scopesOk` はモデレーション用 2 スコープ、`pollsScopeOk` は Polls 用スコープを個別に判定する。設定の「権限が不足しています」はモデレーション用スコープが不足した場合に表示する。モデレーション用 2 スコープだけで接続済みのアカウントも削除・BAN 等を続けられる。エラーは Rust の `ModerationError` をそのままトースト表示。
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
- **リスナー詳細**(`UserCard.svelte`): 開いた時に `listener_donation(platform, authorId)` で読み込み、「累計ギフト」の隣に累計金額を表示する。表示中のリスナーに金額付きの SuperChat / Bits / Gift が届いたら、コメントバッチにつき 1 回再取得する。換算前のミリ単位の額が大きい順、同額は通貨コード順で、最大 6 通貨と「ほか N 通貨」を表示する。円の目安は Rust が `achievements.json` の現在の `rates` と保存済みの通貨キーで計算し、画面では整数円に四捨五入する。正の有限なレートが無い通貨は換算できない通貨として併記する。換算できる通貨が無ければ円の目安は「—」。カードの「金額をリセット」と設定の「リスナー管理」にある「全員の金額をリセット」は確認後に専用 IPC を呼ぶ。金額だけを消去し、全員分の操作では消去した人数を表示する。
- **設定画面**: チャンネル追加(Twitch名 / YouTube videoId or URL)、テンプレ選択、TTS設定、NG/ハイライト編集、OBSサーバURLコピー、通知(効果音ON/OFF+音量)。各セクションはコマンドパレットからアンカースクロールで到達可能(`id="settings-*"`)。**連携設定**(`IntegrationsSettings.svelte`): 「ローカル API」(`settings-local-api`、§15)と「Webhook」(`settings-webhooks`、§16)は通常の「保存」とは独立で、操作ごとに専用コマンドで即時保存する。URL・トークン・署名シークレットは console・エラー表示に出さず、一覧は Rust が返す `urlPreview` だけを表示する。純関数は `integrations.ts`(キーワード欄の行分割・状態ラベル)。
- **テンプレ編集** (`TemplateEditor.svelte`, 実装済み): 設定画面からテンプレ選択 + 編集(`index.html` / `style.css` / `app.js`)。内蔵テンプレは閲覧のみ(書き込みは Err)で、複製してカスタム(`custom:<name>`)にしてから編集する。名前空間・複製・削除の仕様は §9.1。ライブプレビュー強化は将来。
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
- **アンケート** (`Poll.svelte`): 「ツール ▾」からコメント投票を開始・終了し、5 種類のグラフで結果を表示。OBS に同じ結果を配信する(§19)。
- **配信振り返りダッシュボード** (`Dashboard.svelte`, `Sparkline.svelte`): コメント数・視聴者推移などの集計表示。
- **タイマー/ゴール/エフェクト/マイルストーン** (`Timer.svelte`, `GoalsBar.svelte`, `Effects.svelte`, `Milestone.svelte`): 配信演出系。OBS テンプレ `timer`/`goals` と連動。
  - Goals はコメント、視聴者、高評価、リアクションを表示できる。リアクションは YouTube 絵文字リアクション累計で、YouTube のみ有効(`reactionsAvailable=false` で自動非表示)。
  - キーワードエフェクトは §9.2。アプリ内エフェクト有効時は、YouTube匿名リアクションの絵文字別増分を専用16msバッチIPCで受け、右端寄りに浮上・フェード表示する。初回pollはrolling windowの再送分なので、再接続時の重複を避けるため統計・演出とも更新しない。描画数は画面幅別の上限を設け、`prefers-reduced-motion` では演出を省略する。コメント/TTS/OBSには混ぜない。
- **マルチカラム表示** (`MultiColumnView.svelte`): チャンネル/種別ごとの複数列ビュー。
- **設定/モデレーションのポータビリティ** (`ConfigPortability.svelte`, `ModerationPortability.svelte`): 設定・NG/ハイライトのエクスポート/インポート。
- **ウィンドウ最前面ピン**: メインウィンドウを最前面固定するトグル(`core:window:allow-set-always-on-top`)。

## 9. OBS テンプレ (`templates/`)

- 各テンプレは index.html + style.css + app.js の独立した静的サイト。`/ws` に接続し、届いた batch を描画する。ユーザーが CSS を差し替えて見た目をカスタムできる。
- OBS には「ブラウザソース」で `http://127.0.0.1:11180/?template=<name>&ws=ws://127.0.0.1:11180/ws&channel=...` を指定。`template` 未指定時は `default`。OBS ポート変更時は `ws` も同じポートを指す。
- 出荷済みテンプレ(12種):

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
  | `poll` | アンケートの集計・結果(透過背景、`/poll` WS、推奨 800×600。§19) |
  | `danmaku` | ニコ生風に画面を横切る弾幕(後述 §8.1) |
  | `trophy` | 実績解除を等級別のアイコン・色で1件ずつ表示(チャット非表示、`?duration=<秒>` 既定5秒) |
  | `effects` | キーワードエフェクト(透過背景に絵文字等を CSS アニメで描く。推奨 1920x1080。§9.2) |

- 共通クエリパラメータ: `ws`(接続先), `channel`(フィルタ), `only=gift`(投げ銭のみ) 等。テンプレ間で `max` の意味が異なる点に注意 — 通常overlayでは「表示行数の上限」、`danmaku` では「同時アニメDOM数の上限(MAX_ACTIVE)」。`danmaku` 専用: `dur`(横断秒数)/`size`(文字px)/`opacity`/`name`(名前前置)/`outline`(縁取り)/`area`(`full`/`top`/`bottom` 表示縦帯)/`coalesce`(連投まとめ ×N、`0`で無効)/`pin`(投げ銭を上部固定強調、`0`で無効)。弾幕URL生成(`Settings.svelte` の `withDanmaku()`)は弾幕に無関係なパラメータ(`max`/`ttl`/`font`/`bg`/`pos`/`icon`)を allowlist で除外する。
- システム種別(`kind==='system'` 接続通知等)は配信画面に流さない(全テンプレで除外)。
- **削除・BAN の反映(P6 3b)**: `/ws?modEvents=1`(`1` / `true` のみ opt-in。`0`・値なし・未指定は opt-out で購読もしない)のクライアントにだけ、同じソケットで chat 配列の後に `{"moderation":[ModerationEvent…]}` を 16ms tick 単位で送る(channel フィルタも適用)。opt-out の改造テンプレには一切届かない。
- **実績解除(PR3)**: `/ws?achievementEvents=1`(判定は `modEvents` と同じ)のクライアントにだけ、chat 配列の後に `{"achievements":[AchievementUnlocked…]}` を 16ms tick 単位で送る。解除には channel が無いので channel フィルタは掛けない。内蔵 `trophy` がこれを使う。
- **実績の新規作成(設定画面 `AchievementsSettings.svelte`)**: 「実績を追加」は今のタブの対象で IPC `new_achievement(subject)` を呼ぶ。id は Rust が UUID v4 で振り、`builtinId=null`・`edited=false`・ブロンズ・有効・名前「新しい実績」・`announce.tts=false`、条件は schema の先頭の指標・期間・通貨モード(しきい値 1、プラットフォーム未選択)で、`validate_for_save` を通る。返すだけで保存せず、画面は複製と同じくドラフトへ足して即編集状態にする(保存まで永続化されず、破棄で消える)。プラチナは作れず、`builtinId=null` なので削除できる。
- **通貨記号の上書き(`symbolOverrides`)**: レート表の下の行編集欄(記号 → 通貨コード)。保存は `commit_achievements` に編集中の行を渡す(破棄で元に戻る)。Rust の `normalize_symbol_overrides` が commit で検証する: 記号・コードとも trim して ASCII 大文字化して保存、空の記号・コードは不可、コードは英大文字 3 字(`normalize_rates` と同じ基準)、正規化後の記号の重複は不可、英字 3 字の記号(ISO 形式)の新規追加・変更は不可(保存済みで変わっていない物は通す。実行時は使われない)、記号は 16 文字・件数は 64 件まで。違反は `Invalid { id: "", field: "symbolOverrides" }`(入力値は返さない)。UI の `checkRateRows` / `checkSymbolRows` はキー・コードを変形せず(trim・大文字化もしない)にそのまま送り、全項目が空文字の行の除外と行番号付きの事前チェック(空欄、重複。重複は記号の上書き欄では ASCII の大小文字だけを無視して比べ、レート表では完全一致のみ — 非 ISO の記号の大小違いは Rust が別のキーとして扱うため)だけを行う。Rust の trim(`trim_currency`)は Unicode の White_Space(U+0085・U+3000 を含む)と BOM(U+FEFF)を落とす。投げ銭とレート表のキーの正規化(`normalize_currency`)は、trim した表記の完全一致の上書きを優先し、無ければ ASCII の大小文字を無視して引く(小文字の `pt` にも `PT` の上書きが効く)。大小文字を無視した候補の行き先が食い違う時(既知の記号 `US$` などだけ標準のコードにし、それ以外は表記のまま)、表記が ISO 形式(`brl` など)の時、行き先が ISO 形式でない時は上書きを使わないので、正規化は冪等(保存し直してもレートのキーが変わらない)。永続フォーマット(`achievements.json`)は変えない。
  - 送るイベント: 配信側(§7 Phase 3)、アプリ発の `moderate_delete_message` / `moderate_ban_user` 成功時(moderation forwarder 経由で UI・TTS と同じ経路。BAN は `certain`=true・`beforeMs`=null)、ローカル非表示 `hide_message`(`platform`/`channel` 付きの時のみ messageDeleted を OBS へだけ。UI へは送らない)。解除(unhide / unban)は OBS へ戻さない。
  - 同梱テンプレ `default` / `simple` / `bubble` / `ticker` / `donation` / `danmaku` が対応し、該当コメントを DOM から完全に除く(減光しない)。messageDeleted は ID、userCleared は author + `beforeMs`(`certain` に関係なく除く)、chatCleared は `beforeMs` 以前。`donation` の合計、`danmaku` の集約弾幕(×N)は要素ごと除く・合計は戻さない。
  - `ranking` / `goals` / `timer` は集計表示で本文を出さないため対象外(接続 URL も opt-in しない)。
  - 後着対策: 各テンプレが直近イベント(最大 512 件、記録から 10 秒、`performance.now()`)を覚え、後から届いたコメントを表示しない。テンプレは単体の静的サイトなので共有ファイルにせず、同一ブロック(`fc-moderation:begin`〜`end`)を各 `app.js` に複製し、`templates/obsModeration.test.mjs` が一致と挙動を検証する。
- **コメント要素の data-* 属性(自作 CSS 用)**: コメント要素を持つ 6 テンプレが、要素を作った直後に `fcApplyAttrs(el, msg)` で次の 6 属性を付ける。値は `setAttribute` で渡し(HTML 文字列に連結しない)、`msg` が null なら 6 属性すべてを `removeAttribute` する。
  - `data-user` / `data-owner` はわんコメの属性名に合わせた。`data-service` / `data-kind` / `data-role` / `data-user-id` は fast-comment 独自の語彙で、わんコメ用 CSS がそのまま効くとは限らない(`data-service` の値 `niconama` はわんコメ側での裏取り未了)。

  | 属性 | 値の語彙 | 値が無い時 | 由来 |
  |---|---|---|---|
  | `data-service` | `twitch` / `youtube` / `x` / `niconama`(`msg.platform` のまま。niconico だけ `niconama` に写す) | 常に付く | 独自(わんコメの属性名に合わせたが値は未裏取り) |
  | `data-user` | 表示名 `author.name` | 属性を外す | わんコメ |
  | `data-owner` | `true` / `false`(`author.roles.broadcaster`) | 常に付く(`false`) | わんコメ |
  | `data-user-id` | `author.id` | 属性を外す | 独自 |
  | `data-kind` | `normal` / `superchat` / `membership` / `bits` / `gift` / `system`(kind を小文字化) | 常に付く(既定 `normal`) | 独自 |
  | `data-role` | `broadcaster` `moderator` `member` `subscriber` `vip` を、この固定順で空白区切り | 属性を外す | 独自 |

  - 付く要素: `default` / `simple` は `div.comment`、`bubble` は `div.bubble-card`、`donation` は `article.donation-card`、`danmaku` は `div.danmaku-item` と `div.danmaku-pin`、`ticker` は `#ticker-text`。
  - `danmaku` の連投まとめ(×N)は先頭 1 件の値のまま(別の人の同文コメントにも先頭の `data-user` / `data-role` が付く)。`ticker` は要素 1 個を使い回すので、切り替えのたびに付け直し、キューが空なら null を渡して全属性を外す。`ranking` / `goals` / `timer` は対象外。
  - CSS 例: `.comment[data-role~="member"] { color: #4caf50; }` / `.comment[data-user='名前'] { font-weight: bold; }` / `[data-service="niconama"] { ... }`。
  - 各 `app.js` が同一ブロック(`fc-attrs:begin`〜`end`)を複製し、`templates/obsAttrs.test.mjs` が一致と挙動を検証する(`package.json` の test に登録済み)。

### 9.1 テンプレの名前空間(内蔵 / カスタム)

実装は `src-tauri/src/templates.rs`(`TemplateRef` / `TemplateStore`)。`bus.rs` の配信と `lib.rs` の `list_templates` / `read_template_file` / `write_template_file` / `duplicate_template` / `delete_template` はここへ委譲する。

| | 内蔵 | カスタム |
|---|---|---|
| ID | `<name>` | `custom:<name>` |
| 保存先 | リソースの `templates/<name>/`(読み取り専用) | `app_config_dir/templates/<name>/` |
| 配信 | `/?template=<name>` と `/<name>/...`(従来どおり) | `/?template=custom:<name>` と `/custom/<name>/...` |
| 編集・削除 | 不可(`write_template_file` / `delete_template` は Err) | 可 |

- **ID の文法**: `<name>` は ASCII 英数・`-`・`_`、1〜64 バイト(`is_valid_template_name`)。`custom:` 接頭辞があればカスタム、無ければ内蔵。内蔵名 `custom` は不可(配信パス `/custom/` と衝突するため)。文法に合わない ID は `/` が 400、文法は合うがカスタムが存在しなければ 404。内蔵の URL とルーティングは変えていないので、既存の OBS ソースはそのまま動く。カスタム `custom:default` と内蔵 `default` は別物で共存する(カスタムが内蔵を隠す経路は無い)。
- **配信**: `/?template=custom:<name>` は `<base href="/custom/<name>/">` を注入した index.html を返す。`/custom/<name>/...` は `ServeDir` で静的配信する。URL 上の `:` は `custom%3Afoo` とエンコードされても同じに解決される。
- **カスタムの無効化**: 内蔵ルートとカスタムルートが同一または入れ子の時(`app_config_dir` の取得失敗で cwd に落ちた場合など)はカスタム全体を無効にする(一覧に出さず、`/custom` も配信せず、作成・削除・書き込みは Err)。リポジトリの `templates/` を誤って消さないため。
- **新規名の規則**(`validate_new_name`、複製先の名前): 1〜64 バイト、先頭は小文字英字か数字、使える文字は小文字英字・数字・`-`・`_`。Windows 予約デバイス名(`con` / `prn` / `aux` / `nul` / `com1`〜`com9` / `lpt1`〜`lpt9`)は不可。既存のカスタム名とは大文字小文字を区別せず重複不可。
- **複製**(`duplicate_template(source, name)`): `source`(内蔵・カスタムとも可)を `app_config_dir/templates-staging/<uuid>/` へコピーし、`fc-template.json` を書いてから `app_config_dir/templates/<name>/` へ rename で公開する。失敗時は staging を消す。起動時に残った staging は `cleanup_stale_staging` で掃除する。
  - コピーはシンボリックリンク・許可外の名前・許可外の拡張子・深さ 4 超を黙って飛ばす。許可拡張子は html / css / js / json / png jpg jpeg gif webp svg / woff woff2 ttf otf / mp3 ogg wav / txt / md。`fc-template.json` はコピーしない。
  - 上限は 1 ファイル 100 MiB・合計 100 MiB・500 エントリ(zip 取り込みと同じ値)で、超えると失敗する。`index.html` が無い複製も失敗する。
- **削除**(`delete_template(id)`): カスタムのみ。ディレクトリごと消す。内蔵は Err(「内蔵テンプレートは読み取り専用です」)。
- **編集**: `read_template_file` / `write_template_file` が扱うのは `index.html` / `style.css` / `app.js` の 3 つだけ(1MiB 以下)。書き込みは同一ディレクトリの一時ファイルに書いてから rename で置き換える。
- **パスの安全**: ID をパスへ直接連結しない(必ず `TemplateRef::parse` を通す)。編集・複製・削除では、テンプレのディレクトリとファイルを `symlink_metadata` で通常の実体と確かめ、シンボリックリンクは拒否する(複製ではスキップ)。**OBS への配信(`/custom/...` の `ServeDir` と index.html の読み込み)はリンクを検査しない**ので、配信ルートにシンボリックリンクを置かない運用が前提(置けるのはその PC を操作できる人だけ)。`:` は Windows のファイル名に使えないので、ID を誤ってパスに連結しても解決されず失敗する。
- **`fc-template.json`(由来メタデータ)**: 複製時にアプリが書く。ユーザーが編集するファイルではない。

  | キー | 型 | 内容 |
  |---|---|---|
  | `schema` | 数値 | 現在 `1` |
  | `origin` | 文字列 | 作成経路。複製は `"duplicate"` |
  | `source` | 文字列 | 複製元の ID(例 `"default"` / `"custom:foo"`) |
  | `appVersion` | 文字列 | 作成時のアプリ版(`CARGO_PKG_VERSION`) |
  | `createdAtMs` | 数値 | 作成時刻(UNIX エポックからのミリ秒) |

- **zip 取り込み**(`import_template_zip`、実装は `src-tauri/src/template_import.rs`): UI は `<input type="file" accept=".zip">` で読んだバイト列を raw body の IPC で渡し、名前はヘッダ `x-template-name`。Rust は UI が指定したパスを開かない。成功時は `{ template, files, skipped }`、失敗時は日本語のエラー文字列(UI はそのまま表示)。UI は 100 MiB 超なら読み込む前にエラーにし、IPC を呼ばない。取り込み前に「テンプレートの JavaScript は配信画面で実行される。信頼できる配布元の zip だけ取り込む」旨を確認する。名前の既定値は zip のファイル名を小文字化し `[a-z0-9_-]` 以外を `-` にした物(新規名の規則は複製と同じ)。プレビュー iframe は `sandbox="allow-scripts allow-same-origin"`。
  - **取り込めるのは fast-comment 形式**(`/ws` に接続する静的サイト、ルートに `index.html`)だけ。わんコメのテンプレ zip(OneSDK 前提)はそのままでは動かない。わんコメとの互換は CSS の data 属性(上の「コメント要素の data-* 属性」)だけで、OneSDK 互換層は作らない。
  - **上限**(サイズはヘッダを信じず、実際に読んだバイト数で数える): zip 本体 100 MiB、展開後の合計 100 MiB、1 ファイル 100 MiB、編集対象 3 ファイル(`index.html` / `style.css` / `app.js`)は 1 MiB、500 エントリ、深さ 4。エントリ数は「黙って無視する物とディレクトリエントリを除いた件数」で数える(`index.html` 込みで 500 ちょうどは成功、501 は拒否)。中央ディレクトリの総件数が 5000 を超える zip は、個別の検証に入る前に拒否する。
  - **許可拡張子**: html css js json png jpg jpeg gif webp svg woff woff2 ttf otf mp3 ogg wav txt md mp4 webm。
  - **全体を拒否**: `..`、絶対パス、ドライブ指定(`:`)、バックスラッシュ、NUL、シンボリックリンク、暗号化(ヘッダだけ見て「暗号化された zip は取り込めません」と専用文言で拒否。展開はしない)、通常ファイル以外(この種別チェックは OS ゴミ判定より先に行うので、`__MACOSX/` や `.DS_Store` 名のシンボリックリンクも全体拒否)、大文字小文字を無視して重複するパス(フォルダ名の違い `Assets/` と `assets/` も重複)、上限超過、ルートに `index.html` が無い、名前が不正か既存(上書きしない)。
  - **スキップして報告**(`skipped` に出る): 許可外の拡張子、拡張子なし(LICENSE 等)、先頭または末尾が `.` の名前、`[A-Za-z0-9._-]` 以外を含む名前、Windows デバイス名。UI は件数とファイル名を表示する。
  - **黙って無視**: `__MACOSX/`、`.DS_Store`、`Thumbs.db`、`desktop.ini`、zip 内の `fc-template.json`(来歴は取り込み時にアプリが書く。`origin` は `"zip"`)。
  - **大文字小文字**: 編集対象(`index.html` / `style.css` / `app.js`)の判定は ASCII の大文字小文字を無視する(`APP.JS` も 1 MiB 上限、ルートの `INDEX.HTML` も `index.html` として存在扱い)。展開時は正規名(小文字)で書く。
  - **UTF-8 でない名前**: UTF-8 でないファイル名に `\` が混ざって拒否される場合、「ファイル名が UTF-8 でない zip の可能性があります(Windows 標準の圧縮で日本語名を含む等)」の趣旨を文言に足す(拒否そのものは変えない)。
  - **エラー文言**: staging の絶対パスは出さない(zip 内の相対パスだけ)。
  - **最上位フォルダ**: 最上位が単一フォルダなら 1 段はがして展開する。`zip` crate の `extract()` は使わず、1 エントリずつ検証して staging へ書き、成功時だけ rename で公開する。
  - **CSP を付けない理由と信頼境界**: カスタムのテンプレの JS は `127.0.0.1` のオリジンで実行され、`/ws` のチャットを読める。ただし OBS サーバは CORS 全許可で、任意の Web ページも同じ `/ws` を読める。取り込んだ JS が届く範囲はそれと同じで、CSP で守れる物がほぼ無い(画像 URL 経由の送信も防げない)。一方で CSP を付けると Google Fonts・CDN・インラインスクリプトを使う自作テンプレが壊れる。よって CSP は付けず、**信頼できる配布元の zip だけ取り込む**運用とする。プレビュー iframe の sandbox は既存テンプレのプレビュー(WebSocket・音声)が動くことを実機で確かめる(壊れたら外し、ここに書く)。
- **移行の注意(リリースノート用)**: これまで内蔵テンプレを直接編集していた分は引き継がれない。内蔵は読み取り専用になったので、複製してから編集する。複製したカスタムは、以後の内蔵テンプレの修正(バグ修正・新機能)を受け取らない。


### 9.2 キーワードエフェクト(`keyword_effects.rs`)

コメント本文に規則のキーワードが含まれたら、アプリ内と OBS(内蔵 `effects`)へ演出を出す。判定・クールダウン・除外は Rust だけが行い、UI(`Effects.svelte`)と `effects` テンプレは受け取った物を描くだけ。

- **規則(`config.json` の `effects.rules[]`、`EffectRule`)**: `keyword` / `emoji`(表示物。絵文字以外の短い文字でもよい。画像は内蔵テンプレートの対象外。カスタムテンプレートなら zip 内の画像を使える)/ `enabled`(既定 true。false の規則は発火しない。「テスト」は発火する)/ `count` / `target` / `animation` / `durationMs` / `cooldownMs`。旧 config(`keyword`・`emoji`・`count` だけ)は足りない値を既定で補って読み込み、`target="app"` として従来どおり動く。キーワードは読み込み時に trim する。旧 config の `count: 0`(旧 UI での無効化)は `enabled=false`・`count=1` として読む。新しい版で保存した config を旧版に戻して読むと `enabled` は無視される(更新は新しい版へだけ進む前提で、戻した時の互換は保証しない)。
  - `target`: `app` | `obs` | `both`、既定 `app`(未知の値も `app`)。
  - `count`: 1〜50(既定 12)。`durationMs`: 500〜15000(既定 3000、OBS のみ)。`cooldownMs`: 0〜60000(既定 2000)。範囲外は `normalize_all`(ロード・保存時)と発火時に丸める。`emoji` は 16 文字まで。
  - `animation`(OBS のみ。未知の値は `float`): `rain`(降る)・`burst`(弾ける)・`float`(浮かぶ)・`bounce`(跳ねる)・`spin`(回る)・`big`(中央に大きく)。CSS アニメだけで描く。
  - 表示物かキーワードが空(空白のみ含む)の規則は無効。
- **判定**: `moderator` の Hide 除外の後(パイプライン)。対象外は System・`injected_test`・履歴/バックフィル(`route.uncounted`、効果音と同じ基準)・`effects.enabled=false`。キーワードは `regex::escape` + 大文字小文字無視の部分一致で、`text_fragments` ごとに照合(エモート名・著者名は対象外、断片をまたぐ一致はしない。Webhook と共用の `webhook::rules::keyword_regex` / `text_matches`)。1 コメントが複数の規則に当たれば規則ごとに 1 回、同じ規則は 1 コメントで最大 1 回。
- **クールダウン**: 規則ごと、`Instant`(単調時計)で管理。発火した時刻から `cooldownMs` 未満の一致は捨てる(溜めない・窓も延ばさない)。`target` に関係なく共通。規則を編集しても、キーワード(trim + 小文字)が同じ規則はクールダウンを引き継ぐ(同じキーワードの規則が複数ある時は、その中で最後に発火した時刻をどれにも引き継ぐ)。キーワードを変えた規則・新しい規則だけがリセットされる。「テスト」はクールダウンを無視する。
- **配信**: 発火は Bus の `EffectTriggered { ruleId(規則の index), emoji, count, animation, durationMs }` になる(`target` は配信先の振り分け専用でシリアライズしない)。chat の発行の後に流す。
  - OBS: `/ws?effectEvents=1`(`1` / `true` のみ opt-in。判定は `modEvents` と同じ)のクライアントにだけ、`target` が `obs` / `both` の物を chat 配列の後に `{"effects":[EffectTriggered…]}` として 16ms tick 単位で送る(1 tick 1 フレーム、chat のバッチは分割しない、channel フィルタなし、原因の chat の後になるよう 1 tick 保留)。内蔵 `effects` がこれだけを使う。
  - アプリ: `target` が `app` / `both` の物を 16ms バッチで `emit("keyword-effects", [...])`。UI は判定をせず描画のみ(見た目は従来どおり、`prefers-reduced-motion` では省略)。
- **`effects` テンプレ**: 背景透過。1 件につき `count` 個の要素をプリセットの CSS アニメで描き、`delay + 描画時間 = durationMs` 以内に DOM から消す。同時表示は 300 要素まで(超えたら古い物から消す)。再接続は `trophy` と同じ(指数バックオフ 1〜16 秒)。純関数は `fc-effects:begin`〜`end` に置き `templates/effects.test.mjs` が検証する。
- **設定 UI**: 設定の「キーワードエフェクト」節で規則ごとに キーワード・表示物・個数・表示先・アニメ・表示時間・間隔 を編集し、「テスト」(IPC `test_keyword_effect(index, rule)`、保存前の内容でクールダウン無視、`target` どおり OBS とアプリへ)で 1 回流せる。同節に `effects` テンプレの OBS URL(推奨 1920x1080)を載せる。

## 10. 設定永続化 (`config.rs`)

- 保存先: Tauri の app config dir に `config.json`。
- リスナー記録は同じディレクトリの `listeners.json` に保存する。各記録の `donation` は通貨コードからミリ単位の整数へのマップで、欄が無い旧データは空として読む。値が非整数、i64 の範囲外、または負の項目は読み込み時に捨て、有効な項目とほかのリスナー記録を保持する。実績の有効・無効にかかわらず、SuperChat / Bits / Gift の有限かつ正の金額を 1,000 倍して四捨五入し、通貨別に飽和加算する。件数の `SUBGIFT` と `route.uncounted` が付いた履歴などの再送は除外する。通貨キーは集計時点の保存済み `symbolOverrides` を優先し、該当する上書きが無ければ既定の正規化を使う。円の換算値は表示時だけ計算する。リセットは既存の flush で保存し、ストアの終了処理後は受け付けない。リスナー削除時は金額も消える。
- ローカル API と Webhook は別の `integrations.json`(version 1、localApi.enabled 既定 false、webhooks 既定空)に保存。`AppConfig` / `update_config` / エクスポートには載せない。破損・非対応 version は `.corrupt-<時刻ms>` に退避。読み込みエラー(退避失敗を含む)時は保存禁止とし、変更系は FileWrite で拒否する。再起動後に正常に読み込めるまで元ファイルを保護する。同じディレクトリに create_new の一時ファイルを作成し、write → sync_all → rename で保存。
- API トークンは OS 資格情報ストア(service `fast-comment`、key `local-api-token`)に `{token, createdAtMs}` として保存。無効化しても削除しない。
- 内容: channels[], obs{port}, tts{backend, options}, moderation{ngWords[], ngUsers[], highlights[]}, ui{maxBuffer, notifySound, notifyVolume}, youtubeOverrides{apiKey?, clientVersion?, paths?}, credentials{youtubeApiKey?, ...}。
  - `credentials.youtubeApiKey`(空文字既定): 公式YouTube streamList用。空ならInnerTubeのみ。
  - `ui.notifySound`(bool 既定false) / `ui.notifyVolume`(f32 0.0〜1.0 既定0.5): キーワード通知の効果音設定。serde default でキー欠落の旧 config も後方互換。
  - `obs`: ポートに加え `maxRows` / `fontScalePct` / `ttlMs` / `bgOpacityPct` / `position` 等の見た目設定。範囲外値が入らないよう **Rust 側でも `normalize()` で clamp**(`maxRows` 1..=1000, `fontScalePct` 50..=200, `bgOpacityPct` 0..=100, `ttlMs` 500..=600000, `position`∈{top,bottom})。`normalize()` は config ロード時と `update_config` 適用時の両方で呼ぶ。上限定数は Rust 側に1箇所(`MAX_OBS_ROWS`)を置き UI/テンプレと値を揃える。
  - `channels[].overrides{ttsEnabled?, ttsVoice?{voicevoxSpeaker?, bouyomiVoice?, webSpeechVoice?}}`: 枠ごとの読み上げの上書き(解決順は §6)。空なら JSON から省略し、旧 config も読める。`ttsEnabled` は `false`(この枠は読まない)だけ保存し、`true` は `normalize()` で捨てる(「全体に従う」と同じ)。`voicevoxSpeaker` は u32、`bouyomiVoice` は i16、`webSpeechVoice` は前後空白を除き 200 文字まで(空白と不可視文字だけなら無し)。全項目が無い `ttsVoice` は捨てる。ロード・`update_config`・`set_channel_overrides`・`add_channel` のどれも同じ `normalize()` を通す。
  - `channels[].ownStream`(bool 既定false): 「自分の配信」の印(X 自動告知の対象)。旧 config は欠落で false。
  - `xPost{enabled=false, mode="intent"|"api"="intent", intentOpen="button"|"auto"="button", combine=false, template, delaySecs=60, staleMinutes=15, cooldownMinutes=60, dailyLimit=5, callbackPort=47821}`: X 自動告知。`template` 既定は `{title}\n{url}\n{date} {time}`。全項目 serde default。`normalize_all()` で clamp(`delaySecs` 0..=600, `staleMinutes` 0..=180, `cooldownMinutes` 10..=1440, `dailyLimit` 1..=20、`mode` / `intentOpen` は不正値なら既定)。
  - `twitcasting{callbackPort=39822, mode="auto"|"unofficial"|"official"="auto"}`: ツイキャスのログイン待受ポート(§1.3)と取得経路(§4.5。チャンネルの Source を起動する時に読み、稼働中は反映しない)。serde default で旧 config.json も読める。`xPost.callbackPort` と同じく、設定画面と接続コマンドは 1024〜65535 の整数を受け付ける。設定ロードと `update_config` では 1024 未満を各サービスの既定値に戻す。設定画面のログイン(`connect_twitcasting`)が `credentials.twitcastingClientId` と一緒にこの値だけを保存し、待受と status の `callbackUrl` に使う。
  - `twitcastingOverrides{endpoints?, patterns?}`: ツイキャス非公式経路(§4.5)の上書き。両方とも文字列→文字列のマップで、全項目が既定値(未指定・空文字・前後空白のみは既定を使用)。値が既定(両マップが空)なら JSON から省略し、旧 config.json も読める。チャンネルの Source を起動する時に読む。 保存は他の項目と同じく全体保存(`update_config` の `baseRevision` 照合)とインポートだけで、`commit_config` を通って revision を進める。Source が書き戻すことはない。公式 API の送信先(Bearer を載せる先)は設定で変えられず、この上書きは非公式経路にだけ効く。
    - `endpoints`: `userPageBaseUrl`(既定 `https://twitcasting.tv/`、末尾に配信者 ID を連結)/ `eventPubsubUrl`(既定 `https://twitcasting.tv/eventpubsuburl.php`)/ `commentHistoryUrl`(既定 `https://twitcasting.tv/`、`<配信者 ID>/userajax.php` を連結)。
    - `patterns`(正規表現。キャプチャ 1 番目を使う): `movieId`(既定 `data-movie-id="(\d+)"`)/ `movieIdFallback`(既定 `var\s+movieid\s*=\s*"(\d+)"`、`movieId` で取れない時に試す)/ `isOnlive`(既定 `data-is-onlive="([^"]*)"`、`true` / `false` を ASCII の大文字小文字を区別せず判定し、未知値は Unreadable)。不正な正規表現は warn ログを出して既定へ戻す。
  - `credentials.twitcastingClientId`(任意): ツイキャス公開 ClientID の上書き。未設定は JSON から省略し、空欄なら既定値を使用する。トークンは資格情報一覧の `twitcasting-oauth-token` にのみ保存する(§1.3)。
  - `credentials.xPostClientId`(任意): API モード用の X OAuth 2.0 Client ID。未設定なら JSON に出さない。設定画面の「X に接続」(`connect_x_post`)が `xPost.callbackPort` と一緒にこの 2 項目だけを保存する。
  - `xOverrides.endpoints` の `postAuthorizeUrl` / `postTokenUrl` / `postTweetsUrl` / `postUsersMeUrl`: API モードの X エンドポイントの上書き(起動時に読む。変えたらアプリを再起動)。
  - X 投稿の認証は `config.json` に置かない: OS 資格情報ストア(`secret_store`)のエントリ `x-post-oauth-token` に `{ clientId, refreshToken, userId, username }`。access token はメモリのみ。
  - 台帳 `<config_dir>/x-autopost-ledger.json`(§13)。
- 起動時ロード、変更時保存。`config.json` をアプリ設定の正本とする。読込時は JSON の末尾まで検証する。設定の型エラーは固定の項目名で示し、入力値や任意のマップキーをログ・画面に出さない。インポート後は Rust の正規化済み設定をフォームに反映する。ブラウザでは外観設定を反映できる。
- 全体保存（インポートを含む）では、X とツイキャスそれぞれの接続コマンドの待機中に、そのサービスの ClientID と待受ポートを現在の値のまま保存する。保持した項目は `protectedOAuth` で返す。UI は応答の正規化済み設定全体でフォームを置き換えるので、保持した値がそのまま入る。接続コマンドに無効なポートを渡した時のエラーは入力値を含まない。
- 設定の revision: Rust は `AppState.config_revision`(u64、プロセス内で単調増加、永続化しない)を持つ。設定の保存は `config::commit_config` だけが行い、設定のロック内でディスク保存に成功した時だけメモリ・watch と一緒に revision を 1 進める。`get_config` は `{ config, revision }` を返す。既知の制限: `AppConfig::save` は受け入れテストが使うため公開のままで、クレート内の迂回保存は lib.rs のソース検査テスト(`src` 全体の `config_tx.send*`・`AppConfig::save(`・設定ディレクトリを渡す `.save(`)で防ぐ。
- 書込 IPC は保存後の `revision`(保存しなかった時は null)と、書いた部分の正規化済みの値を返す。`update_config` → `{ warnings, protectedOAuth, revision, config }`(正規化後の設定全体)、`set_goals_show_in_app` → `{ revision, showInApp }`、`set_moderation` → `{ revision, moderation }`、`append_moderation_value` → `{ revision, value, moderation }`、`add_channel` / `remove_channel` / `set_channel_own_stream` / `set_channel_overrides` → `{ revision, channels }`。保存が応答より前に起きる `connect_twitcasting` / `connect_x_post`(`{ revision, clientId, callbackPort }`)、`connect_youtube_oauth`(`{ revision, youtubeOauthClientId }`、ClientID が変わった時だけ保存)、`disconnect_x_post`(API モードを戻した時 `{ revision, xPost: { mode } }`)は引数 `onConfigSaved` の Channel で送る。
- 枠の上書きの保存: `set_channel_overrides(key, overrides, baseRevision)` は基準 revision が必須で、現在の revision と違えば保存せず上の固定文言で拒否する(`commit_channel_overrides`)。該当する枠が無ければ「チャンネルが見つかりません」。実行中の Source は止めず、読み上げ OFF なら未読み上げ分を捨てる。エラー文に入力値を出さない。
- `add_channel` の `replaces`(key 1 件または配列): 該当行をすべて外して新しい行を 1 回の保存で入れる(原子的。1 件でも見つからなければ何も変えない)。X・ニコニコの行置き換えでは、新しい行の `overrides` が空なら置き換え前の行のものを引き継ぐ(`ownStream` は引き継がない)。
- 起動時に `config.json` を読めない時は、元ファイルを `config.json.broken-<unix ms>`(既存なら `-1`、`-2`…。既存の退避は上書きしない)へ複製して既定値で続行する(`load_or_preserve`)。退避にも失敗したら保存を停止し、保存系の IPC は固定文(`CONFIG_SAVE_BLOCKED_ERROR`)で拒否して元ファイルを守る。
- 全体保存の楽観的排他: 設定画面の「設定を保存」は `update_config(newConfig, baseRevision)` を送る。`update_config` は `newConfig` を JSON 値で受け、型変換を含む他の検証(API モードの接続確認など)より先に `baseRevision` を照合し、現在の revision と違えば保存せず固定文言「設定が別の操作で変更されました。読み直してから保存してください」で拒否する。設定全体のインポートは意図的な上書きなので `baseRevision` を送らない(ログイン中の OAuth 項目の保持は上記のとおり)。部分更新の IPC は最新設定へのパッチなので `baseRevision` を取らない。
- 画面(設定画面・コメント画面)が基準 revision を進めるのは (a) `get_config` の応答でフォーム/状態を丸ごと置き換えた時、(b) 自分が呼んだ書込 IPC の応答の revision が基準 + 1 ちょうどの時(書いた部分を応答の正規化済みの値で反映する。全体保存では応答の設定全体でフォームを置き換える)だけ。それ以外(基準 + 2 以上、応答待ちの間に別の書込があった)は再読込を求める。基準は単調増加で、応答(`get_config`・書込の応答・Channel)の revision が基準以下なら何も反映しない(全体読込で基準と同じ revision を読めた時は再読込要求だけ下ろす)。部分読込が基準より新しい設定を返したら、その項目を表示に反映して再読込を求める。自分の書込・再読込の間は、それが書き換える欄を無効にし、全体保存中はフォーム全体を無効にする。接続系コマンドの `onConfigSaved` は、そのコマンドが settle した後に届いたら反映せず、基準より新しければ再読込を求める。ログインが失敗・取消で終わった時は ClientID 欄を保存済みの値に戻す(全体保存は ClientID 欄の未保存の入力値を使わず、保存済みの ID を送る。待受ポートは全体保存で送るので入力値を残す)。他の画面の書込は設定画面へ知らせず、古いフォームの保存は Rust の拒否に任せる。拒否されたら「設定を読み直してください」と再読込ボタンを出し、設定全体の再読込に成功するまで通常保存を止める。
- ログイン終了後、手動の「設定を再読み込み」、モデレーション規則の取込後、設定全体のインポート後は、共通の関数で保存済み設定を読み直す。10 秒で打ち切る。応答の revision が基準以下なら反映しない(下記)。読込中は対象の入力欄と通常保存を無効にする。ログインの ClientID・待受ポート欄と通常保存はログイン開始時から無効にする。ツイキャスと X のログイン取消は設定のインポートや再読込中も操作できる。
- 読込成功時は対象項目だけをフォームに反映する。ログイン後は該当サービスの ClientID と待受ポート、規則取込後は `moderation`、手動再読込と設定全体のインポート後は設定全体を反映する。失敗・期限切れ時は入力欄を解放し、設定全体の再読込に成功するまで全体保存を止める。設定画面の破棄後に完了した読込では何もしない。
- コメント画面は読んだ設定の revision を持ち、インポートなどで新しい revision を知ったら設定を読み直す。読み直すまで「コメビュに表示」を無効にする。読込に失敗したら(Tauri の外で読めない時を含む)1・2・4 秒あけて再試行し、それでも失敗したら止めて「設定を読み直してください」を出す。全体読込の成功、設定画面からの反映、新しい revision を知った時に再試行の回数を戻す。
- 接続と取消には UI が発行した試行 ID を渡す。設定画面を閉じると進行中の試行を取り消す。取消は対応する試行だけに効く。進行中の試行は重複開始を拒否する。開始前に取り消した試行と終了済みの試行は、サービスごとに直近 32 件を保持し、その間の再利用を拒否する。
- コメント画面の「コメビュに表示」は `set_goals_show_in_app` で保存済み設定の `goals.showInApp` だけを更新する。
- コメント画面の NG 追加は `append_moderation_value(kind, value)`、規則 JSON のインポートは `set_moderation(moderation)` を使う。Rust 側で保存を直列化し、最新設定の `moderation` だけを更新する。型とリスト種別は Rust 側で検証する。NG 追加は JavaScript の `String.prototype.trim` と同じ空白を前後から除いて重複を判定する。コメントの操作メニューでは保存失敗をトーストで知らせる。不正な正規表現は既存のモデレーション判定と同様に無視し、パターン本文を含まない警告を記録する。
- **localStorage 併用**: 一部のUI/ウィンドウ局所設定は各ウィンドウの `localStorage` に保持する(`config.json` には載せない)。現状: 弾幕表示設定(`fc.danmaku`)、テーマ、最前面ピン状態、コマンドパレット履歴 等。これらはウィンドウローカルかつ低リスクなため意図的に localStorage 側に置いている。

### 10.1 自動バックアップと復元 (`backup.rs`)

- `config.json` の `backup { enabled, keep }` で制御する。既定は有効・10 世代。世代数は Rust 側で 1〜50 に丸める。最新が 20 時間以上前か初回なら起動時に作成し、稼働中は 24 時間ごとに作成する。前回と全データの SHA-256 が同じ場合は省略する。設定画面から手動作成、一覧、フォルダを開く操作ができる。
- 保存先は `<app_config_dir>/backups/auto/fc-backup-<UTC ミリ秒>.zip`。世代の順序は名前の時刻で判定し、この名前の通常ファイルだけを整理する。zip は一時ファイルを同期してから rename で公開する。
- 対象は `config.json`、`listeners.json`、`achievements.json`、`achievement-progress.json`、`sounds.json` と、`sounds/`、`templates/`、`skin/` のユーザーファイル。対象パスは `manifest.json`（schema `fast-comment-backup`、version 1）に記録する。zip では `credentials`、`xOverrides.bearerToken`、`youtubeOverrides.apiKey` を空にする。設定にある8文字以上の鍵・トークン・Cookie がファイル名または内容に残っていれば作成を中止する。OS の資格情報、`integrations.json`、X 自動告知の台帳、ログ、エクスポート、バックアップ自体、トップレベルの一時ファイルや退避ファイルは対象外。カスタムテンプレートとスキンの中にある同名のファイルは対象に含める。
- 復元はメイン画面の設定から行う。zip の相対パスと各ストアの JSON 形式を検証してから `backups/restore-pending/` に展開する。最大 2,000 エントリ、展開後合計 200 MiB、1 エントリ 50 MiB。リンクは拒否する。事前バックアップを作成して世代を整理する間は復元元 zip も保持する。`READY` を書いて再起動し、ストアの読み込み前に再検証する。既存の対象を `<名前>.pre-restore-<ms>` に退避して置き換え、zip に無い対象は退避した状態にする。
- ロールバックは失敗した対象ごとに最大3回試す。回復できなければ `APPLYING` を保持し、該当ストアの保存を止めて起動中は失敗を表示する。次回起動時に再試行する。回復後は `restore-failed-<ms>/` に作業データを残し、一度だけ通知する。起動時に作業用の一時ファイルを掃除し、失敗記録と退避はそれぞれ最新3世代を残す。バックアップ欄では自動作成の最後の失敗理由と一覧取得エラーを表示する。
- 既知の限界。事前バックアップの後、再起動までの変更は失われる。以下は現在の値・状態を引き継ぐ。
  - `credentials` 全体と OS の資格情報。
  - `xOverrides.bearerToken` と `youtubeOverrides.apiKey`。
  - `xOverrides.endpoints` 全体（`guestActivateUrl`、`broadcastShowUrl`、`liveChatUrl`、`userQueryUrl`、`userByScreenNameUrl`、`avatarContentUrl`）。
  - 棒読みちゃん自動起動の `tts.options.bouyomiPath` と `tts.options.bouyomiLaunchElevated`。
  - ローカル API と Webhook の設定、X 自動告知の台帳。

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

- リスナーのギフト金額は全期間の累計。`route.uncounted` の付かない経路で同じコメントが再配信されると、重複して数えることがある。通貨記号の上書き変更は変更後に届くギフトに適用し、集計済みの金額は記録時の通貨キーに残る。ツイキャスのギフトと X には金額データがないため集計対象外。

- WSLでは Tauri ビルド不可(Linuxバイナリになる)。ビルドはWindows側。
- Webhook のキューと重複排除はメモリのみ。終了時に未送信を捨てる。送信済みコメントの削除は通知を取り消さない。キーワードは全角/半角・ひらがな/カタカナを正規化しない。Discord の素の URL の自動リンク化は防げない。設定 UI は §8。
- YouTube InnerTube は非公式 → 仕様変更リスク。寛容パース+overrides+ログで吸収。
- X `live-chat` ストリームも非公式 → 同リスク。寛容パース+`xOverrides` で吸収。
- ツイキャスの取得(視聴ページ HTML・eventpubsuburl・WS)も非公式 → 同リスク。寛容パース+`twitcastingOverrides` で吸収(§4.5)。
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
- **文面テンプレート**(`xPost.template`、既定 `{title}\n{url}\n{date} {time}`): 変数 `{title}`(先頭配信のタイトル。YouTube > Twitch > ニコ生)、`{url}`(含む全配信の URL を 1 行ずつ)、`{platform}`(` / ` 連結)、`{date}`(`YYYY/MM/DD`)、`{time}`(`HH:MM`)。空行は詰め、未知の `{…}` は残して警告。`{url}` の無いテンプレートは `MissingUrl` で拒否。タイトルは制御文字・改行を空白に、`@` を `＠` に、`$`+英字を `＄` に置換(ハッシュタグは残す)。直近の台帳の文面と完全一致したら末尾に ` {time}` を足し、それでも一致すれば `Duplicate` で開かない。
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
- **状態取得**: `get_x_post_status` → `{ clientId, configured, connected, username, callbackUrl, unsavedToken }`。接続・切断の後は駆動タスクに `AuthChanged` を送り、投稿できるかを読み直させる。接続終了後の設定再取得と状態の `clientId` 照合はツイキャスと同じ。
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


## 15. ローカル API (PR3: アプリ内操作)

- ベース URL: `http://127.0.0.1:<obs.port>/api/v1`。既定 OFF。`GET /status`・`GET /comments`・`GET /events` を提供する。
- 有効化時、32 バイトの OS 乱数を base64url(パディングなし)にして `fc_` を付けた 46 文字のトークンを生成。資格情報 → 設定ファイル → ゲートの順で保存・反映する。メモリに常駐させるのは SHA-256 ダイジェストだけ。
- 無効時は `/api`・`/api/`・`/api/*` に本文なし 404。他の検査より先に行う。有効時は Host → Origin → Bearer → ルーティングの順。
- Host は `127.0.0.1:<port>`・`localhost:<port>`・`[::1]:<port>` の完全一致(大小無視)。Origin は値によらず付いていれば 403。API に CORS ヘッダを付けない。
- 認証は `Authorization: Bearer <token>` のみ(スキーム大小無視)。複数ヘッダ・512 バイト超・非 UTF-8・余分な空白は 401。クエリ・Cookie は使わない。ダイジェストを定数時間比較し、401 に `WWW-Authenticate: Bearer` を付ける。401 の warn ログはプロセス全体で 1 分に 1 回まで。トークン・Authorization ヘッダ・クエリ文字列はログに出さない。
- `/status` は `{app:"fast-comment",version,apiVersion:1,epoch,headCursor}`。headCursor は履歴内の最新コメントのカーソル(空なら null)。epoch はプロセス起動ごとの 12 桁 hex(ポート変更でも維持)。認証後の未知パスは JSON 404、メソッド違いは JSON 405。JSON の Content-Type は `application/json; charset=utf-8`、有効時の API 応答に `Cache-Control: no-store`・`X-Content-Type-Options: nosniff`。
- 起動時に資格情報が欠落・読込失敗ならゲートを閉じ、設定画面に再生成を案内。再生成は資格情報の保存成功後だけゲートを更新する。失敗時は旧トークンを維持。設定操作は専用 mutex で直列化。
- 設定画面の「ローカル API」は専用コマンドで即時保存。通常の「保存」とは独立。4 つのローカル API コマンドは呼び出し元が main ウィンドウの場合だけ許可する。トークンの表示・コピー時のみ資格情報から平文を取得する。トークンを OBS のソース URL やテンプレートに書かない。
- `GET /comments`: 有効な間だけ Bus のモデレーション判定後のコメントと削除イベントを購読し、直近 1000 件をメモリ保持。無効化で購読・履歴を破棄。有効化で空から始め、連番は起動中に巻き戻さない。
- カーソルは `<epoch>.<seq>`。`after` なしは最新 `limit` 件(既定 100、1〜500)を古い順、ありは指定より後を古い順に返す。`channel` は完全一致、`kind` は normal / superChat / membership / bits / gift / system のカンマ区切り。不正な値は 400。limit は ASCII 数字のみ許可する。同じ epoch で発行済みの連番より大きいカーソルは 400(削除後も発行済み連番は維持)。未知のクエリパラメータは無視する。応答は `{items,nextCursor,gap}`。0 件の nextCursor は after を維持(指定なしなら null)。別 epoch は最新側を返し gap=true、容量超過で必要な履歴が消えた場合も gap=true。
- ApiComment は専用 DTO から生成し、seq / id / platform / channel / author / fragments / text / kind / amount / timestampMs / highlight のみ公開。text は plain_text()、highlight バッジは badges から除く。raw / listener / nativeId / skipTts / injectedTest は非公開。任意値は欠落時キーごと省略。削除イベントの対象は OBS と同じ ID・platform・channel・author・beforeMs(同時刻を含む)で履歴から除く。同じ id の再受信は新しい seq で追記し、クライアントは後着を上書きとして扱う。
- `GET /events` は SSE。実績解除は SSE イベント `achievement`(data は Webhook と同じ K12 の 11 キーのオブジェクト 1 件、`id:` なし、履歴・再開対象外、1 解除 1 イベント)で次の tick に流す。実績は同じ tick の chat / moderation を送り終えた後にまとめて送り、chat のバッチを分割しない(chat A、実績、chat B の順でも chat は 1 回)。Bus の実績の欠落数(lagged)は接続キューの欠落数に足して lagged の dropped に出す。16ms ごとに空でない chat / moderation をこの順に送る(data は `{items:[…]}`)。chat のみ最後のカーソルを id に付ける。接続ごとに合計 256 件の待ち行列を持ち、溢れたら古い物から破棄し、送出前に lagged (`{dropped}`) を 1 回送る。Bus 購読側で検出した欠落も通知し、欠落分の連番を予約して GET の gap に反映する。削除通知の取りこぼし時は、削除済みコメントを返さないため履歴を破棄する。各 tick では先に削除通知を確保し、その時点までに受信したコメントを取り込み終えてから削除を適用する。例外: 同じ tick の中で、境界の無い(`beforeMs` 無し)クリアの後に、それに当てはまるコメントが届いた時だけ、そこで区切って chat / moderation の組を複数回送る(クライアントで新しいコメントにクリアが当たらないように)。後から来るコメントへの削除の適用は、`messageDeleted` の ID・`beforeMs` 付きクリア・境界の無いクリアで実際に消した ID を、10 秒・合計 4096 件まで保持して行う(それを超えた分は保証しない)。境界の無いクリアそのものは保持しない(後の正当なコメントを消さないため)。
- SSE は Last-Event-ID 優先、未指定または空なら after から再開。同じ epoch の未来カーソルは 400。クエリは after のみを解釈し、それ以外(limit / channel / kind を含む)は無視する。履歴確定とライブ購読を同じロックで行い、境界の重複・欠落を防ぐ。再送は 1 イベント 500 件まで、古いカーソルには lagged を先行。不明な欠落数は 0。無通信 15 秒で `: ping`。同時 8 本、9 本目は 429 too_many_streams、切断で枠を返す。無効化・トークン再生成・OBS サーバ停止で直ちに切断。timer / tts イベントは送らない。
- `POST /test-comments`: platform 必須(twitch / youtube / x / niconico)、kind は normal(既定) / superChat / membership / bits / gift、count は 1〜20(既定 1)、name は 64 文字以下、text は 500 文字以下、amount は有限の 0〜1,000,000,000。負のゼロは 0 に正規化する。未知キー・不正な型・範囲外は丸めず 400 invalid_request。name / text 省略は空文字として扱い、生成時に空白を trim して空なら「テストユーザー」/「テストコメントです」。amount は superChat / bits のみ適用。成功は 202 `{injected,ids}`。ID はプロセスごとに一度生成する UUID と連番の `injected-test:<UUID>-<連番>`、injected_test フラグを持ち、Webhook の対象外。source_tx → 既存パイプラインを通す。
- `GET /tts` は `{paused,depth,items:[{id,preview}]}`。`POST /tts/pause`・`resume`・`skip`・`clear` は既存コマンドの共通処理を使い、操作後のキュー状態を返す。pause / resume は冪等。UI の停止表示・発話停止・棒読みちゃんの停止/再開命令も既存どおり。
- `GET /timer` は既存の TimerSnapshot。`POST /timer/start` は本文なし、または `{durationSec:1〜359999}`。省略時は設定の既定秒数。`POST /timer/pause`・`resume`・`reset` は本文なし。遷移は既存コマンドと共通の純関数で処理し、操作が返すスナップショットを応答する。UI / OBS の watch 通知も共用。
- `GET /api/v1/poll` は PollSnapshot。`POST /api/v1/poll/start` は §19 の開始パラメータを JSON で受け、`POST /api/v1/poll/stop`・`POST /api/v1/poll/reset` は本文なし。各操作は IPC と同じ状態・入力検証を使い、成功時は操作後の PollSnapshot を返す。入力不正は 400 `invalid_request`、二重開始は 409 `poll_already_open`。
- 操作本文は JSON オブジェクトのみ、最大 16 KiB(413 payload_too_large)。本文ありで JSON 以外なら 415 unsupported_media_type。本文なし操作に JSON 本文が来た場合も 400。変更系 POST 全体で容量 20・毎秒 10 補充のトークンバケット(tokio::time)を共有し、OBS ポート変更でも枠を維持する。認証・入力検証後に消費。超過時は 429 rate_limited と Retry-After: 1、操作を実行しない。GET は制限しない。依存未提供は 503 unavailable、操作内部エラーは詳細を伏せた 500 internal。
- config / webhooks / token / chat/send / moderation/ban / tts/speak のルートは提供しない。Webhook は §16。


## 16. Webhook (PR4: 中核と保存)

- 公開モジュール `webhook` / `integrations` で判定・配送・保存を提供し、Tauri に依存しない。外部送信は `Sender` trait 経由。設定 UI は PR5。
- 定義は `integrations.json` の `webhooks` のみ。最大 10 件、名前は trim 後 1〜64 文字、キーワードは trim・空語除去後 20 語以内・各 1〜64 文字。自動停止は `paused: {reason, atMs}` として保存。
- URL と署名シークレットは OS 資格情報ストアの `webhook:<id>` に `{url, signingSecret?}` として保存。汎用 JSON のシークレットは暗号学的乱数 32 バイトの小文字 hex。変更は既存の async mutex で API 設定と共に直列化する。新規・編集・削除とも資格情報を先に変更し、ファイル保存失敗なら元の資格情報を復元(新規なら削除)する。復元失敗時は秘密を含まない固定文で警告する。読み込み不能な資格情報は URL 未設定として送らない。
- メインウィンドウだけが `list_webhooks` / `save_webhook` / `delete_webhook` / `resume_webhook` / `test_webhook` / `reveal_webhook_signing_secret` / `regenerate_webhook_signing_secret` を呼べる。一覧は URL の有無と伏せ字プレビューのみ。完全な URL を取得するコマンドは作らない。署名シークレットは明示的な表示・再生成時だけ返す。`AppConfig` / `update_config` / HTTP API には追加しない。
- 判定はモデレーション後の Bus を購読し、投げ銭(SuperChat/Bits/Gift) > メンバー加入 > キーワードの順に 1 件だけ選ぶ。キーワードは文字列を regex::escape した大文字小文字無視の部分一致で Text 断片ごとに照合する。エモート名・著者名・断片境界を照合対象にしない。System / injected_test / skip_tts / 無効・停止中は除外。(platform,id) を直近 8192 件保持して重複排除。有効な送り先が無い時は購読しない。Bus の lag は数えて破棄する。配信対象は設定の反映後に発生したイベントで、反映直後に発生したイベントは対象外になり得る。
- 実績解除(`triggers.achievement`、既定 false)は別経路。Bus の実績解除を購読し、有効にした送り先だけへ積む(キーワード等は見ない)。購読は chat と同じく有効な Webhook が 1 つでもあれば行い、送るかどうかは送り先ごとに `triggers.achievement` で決める。解除は原因になった chat の後に流す。汎用 JSON は同じ v1 封筒(`trigger:"achievement"`、`comment` 無し)の `achievement` キーに K12 の 11 キー(`type`・`achievementId`・`name`・`tier`・`subject`・`value`・`threshold`・`currency`・`unit`・`windowKey`・`unlockedAtMs`)を入れる。`currency` があれば金額系(`value`・`threshold` はミリ単位の整数、`unit:"milli"`)、無ければ回数系(回数そのもの、`currency:null`、`unit:"count"`)。`threshold` はエンジンが解除する最小値と一致し、解除通知では `value` は常に `threshold` 以上。`subject` は視聴者が `{kind,platform,authorId,name}`、配信者は `{kind:"streamer"}` のみ。説明・アイコン・内部 id は送らない。実績はチャットと同じ待ち行列(SSE は接続ごとに 256、Webhook は送り先ごとに 100)を共有し、同時に大量に解除されると古い物から落ちて lagged / dropped に数えられる。Discord は description に「名前 さんが「実績名」を解除しました」を入れる(`allowed_mentions` は `{"parse":[]}` のまま)。
- Discord は username=fast-comment、allowed_mentions.parse=[]、embed 1 件。Markdown の 12 文字を本文・著者名・タイトル内の利用者入力でエスケープ。description は 1000 文字、author.name と title は各 256 文字、全体は 6000 文字以内(省略記号を含む)。アイコンは HTTPS のみ。時刻はコメント時刻の UTC ISO 8601 ミリ秒。署名ヘッダ無し。
- 汎用 JSON は schema=fast-comment.webhook.v1、UUID v4 の eventId、trigger、sentAtMs、ApiComment から seq を除いた comment。matchedKeyword は keyword の時だけ。Content-Type とバージョン付き User-Agent を設定。X-FastComment-Event-Id / Timestamp(UNIX 秒) / Signature(v1=小文字 HMAC-SHA256) を必ず付ける。鍵は保存文字列の UTF-8 バイト列、対象は `<timestamp>.<本文バイト列>`。Timestamp は各送信試行の時刻とし、試行ごとに署名し直す。再送で本文・eventId は不変。
- URL は生の空白・制御文字・userinfo・fragment・900 バイト超を拒否して url crate で検査。Discord は HTTPS、指定 4 ホストの完全一致、ポート指定無し、`/api/webhooks/<id>/<token>` または数字版付きのパス。汎用は HTTPS の任意ホストと、HTTP の localhost・ループバック/プライベート/リンクローカル IP リテラルを許可。DNS 検索による制限はしない。形式変更時も再検証する。
- HTTP クライアントは専用 1 個、接続 5 秒・全体 10 秒、リダイレクトと Cookie 保存無し。応答本文は最大 64 KiB。reqwest エラーは without_url で URL を除去し、外部には固定の分類だけを返す。URL を持つ型は Debug を伏せる。
- 送り先ごとのワーカーは同時 1 送信・到着順、100 件のキューから溢れた最古を破棄。入列から 10 分で期限切れ。トークンバケットは満杯 5 から開始し 2 秒に 1 個補充。再送もトークンを消費。Discord の Remaining=0 と Reset-After を尊重する。
- 最大 4 試行、待機は 2/8/30 秒の ±20%。接続前の失敗・408/425/5xx を再送。送信後タイムアウト・読み取り失敗は Discord では uncertain、汎用では再送。429 は Discord なら Retry-After と本文 retry_after の大きい方 +250ms、汎用なら Retry-After(無ければ通常待機)で送り先のキュー全体を止める。残り寿命を超える再送は破棄。
- 401/403 は unauthorized、404/410 は notFound、3xx は redirected として停止。他の 4xx は停止せず失敗。試行を尽くした配送が連続 10 件なら unreachable で停止し、成功で連続数を戻す。停止・無効化・削除・設定変更は待機キューを破棄し、変更した送り先だけワーカーを作り直す。再開は専用コマンドのみ。
- 状態は active/disabled/paused/noUrl、成功・失敗・破棄・uncertain 件数、待ち件数、最後の結果。`webhook-status` は変化時のみ最大毎秒 1 回 emit。テスト送信は固定文・trigger=test で規則・キューを通さず 1 回だけ、無効・停止中でも送れ、停止は解除しない。

- 設計書からの補足: Discord の title にも 256 文字の上限を適用する(全体 6000 文字だけでは HTTP 400 になるため。[公式 Embed Limits](https://discord.com/developers/docs/resources/message#embed-object-embed-limits))。
- 設定 UI(PR5): 設定画面「Webhook」(`id="settings-webhooks"`)。一覧は名前・形式・有効/無効・状態ラベル(active/disabled/paused/noUrl)・URL の伏せ字プレビュー・件数・直近の結果を出し、状態は `list_webhooks` の `status` で取得、以降は `webhook-status` イベントで更新する。追加・編集フォームは名前・形式・URL(伏せ字入力。編集時に空欄なら `url` を送らず保存済みの URL を維持)・きっかけ 3 種・キーワード(1 行 1 語。trim して空行を除く。重複は除かない)・有効。10 件で追加を無効化。削除と署名シークレット再生成は確認つき。汎用 JSON のみ署名シークレットの表示・コピー・再生成。停止中は理由(unauthorized / notFound / redirected / unreachable)を日本語で出して「再開」を出す。エラーは IPC の種別(InvalidWebhook 等)を固定文に変換して表示する。使い方・署名の検証例は README「Webhook の設定」。

## 17. 実績の進捗保存(補足)

- 5 秒ごとの定期 flush は、前回の保存から進捗が変わっていない時は書かない(終了時・無効化時の flush も同じで、変更が無ければ何もせず成功を返す。ただし進捗ファイルが未作成なら 1 回は書く)。Webhook の転送は終了時に、保留中だった実績解除を送らず、件数だけをログに残す(送信待ち列は直後の停止で破棄されるため)。この件数はベストエフォートの概数で、送り先で絞る前の数を数える一方、停止で捨てた送信待ちは含まない。終了処理が転送の後片付けを待たないため、ログ自体が出ないこともある(失われるのはログだけで、進捗・解除の保存には影響しない)。
- 無効化の保存が競合で Stale になっても、進捗の保存は行われることがある(進捗は正しい値)。

## 18. 種別ごとの効果音 (`sound_effects.rs`・`sounds.rs`)

- **責務**: 判定と間引きは Rust、UI は鳴らすだけ。既存のハイライト通知音(`ui.notifySound`、880Hz のビープ)はそのまま残す。
- **設定**: `config.json` の `soundEffects`(既定は無効)。`enabled` / `volume`(0〜1、既定 0.6)/ `minIntervalMs`(0〜5000、既定 300)と、種別ごとの規則 `superChat` / `bits` / `gift` / `membership` / `firstComment`(各 `enabled`・`sound`・`minJpy`)。`minJpy` は投げ銭系の 3 つだけが見る(円、既定 0、上限 1000 万)。`sound` は `builtin:<chime|bell|pop|coin>` か `custom:<uuid>`。旧 config.json に無くても既定値で読め、型違い・不正な `sound` は項目単位で既定値に落とし、範囲外の数値は上限・下限に丸める(読み込みは寛容)。
- **判定** (`select_sound`、1 件に鳴る音は 1 つ): 投げ銭(SuperChat / Bits / Gift)> メンバー > 初コメ(`listener.isFirstEver`)。上位の規則が OFF か金額未満なら次の候補へ落とす。鳴らさない物: 全体 OFF、System、`injected_test`、`route.uncounted`(履歴・バックフィル・ギフトコンボ更新)、モデレーションで非表示になった物(パイプラインが判定前に落とす)、枠の `soundEnabled === false`。
- **金額**: 実績のレート表と記号の上書き(`achievements/currency.rs`)で円に換算し、`>= minJpy` で鳴らす(ちょうど閾値は鳴る)。レート表に無い通貨など円に換算できない時は、`minJpy` が 0 なら鳴らし、0 より大きければ鳴らさない。**Bits** は通貨 `BITS` で、レート表に `BITS` を足していない限り換算できないので、その場合は `minJpy` の判定対象外(常に鳴る)。足してあれば他と同じく換算して比べる。
- **間引き**: アプリ全体で最小間隔(`SoundGate`)。間隔内の音は捨て、溜めない。鳴らない種別・捨てた音は間隔の起点を動かさない。
- **伝播**: パイプライン(`spawn_pipeline`)が選んだ音を `ChatMessage.sound`(`Option<String>`、無ければ送らない)に載せ、既存の `chat` バッチで UI へ送る。新しい emit は無い。同じメッセージでハイライトも当たった時は効果音を優先し、ビープは鳴らさない(画面のフラッシュは出す)。間引きで音が捨てられたメッセージは従来どおりビープが鳴る。OBS の `/ws` にも同じ JSON が流れるが、テンプレは見ない。
- **内蔵音**: WebAudio の合成音(ファイルは同梱しない)。AudioContext が止まっている(自動再生の制限)間に来た音は溜めずに捨て、解除を試みる。解除後に来た音から鳴らす(試し鳴らしは解除を待つ)。削除の印(`removed`)が付いたコメントは鳴らさない。`soundEffects.ts` が AudioContext・Audio・読み込みを注入して鳴らす純モジュール。
- **カスタム音源**: 設定画面でファイルを選び、バイト列を IPC(`add_custom_sound`、raw body、表示名はヘッダ `x-sound-name` を URI エンコード)で Rust に渡す。拡張子とマジックバイトの両方で wav / mp3 / ogg を検証し、2 MiB 以下・最大 32 件。保存先は `app_config_dir/sounds/<uuid>.<ext>`、索引は `sounds/sounds.json`。ファイル名は Rust が UUID で付け、ユーザーが入れた名前は表示名(制御文字除去・60 文字)として索引にだけ持つ。パスに使う前に ID を検証し(小文字ハイフン形式の UUID)、索引に無い ID や拡張子は無視する。symlink は作らず、読む時も通常ファイルだけ。`sounds` ディレクトリがリンク(symlink・ジャンクション)なら、`symlink_metadata` と canonicalize の比較(保管庫の親の配下か)で、読み書きとも拒否する。索引は無い時だけ空とみなし、読み取り失敗・破損はエラーにして上書きしない。拡張子の判定は表示名を切り詰める前の元のファイル名で行う。エラー文・ログにパスや入力名を出さない。再生用の `read_custom_sound` は bytes を返し、UI は Blob URL にして(最大 8 件の LRU で)キャッシュする。読めない時は内蔵の `chime` で代用する。
- **削除**: `remove_custom_sound` は本体ファイル → 索引の順に消す(本体が既に無ければ成功扱い、それ以外の失敗は索引を変えずエラー)。**設定は書き換えない**。削除済みの `custom:<id>` を指す規則は、(1) 設定の保存時に各規則の既定の内蔵音へ置き換えて保存し(保存値は画面へ返る)、(2) 実行時の判定でも既定音として扱う。画面は削除成功後に、フォーム内でその id を参照する行だけを既定音にする(ほかの未保存の編集は残す)。
- **枠ごとの上書き**: `ChannelOverrides.soundEnabled`。`tts_enabled` と同じく `false` だけを保存する(`true` は正規化で消える)。枠ごとの設定ダイアログにチェックボックス「この枠は効果音を鳴らさない」。
- **試し鳴らし**: 設定画面の各行のボタンは UI だけで鳴らす(判定・間引きの経路は通さない)。
- **対象外 (v1)**: チャンネルポイントの引き換えと接続断の音は将来の候補。


## 19. アンケート (コメント投票 / Twitch 公式 Polls)

- アプリ全体で同時に 1 件の投票を扱う。`PollSnapshot` は `id`・`source`(`"chat"` / `"twitch"`)・`status`(`idle`/`open`/`closed`)・`question`・`options`(`label`・`votes`・`percent`・`leading`)・`totalVoters`・`startedAtMs`・任意の `endsAtMs`・`display`・`showPercent`・`showCounts` を持つ。票が 0 の時の割合は 0。割合・最多の判定は Rust で計算する。
- **コメント方式 (`source: "chat"`、省略時の既定値)** の `poll_start` は trim 後の質問 1〜100 文字、選択肢 2〜10 件(各 1〜40 文字、大小文字を無視して一意)と表示設定を受け付ける。`durationSecs` は 10〜3600 秒。省略時は手動終了。期限は Rust の単調時計で管理する。`poll_stop` は結果を保持したまま閉じ、`poll_reset` は待機状態に戻す。`poll_set_display` は表示形式(`bar-h`/`bar-v`/`pie`/`donut`/`number`)と割合・票数の表示を変更する。
- コメント方式の受付中はモデレーション後のコメントを判定する。System・非表示・履歴やバックフィル(`route.uncounted`)・空の著者 ID を除外し、`injected_test` のテスト注入は数える。`plain_text()` の前後の空白と見えない文字(Twitch のクライアントが連投のために付ける U+E0000、BOM、ゼロ幅文字)を落とし、番号 1〜N(全角数字可)を優先し、該当しなければラベルと大小文字を無視して全文一致で照合する。投票者のキーはプラットフォームと著者 ID の組。最新の選択肢で上書きし、同じ選択肢の連投は集計を変えない。最大 100,000 人。上限到達後も既存の投票者は変更できる。
- 状態はメモリ内の `watch` に公開する。`poll_state` で現在値を取得でき、票による `poll-updated` と OBS 更新は最大 100ms に 1 回。操作や自動終了は即時反映する。`config.json` の `poll` に最後に使った方式・質問・選択肢・表示設定を保存する。欠落・破損時は既定値を使う。票・結果は再起動で消える。アンケートの通知・ログ・エラーに投票者の名前や ID を含めない。
- ローカル API は §15。OBS は設定の OBS 節またはアンケート画面から URL をコピーする。`/poll` WS は接続時と更新時に最新のスナップショットを送る。透過テンプレートは質問・残り時間・総投票者数・グラフを表示する。待機中は非表示、終了後は「結果」として最多の選択肢をすべて強調する。

### 19.1 Twitch 方式 (`source: "twitch"`)

- アンケート画面の方式で Twitch を選択する。質問は trim 後 1〜60 文字、選択肢は 2〜5 件で各 1〜25 文字。文字数は Unicode スカラー値で数え、期間は 15〜1800 秒で必須。選択肢の重複判定はコメント方式と同じく、前後の空白・不可視文字を除き、大文字・小文字を無視する。視聴者は Twitch 側で投票する。
- 有効な Twitch チャンネルのうち、「自分の配信」に設定したものが 1 件必要。Device Code Flow でログインし、`pollsScopeOk` が true で、Helix のユーザー検索で得た配信者 ID がログイン中のユーザー ID と一致した場合に作成する。Polls 権限が不足している場合はアンケート画面から Twitch のログイン設定を開ける。
- 作成時に通信が途切れた場合は、POST を繰り返さず、`GET /polls` に配信者 ID だけを渡し、先頭の投票を確認する。受付中で、質問と選択肢の内容・順序、`duration` が送った値と一致し、`started_at` が POST 直前の壁時計(UTC)の 10 秒前より後なら、その ID を採用する(配信者が Twitch 側で先に始めた同じ内容の投票を拾わないため)。この GET は通信失敗と 5xx の時だけ、1 秒・2 秒・4 秒待ってから最大 3 回試し、アプリの終了で即やめる。一致する投票を確認できなければ、ローカルの投票を開始せず、Twitch 側に投票が作られた可能性があるので Twitch で確認・終了するよう知らせる。採用した投票の開始・終了予定時刻は Twitch の `started_at` と `started_at + duration` を使う。作成が確認できた時点で、方式・質問・選択肢・表示設定を `config.json` に保存し、ローカルの投票を開始する。保存に失敗した場合も作成済みの投票を追跡し、保存エラーを知らせる。
- Helix の `GET /polls` を 3 秒ごとに実行し、選択肢順の票数を上書きする。総投票者数はその合計。Twitch の状態が受付中以外なら最終値で閉じ、404 なら直前の票で閉じて集計を終了する。応答に `started_at` と `duration` があれば、開始・終了予定時刻を Twitch の値に合わせ直す。通信失敗時は前回の票を保持し、レート制限時は `Ratelimit-Reset` まで待つ。401 は認証を更新して 1 回再試行する。集計の失敗は分類名だけを WARN に出し、同じ分類が連続する間は初回だけ記録する。ポーラーは投票のロックを外して HTTP を待ち、反映前にアプリの終了トークンとローカルの投票 ID を確認する。
- 手動終了は `TERMINATED` を要求して最終結果を取得する。リセットは Twitch 側が受付中の時だけ `TERMINATED` を要求する。最初の GET の 404 と、この PATCH の 400・404 は終了済みとして扱う。結果は Twitch 上に残る。遠隔操作が失敗しても、手動終了は直前の票で閉じ、リセットは待機状態に戻し、失敗の文言を返す。
- IPC とローカル API は同じ操作を使い、画面と OBS は同じスナップショットを表示する。アフィリエイト・パートナーの案内は作成時の権限エラーに限り、終了時の 403 には汎用の文言を返す。400 の文言には安全な Helix のメッセージを添える。認証の取得・更新で起きた失敗は、操作回数制限を `RateLimited`(解除時刻が分からないので 60 秒)、通信失敗を `Network`、提供停止を HTTP 503、競合を 409、権限不足を 403 として返し、資格情報の読み書きや応答解析の失敗だけを認証情報の確認失敗とする。エラーやログに資格情報やリクエスト URL を含めない。

PR3 は YouTube 公式投票を同じスナップショットとグラフへ接続する。
