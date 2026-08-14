//! X (Twitter) ライブ配信 (broadcasts) チャット Source。
//!
//! X のライブチャットは新基盤 `api.x.com/live-chat` に移行済みで、Web UI も
//! チャット欄をこのエンドポイントから描画する(実測 2026-08)。旧 Periscope
//! chatapi(chatnow WS / history)には room join ack と presence しか流れず、
//! チャット本文は一切来ない。本文を拾うには live-chat の NDJSON ストリームを
//! 読むのが唯一の経路。
//!
//! フロー:
//! 1. POST guest/activate.json(公開 Bearer)→ guest_token
//! 2. GET broadcasts/show.json?ids={id} → state / twitter_user_id /
//!    配信者名 / タイトル / total_watching(視聴者数)
//! 3. GET {liveChatUrl}?broadcastId={id} … 認証ヘッダ不要。
//!    chunked NDJSON で、接続直後に直近チャットが isBackfill:true で一括
//!    バックフィルされ、以降は新着がリアルタイム push される(接続は維持)
//! 4. show.json を定期再取得して viewers 更新と配信終了検出を行う
//!
//! NDJSON 行の形: `{"userId":"<数値文字列>","chatType":1,"message":"本文",
//! "ts":"<ナノ秒>","isBackfill":true}`。chatType 1 が本文、39 はモデレーション系
//! メタ(message に対象チャットの ts が入る)。行に username は含まれず、
//! ゲストで userId→username を解決できる API も存在しない(users/lookup=404、
//! GraphQL=403/404、intent/user=SPA シェル; いずれも実測 2026-08)。そのため
//! 配信者(twitter_user_id 一致)は show.json の名前で表示し、他は既定で
//! 「ユーザー<ID下4桁>」の匿名表示に劣化させる。NG 等は userId 基準で機能する。
//! `CredentialsConfig` に X のログイン cookie(auth_token / ct0)が設定されて
//! いる場合のみ、GraphQL liveAtomsUserQuery をバッチで叩いて実名に差し替える
//! (`name_resolver_task`)。cookie 失効時は匿名表示へ自動フォールバックし、
//! チャット受信そのものは止めない。
//!
//! パースは固い struct deserialize をせず `serde_json::Value` のパス探索で
//! 欠落を None に劣化させる(SPEC の YouTube パースと同方針)。URL/Bearer は
//! `XOverrides` で再ビルド無しに上書きできる。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::config::XOverrides;
use crate::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use crate::stats::YoutubeMetadataUpdate;

/// X Web クライアントに埋め込まれている公開 Bearer(シークレットではない)。
const X_WEB_BEARER: &str = "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs=1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const GUEST_ACTIVATE_URL: &str = "https://api.x.com/1.1/guest/activate.json";
const BROADCAST_SHOW_URL: &str = "https://x.com/i/api/1.1/broadcasts/show.json";
const LIVE_CHAT_URL: &str = "https://api.x.com/live-chat";
/// ブラウザ相当の UA。X の Web API はブラウザ以外の UA を弾くことがある。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
/// bootstrap / show ポーリング各リクエストの全体タイムアウト。
/// live-chat ストリームには適用しない(張りっぱなしが正常のため)。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP + TLS 接続確立のタイムアウト(ストリーム接続にも効く)。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// ストリームからこの時間チャンクを受信しなければ half-open とみなし再接続する。
/// 無コメント区間はサーバーも無送信のため短くしすぎない(実測: 43 秒無音の後に
/// 新着が届いた)。再接続コストはバックフィル+dedup で吸収されるので低い。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// show.json を再取得して viewers / 配信終了を反映する間隔。
const SHOW_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// この間隔で受信状況を info ログへ出す(生存中の証拠)。
const HEARTBEAT_LOG_EVERY: Duration = Duration::from_secs(30);
/// dedup で保持する直近チャット ID の上限(メモリ上限)。超過分は古い順に捨てる。
const DEDUP_CAPACITY: usize = 4096;
/// 接続時バックフィルから UI へ流す最大件数。live-chat は配信開始以降の
/// 全チャットをバックフィルするため、長時間配信では数千件になり得る。
/// 直近分だけ出せば「途中から開いても文脈が見える」には十分。
const BACKFILL_EMIT_MAX: usize = 20;
/// バックフィル終端の合図(非バックフィル行)が来なくてもこの時間で吐き出す。
/// 無コメント配信ではバックフィルの後に何も届かないため、タイマーが唯一の終端。
const BACKFILL_FLUSH_AFTER: Duration = Duration::from_secs(5);
/// userId→表示名解決に使う GraphQL(X Web UI がチャット欄で使うのと同じ)。
/// queryId 込みの URL なので、変わったら overrides の userQueryUrl で差し替える。
const USER_QUERY_URL: &str =
    "https://x.com/i/api/graphql/n9VGEKZLhDHED8ouN0dxUw/liveAtomsUserQuery";
/// 1回の liveAtomsUserQuery に載せる userId の上限。
const NAME_RESOLVE_BATCH_MAX: usize = 50;
/// 名前解決1回のタイムアウト。超えたら匿名のまま流す(チャットを遅らせない)。
const NAME_RESOLVE_TIMEOUT: Duration = Duration::from_secs(4);
/// 名前キャッシュの上限。超えたら全消しする(1配信の発言者数では届かない)。
const NAME_CACHE_MAX: usize = 8192;
/// 受信ループ → 名前解決タスク間のチャネル容量。解決が詰まった時の緩衝。
const RESOLVE_QUEUE_CAPACITY: usize = 512;

/// URL または生 ID から broadcast ID を取り出す。
///
/// `https://x.com/i/broadcasts/{id}` / `https://twitter.com/i/broadcasts/{id}`
/// 形式のほか、英数字のみの入力はそのまま ID として扱う。
pub fn extract_broadcast_id(input: &str) -> String {
    let s = input.trim();
    if let Some(pos) = s.find("/i/broadcasts/") {
        let rest = &s[pos + "/i/broadcasts/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !id.is_empty() {
            return id;
        }
    }
    // URL でなければクエリ/フラグメント/末尾スラッシュだけ落として ID とみなす。
    let core = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    core.rsplit('/').next().unwrap_or(core).to_string()
}

/// X ログイン cookie(ユーザー名解決用)。`CredentialsConfig` 由来。
/// live-chat 本文の受信には不要で、無くてもチャットは動く。
#[derive(Clone)]
pub struct XAuth {
    /// auth_token cookie。
    pub auth_token: String,
    /// ct0 cookie(x-csrf-token ヘッダにも同じ値を入れる)。
    pub csrf_token: String,
}

/// show.json から得る配信メタデータ。
struct ShowInfo {
    /// 配信者の Twitter user_id(rest_id)。live-chat の userId と同じ空間。
    /// 旧実装が使っていた `user_id` は Periscope ID で、live-chat とは別空間
    /// なので broadcaster 判定に使ってはいけない(実測: user_id="1eVQ…" に対し
    /// twitter_user_id="1980639…")。
    broadcaster_id: Option<String>,
    /// 配信者の表示名(user_display_name、無ければ username)。
    broadcaster_name: Option<String>,
    /// 配信タイトル(チップ表示用)。
    title: Option<String>,
    /// 現在の視聴者数(total_watching)。文字列で返るため寛容にパースする。
    viewers: Option<u32>,
    /// RUNNING / ENDED / TIMED_OUT 等。
    state: String,
}

#[derive(Debug)]
struct XSessionError {
    error: anyhow::Error,
    stable: bool,
}

impl XSessionError {
    fn new<E>(error: E, stable: bool) -> Self
    where
        E: Into<anyhow::Error>,
    {
        XSessionError {
            error: error.into(),
            stable,
        }
    }
}

/// X ライブ配信1件のチャットを購読する Source。
pub struct XSource {
    /// 正規化済み broadcast ID。
    broadcast_id: String,
    overrides: XOverrides,
    /// ユーザー名解決用のログイン cookie。None なら匿名表示で動く。
    auth: Option<XAuth>,
    /// チップの接続状態表示用。live フラグ/タイトル/視聴者数を送る。
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl XSource {
    pub fn new(
        identifier: String,
        overrides: XOverrides,
        auth: Option<XAuth>,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        XSource {
            broadcast_id: extract_broadcast_id(&identifier),
            overrides,
            auth,
            metadata_tx,
        }
    }

    fn bearer(&self) -> String {
        self.overrides
            .bearer_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| X_WEB_BEARER.to_string())
    }

    /// overrides の endpoints にキーがあればそれを、無ければ既定 URL を使う。
    fn endpoint_url(&self, key: &str, default: &str) -> String {
        self.overrides
            .endpoints
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default.to_string())
    }
}

impl Source for XSource {
    fn name(&self) -> String {
        format!("x:{}", self.broadcast_id)
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let mut backoff = Backoff::new();
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                match self.connect_and_listen(&tx, &cancel).await {
                    Ok(stable) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        self.send_live_state(false, None, None).await;
                        if stable {
                            backoff.reset();
                        }
                    }
                    Err(e) => {
                        self.send_live_state(false, None, None).await;
                        if e.stable {
                            backoff.reset();
                        }
                        tracing::warn!("x:{} 接続エラー: {:#}", self.broadcast_id, e.error);
                    }
                }

                let delay = backoff.next_delay();
                tracing::info!(
                    "x:{} {}ms 後に再接続",
                    self.broadcast_id,
                    delay.as_millis()
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }
}

impl XSource {
    /// 1接続分のセッション。show.json → live-chat ストリーム接続 → 受信ループ。
    ///
    /// 戻り値は Twitch と同じ「安定セッション」判定(チャット実受信 or 30秒以上)。
    async fn connect_and_listen(
        &self,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> Result<bool, XSessionError> {
        // ストリームを全体タイムアウトで殺さないよう Client には connect_timeout
        // のみ設定し、bootstrap 系は各リクエストに個別タイムアウトを付ける。
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| XSessionError::new(e, false))?;

        // HTTP フローは配信未開始/終了でも失敗するため、失敗は不安定扱いで
        // バックオフを伸ばし続ける(=配信開始待ちのポーリングを兼ねる)。
        // チャンネル削除/設定変更の cancel は bootstrap 中も効かせる。
        let mut show = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = self.fetch_show(&client) => r.map_err(|e| XSessionError::new(e, false))?,
        };
        // ENDED/TIMED_OUT はストリーム接続前に止め、原因をログへ明示する。
        // ループ側の再接続は継続するので、同じ枠で再配信が始まれば自動で拾える。
        if matches!(show.state.as_str(), "ENDED" | "TIMED_OUT") {
            return Err(XSessionError::new(
                anyhow::anyhow!("配信が終了済み (state={})", show.state),
                false,
            ));
        }
        if show.state != "RUNNING" {
            tracing::warn!(
                "x:{} 想定外の配信状態 state={}",
                self.broadcast_id,
                show.state
            );
        }

        // live-chat ストリームへ接続。認証ヘッダは不要(実測: 素の GET で 200)。
        let live_chat_url = self.endpoint_url("liveChatUrl", LIVE_CHAT_URL);
        let resp = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = client
                .get(&live_chat_url)
                .query(&[("broadcastId", self.broadcast_id.as_str())])
                .send() => r.map_err(|e| XSessionError::new(e, false))?,
        };
        let status = resp.status();
        if !status.is_success() {
            // ライブでない配信への接続は 403 が返る(実測)。bootstrap の state
            // チェック後に終了した際などに踏む。再接続で fetch_show が拾い直す。
            return Err(XSessionError::new(
                anyhow::anyhow!("live-chat 接続拒否 (HTTP {status})"),
                false,
            ));
        }
        tracing::info!(
            "x:{} live-chat 接続 status={} content-type={:?}",
            self.broadcast_id,
            status,
            resp.headers().get("content-type"),
        );
        let mut stream = resp.bytes_stream();

        self.send_live_state(true, show.title.clone(), show.viewers).await;

        // 受信ループは同期処理に徹し、emit は名前解決タスク経由で broadcast へ
        // 流す(cookie 未設定時は素通し)。FIFO の mpsc なので表示順は保たれる。
        let (chat_tx, chat_rx) = mpsc::channel::<ChatMessage>(RESOLVE_QUEUE_CAPACITY);
        let resolver = self.auth.clone().map(|auth| {
            NameResolver::new(
                client.clone(),
                self.endpoint_url("userQueryUrl", USER_QUERY_URL),
                self.bearer(),
                auth,
            )
        });
        if resolver.is_some() {
            tracing::info!("x:{} ユーザー名解決 有効(cookie 設定あり)", self.broadcast_id);
        }
        tokio::spawn(name_resolver_task(
            chat_rx,
            tx.clone(),
            resolver,
            self.broadcast_id.clone(),
        ));

        let mut stats = SessionStats::new(&self.broadcast_id);
        let mut seen = ChatDedup::new(DEDUP_CAPACITY);
        // NDJSON はチャンク境界が行境界と一致しない(実測: 行が分割されて届く)
        // ため、バイト列を貯めて改行単位で切り出す。
        let mut line_buf: Vec<u8> = Vec::new();

        // 接続直後のバックフィル(過去チャット)は直近 BACKFILL_EMIT_MAX 件に
        // 絞り、skip_tts を立てて流す(接続のたびに読み上げが暴発しないように)。
        // 非バックフィル行の到着かタイマーで終端し、以降は即時 emit に切り替える。
        let mut backfill_open = true;
        let mut backfill_buf: VecDeque<ChatMessage> = VecDeque::new();
        let backfill_deadline = tokio::time::Instant::now() + BACKFILL_FLUSH_AFTER;

        let mut last_chunk = tokio::time::Instant::now();
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + HEARTBEAT_LOG_EVERY,
            HEARTBEAT_LOG_EVERY,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // fetch_show 直後なので初回 tick は SHOW_POLL_INTERVAL 後。
        let mut show_poll = tokio::time::interval_at(
            tokio::time::Instant::now() + SHOW_POLL_INTERVAL,
            SHOW_POLL_INTERVAL,
        );
        show_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                    return Ok(true);
                }
                _ = heartbeat.tick() => {
                    stats.log_progress();
                }
                // バックフィル終端がこないまま無コメントが続くケースの吐き出し。
                _ = tokio::time::sleep_until(backfill_deadline), if backfill_open => {
                    backfill_open = false;
                    self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                }
                _ = show_poll.tick() => {
                    match tokio::time::timeout(HTTP_TIMEOUT, self.fetch_show(&client)).await {
                        Ok(Ok(next)) => {
                            stats.show_polls += 1;
                            if matches!(next.state.as_str(), "ENDED" | "TIMED_OUT") {
                                tracing::info!(
                                    "x:{} 配信終了を検出 (state={})",
                                    self.broadcast_id,
                                    next.state
                                );
                                self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                                return Ok(stats.is_stable());
                            }
                            self.send_live_state(true, next.title.clone(), next.viewers).await;
                            show = next;
                        }
                        Ok(Err(e)) => {
                            stats.show_errors += 1;
                            tracing::debug!("x:{} show 再取得失敗: {:#}", self.broadcast_id, e);
                        }
                        Err(_elapsed) => {
                            stats.show_errors += 1;
                            tracing::debug!("x:{} show 再取得タイムアウト", self.broadcast_id);
                        }
                    }
                }
                // チャンク無受信が続いたら half-open とみなして張り直す。
                _ = tokio::time::sleep_until(last_chunk + STREAM_IDLE_TIMEOUT) => {
                    self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                    return Err(XSessionError::new(
                        anyhow::anyhow!(
                            "ストリーム無受信 {}秒(half-open の疑い)",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        ),
                        stats.is_stable(),
                    ));
                }
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            last_chunk = tokio::time::Instant::now();
                            line_buf.extend_from_slice(&bytes);
                            // 改行のたびに1行取り出して処理。行断片は buf に残す。
                            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = line_buf.drain(..=pos).collect();
                                let line = String::from_utf8_lossy(&line);
                                let line = line.trim();
                                if line.is_empty() {
                                    continue;
                                }
                                self.handle_line(
                                    line,
                                    &show,
                                    &chat_tx,
                                    &mut stats,
                                    &mut seen,
                                    &mut backfill_open,
                                    &mut backfill_buf,
                                );
                            }
                        }
                        Some(Err(e)) => {
                            self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                            return Err(XSessionError::new(e, stats.is_stable()));
                        }
                        None => {
                            // サーバー側のストリーム終了。配信終了直前などに起きる。
                            // 残った行断片は改行なしで終わることがあるため処理する。
                            let tail = String::from_utf8_lossy(&line_buf).trim().to_string();
                            if !tail.is_empty() {
                                self.handle_line(
                                    &tail,
                                    &show,
                                    &chat_tx,
                                    &mut stats,
                                    &mut seen,
                                    &mut backfill_open,
                                    &mut backfill_buf,
                                );
                            }
                            self.flush_backfill(&mut backfill_buf, &chat_tx, &mut stats);
                            tracing::info!("x:{} live-chat ストリーム終了", self.broadcast_id);
                            return Ok(stats.is_stable());
                        }
                    }
                }
            }
        }
    }

    /// NDJSON 1行を処理する。チャット(chatType:1)なら dedup を通して
    /// バックフィルバッファ or 即時 emit へ、他はカウントのみ。
    #[allow(clippy::too_many_arguments)]
    fn handle_line(
        &self,
        line: &str,
        show: &ShowInfo,
        chat_tx: &mpsc::Sender<ChatMessage>,
        stats: &mut SessionStats,
        seen: &mut ChatDedup,
        backfill_open: &mut bool,
        backfill_buf: &mut VecDeque<ChatMessage>,
    ) {
        stats.lines += 1;
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                stats.parse_errors += 1;
                if stats.parse_errors <= 5 {
                    tracing::info!(
                        "x:{} NDJSON パース失敗: {}",
                        self.broadcast_id,
                        truncate_chars(line, 200)
                    );
                }
                return;
            }
        };
        let chat_type = v.get("chatType").and_then(Value::as_i64).unwrap_or(-1);
        if chat_type != 1 {
            *stats.non_chat.entry(chat_type).or_insert(0) += 1;
            return;
        }
        let is_backfill = v.get("isBackfill").and_then(Value::as_bool).unwrap_or(false);
        let Some(chat) = self.line_to_chat(&v, show) else {
            stats.parse_errors += 1;
            if stats.parse_errors <= 5 {
                tracing::info!(
                    "x:{} チャット行の正規化失敗: {}",
                    self.broadcast_id,
                    truncate_chars(line, 200)
                );
            }
            return;
        };
        // 再接続のたびにバックフィルが全量再送されるため、確定 ID で弾く。
        if !seen.insert(&chat.id) {
            stats.dup += 1;
            return;
        }
        if is_backfill {
            stats.backfill_seen += 1;
        }
        if *backfill_open {
            if is_backfill {
                backfill_buf.push_back(chat);
                if backfill_buf.len() > BACKFILL_EMIT_MAX {
                    backfill_buf.pop_front();
                    stats.backfill_skipped += 1;
                }
                return;
            }
            // 非バックフィル行の到着 = バックフィル終端。貯め分を吐いてから
            // 通常運転へ切り替える。
            *backfill_open = false;
            self.flush_backfill(backfill_buf, chat_tx, stats);
        }
        self.emit(chat, chat_tx, stats);
    }

    /// バックフィルバッファを古い順に emit する(TTS は抑制)。
    fn flush_backfill(
        &self,
        buf: &mut VecDeque<ChatMessage>,
        chat_tx: &mpsc::Sender<ChatMessage>,
        stats: &mut SessionStats,
    ) {
        while let Some(mut chat) = buf.pop_front() {
            chat.skip_tts = true;
            self.emit(chat, chat_tx, stats);
        }
    }

    fn emit(
        &self,
        chat: ChatMessage,
        chat_tx: &mpsc::Sender<ChatMessage>,
        stats: &mut SessionStats,
    ) {
        stats.chats += 1;
        if stats.chats == 1 {
            tracing::info!("x:{} 初チャット受信", self.broadcast_id);
        }
        // 名前解決タスクが GraphQL 待ちで詰まった時だけ満杯になり得る。
        // ブロックすると受信ループごと止まるため、捨てて数える。
        if chat_tx.try_send(chat).is_err() {
            stats.queue_full += 1;
            if stats.queue_full == 1 {
                tracing::warn!(
                    "x:{} 名前解決キュー満杯 — チャットを破棄",
                    self.broadcast_id
                );
            }
        }
    }

    /// chatType:1 の NDJSON 行を `ChatMessage` へ正規化する。
    fn line_to_chat(&self, v: &Value, show: &ShowInfo) -> Option<ChatMessage> {
        let text = v.get("message").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            return None;
        }
        let user_id = id_string(v.get("userId"))?;
        // ts はナノ秒の文字列(数値で来る個体にも耐える)。
        let ts_raw = v
            .get("ts")
            .and_then(|t| match t {
                Value::String(s) => s.trim().parse::<i64>().ok(),
                Value::Number(n) => n.as_i64(),
                _ => None,
            })
            .unwrap_or(0);
        let timestamp_ms = if ts_raw > 0 {
            normalize_epoch_ms(ts_raw)
        } else {
            now_ms()
        };

        let broadcaster = show
            .broadcaster_id
            .as_deref()
            .is_some_and(|b| b == user_id);
        let name = if broadcaster {
            show.broadcaster_name
                .clone()
                .unwrap_or_else(|| anon_name(&user_id))
        } else {
            anon_name(&user_id)
        };

        // uuid が無いため ts+userId を ID 兼 dedup キーにする。ts はナノ秒
        // 精度なので同一ユーザーの衝突は実質起きない。
        let id = format!("x-{ts_raw}-{user_id}");

        Some(ChatMessage {
            id,
            platform: Platform::X,
            channel: self.broadcast_id.clone(),
            author: Author {
                id: user_id,
                name,
                display_color: None,
                badges: Vec::new(),
                roles: Roles {
                    broadcaster,
                    ..Roles::default()
                },
            },
            fragments: vec![Fragment::text(text.to_string())],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms,
            raw: None,
            skip_tts: false,
        })
    }

    /// guest_token を取り、show.json から配信メタデータを得る。
    async fn fetch_show(&self, client: &reqwest::Client) -> anyhow::Result<ShowInfo> {
        let bearer = self.bearer();

        // 1. ゲストトークン。show.json は guest_token 必須。
        let v: Value = client
            .post(self.endpoint_url("guestActivateUrl", GUEST_ACTIVATE_URL))
            .timeout(HTTP_TIMEOUT)
            .header("authorization", format!("Bearer {bearer}"))
            .header("origin", "https://x.com")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let guest_token = v
            .get("guest_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("guest_token が取得できない"))?
            .to_string();

        // 2. broadcast メタデータ。
        // ID は外部入力なので手組み連結せず .query() でエンコードさせる。
        let v: Value = client
            .get(self.endpoint_url("broadcastShowUrl", BROADCAST_SHOW_URL))
            .timeout(HTTP_TIMEOUT)
            .query(&[("ids", self.broadcast_id.as_str())])
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-guest-token", &guest_token)
            .header("origin", "https://x.com")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let broadcast = v
            .pointer(&format!("/broadcasts/{}", self.broadcast_id))
            .cloned()
            .unwrap_or(Value::Null);
        if broadcast.is_null() {
            anyhow::bail!("broadcast が見つからない(ID 誤りか削除済みの可能性)");
        }
        let state = broadcast
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // live-chat の userId と同じ空間なのは twitter_user_id(user_id は
        // Periscope ID なので使わない)。
        let broadcaster_id = id_string(broadcast.get("twitter_user_id"));
        let broadcaster_name = broadcast
            .get("user_display_name")
            .or_else(|| broadcast.get("username"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // Periscope 由来の "status" が配信タイトル。将来の変化に備え "title" も見る。
        let title = broadcast
            .get("status")
            .or_else(|| broadcast.get("title"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // total_watching は文字列で返る("269")。数値で来ても受ける。
        let viewers = broadcast.get("total_watching").and_then(|t| match t {
            Value::String(s) => s.trim().parse::<u32>().ok(),
            Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
            _ => None,
        });

        Ok(ShowInfo {
            broadcaster_id,
            broadcaster_name,
            title,
            viewers,
            state,
        })
    }

    /// チップの接続状態表示用に live 状態を stats へ送る。
    /// `full_snapshot: false` の部分更新なので、切断時も直前のタイトルは
    /// stats 側の live=false 処理に従って整理される。
    async fn send_live_state(&self, live: bool, title: Option<String>, viewers: Option<u32>) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::X,
            channel: self.broadcast_id.clone(),
            concurrent_viewers: viewers,
            likes: None,
            title,
            live: Some(live),
            reactions_delta: None,
            full_snapshot: false,
        };
        let _ = tx.send(update).await;
    }
}

/// ID フィールドを寛容に文字列化する(文字列/数値の両方を受ける)。
fn id_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// username が解決できないユーザーの表示名。userId の下4桁で個体識別だけ
/// できるようにする(NG 等の同定は author.id=userId 全体で行われる)。
fn anon_name(user_id: &str) -> String {
    let tail: String = user_id
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("ユーザー{tail}")
}

/// 受信ループから渡されたチャットを(可能なら)実名に差し替えて broadcast へ
/// 流すタスク。cookie 未設定なら素通し。FIFO 処理なので表示順は保たれる。
/// 解決の失敗・タイムアウトは匿名のまま流し、チャット自体は決して止めない。
async fn name_resolver_task(
    mut rx: mpsc::Receiver<ChatMessage>,
    tx: broadcast::Sender<ChatMessage>,
    mut resolver: Option<NameResolver>,
    broadcast_id: String,
) {
    let mut warned_no_subscriber = false;
    while let Some(first) = rx.recv().await {
        // キューに溜まっている分を吸い出してまとめて1クエリで解決する
        // (バッチ待ち時間は入れない: 1件目の表示を遅らせない)。
        let mut batch = vec![first];
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
            if batch.len() >= RESOLVE_QUEUE_CAPACITY {
                break;
            }
        }
        if let Some(r) = resolver.as_mut() {
            let mut unknown: Vec<String> = Vec::new();
            for m in &batch {
                // 配信者は show.json の名前で解決済み。
                if m.author.roles.broadcaster {
                    continue;
                }
                if !r.cache.contains_key(&m.author.id) && !unknown.contains(&m.author.id) {
                    unknown.push(m.author.id.clone());
                }
            }
            if !unknown.is_empty() {
                r.resolve_batch(&unknown).await;
            }
            for m in &mut batch {
                if m.author.roles.broadcaster {
                    continue;
                }
                if let Some(name) = r.cache.get(&m.author.id) {
                    m.author.name = name.clone();
                }
            }
            if r.disabled {
                tracing::warn!(
                    "x:{broadcast_id} 名前解決を無効化(cookie 失効?) — 匿名表示で継続"
                );
                resolver = None;
            }
        }
        for m in batch {
            // 購読者ゼロだと send は Err になる。無言のままだと
            // 「受信はしているのに UI に届かない」を切り分けられない。
            if tx.send(m).is_err() && !warned_no_subscriber {
                warned_no_subscriber = true;
                tracing::warn!(
                    "x:{broadcast_id} tx.send 失敗(購読者なし) — パイプライン未接続の疑い"
                );
            }
        }
    }
}

/// userId→表示名の解決器。GraphQL liveAtomsUserQuery(要ログイン cookie)を
/// バッチで叩いて cache に貯める。認証失敗(401/403)で自身を無効化する。
struct NameResolver {
    client: reqwest::Client,
    url: String,
    bearer: String,
    auth: XAuth,
    cache: HashMap<String, String>,
    disabled: bool,
}

impl NameResolver {
    fn new(client: reqwest::Client, url: String, bearer: String, auth: XAuth) -> Self {
        NameResolver {
            client,
            url,
            bearer,
            auth,
            cache: HashMap::new(),
            disabled: false,
        }
    }

    /// ids をまとめて解決し cache へ。失敗はログのみ(呼び出し側は匿名で続行)。
    async fn resolve_batch(&mut self, ids: &[String]) {
        if self.disabled {
            return;
        }
        for chunk in ids.chunks(NAME_RESOLVE_BATCH_MAX) {
            let variables = json!({ "userIds": chunk }).to_string();
            let resp = self
                .client
                .get(&self.url)
                .timeout(NAME_RESOLVE_TIMEOUT)
                .query(&[("variables", variables.as_str())])
                .header("authorization", format!("Bearer {}", self.bearer))
                .header("x-csrf-token", &self.auth.csrf_token)
                .header(
                    "cookie",
                    format!(
                        "auth_token={}; ct0={}",
                        self.auth.auth_token, self.auth.csrf_token
                    ),
                )
                .send()
                .await
                .and_then(reqwest::Response::error_for_status);
            match resp {
                Ok(resp) => match resp.json::<Value>().await {
                    Ok(v) => self.absorb(&v),
                    Err(e) => tracing::debug!("liveAtomsUserQuery JSON パース失敗: {e:#}"),
                },
                Err(e) => {
                    // cookie 失効は回復しない。以降のクエリを止めて匿名運転へ。
                    if matches!(e.status().map(|s| s.as_u16()), Some(401) | Some(403)) {
                        self.disabled = true;
                        return;
                    }
                    tracing::debug!("liveAtomsUserQuery 失敗: {e:#}");
                }
            }
        }
        // 際限なく貯めない保険。1配信の発言者数では実質届かない。
        if self.cache.len() > NAME_CACHE_MAX {
            self.cache.clear();
        }
    }

    /// liveAtomsUserQuery 応答から rest_id→表示名を取り込む。
    /// 形: data.users[].result.{rest_id, core:{name, screen_name}}(実測)。
    fn absorb(&mut self, v: &Value) {
        let Some(users) = v.pointer("/data/users").and_then(Value::as_array) else {
            return;
        };
        for u in users {
            let Some(result) = u.get("result") else { continue };
            let Some(id) = id_string(result.get("rest_id")) else { continue };
            let name = result
                .pointer("/core/name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    result
                        .pointer("/core/screen_name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                });
            if let Some(n) = name {
                self.cache.insert(id, n.to_string());
            }
        }
    }
}

/// 直近チャット ID の FIFO 集合。再接続のたびに live-chat はバックフィルを
/// 全量再送するため、既出 ID を弾いて二重表示を防ぐ。容量超過分は古い順に捨てる。
struct ChatDedup {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
    capacity: usize,
}

impl ChatDedup {
    fn new(capacity: usize) -> Self {
        ChatDedup {
            set: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// 新規 ID なら登録して true、既出なら false を返す。
    fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// 1ストリームセッションの受信統計。Drop で必ずサマリを info ログへ残し、
/// 「接続はできるがチャットが来ない」を現地で診断可能にする。
struct SessionStats {
    broadcast_id: String,
    started: Instant,
    /// 処理した NDJSON 行数(パース失敗含む)。
    lines: u64,
    /// UI へ emit したチャット数(バックフィル含む)。
    chats: u64,
    /// isBackfill 付きで受信した行数(dedup 通過分)。
    backfill_seen: u64,
    /// BACKFILL_EMIT_MAX 超過で捨てたバックフィル数。
    backfill_skipped: u64,
    /// dedup で既出のため捨てた数(再接続のバックフィル再送など)。
    dup: u64,
    /// JSON パース/正規化失敗の行数。
    parse_errors: u64,
    /// 名前解決キュー満杯で捨てたチャット数。
    queue_full: u64,
    /// show.json 再取得の成功/失敗回数。
    show_polls: u64,
    show_errors: u64,
    /// chatType 別の非チャット行数(1 以外)。キーは chatType(欠落は -1)。
    non_chat: std::collections::HashMap<i64, u64>,
}

impl SessionStats {
    fn new(broadcast_id: &str) -> Self {
        SessionStats {
            broadcast_id: broadcast_id.to_string(),
            started: Instant::now(),
            lines: 0,
            chats: 0,
            backfill_seen: 0,
            backfill_skipped: 0,
            dup: 0,
            parse_errors: 0,
            queue_full: 0,
            show_polls: 0,
            show_errors: 0,
            non_chat: std::collections::HashMap::new(),
        }
    }

    /// Twitch と同じ「安定セッション」判定(チャット実受信 or 30秒以上継続)。
    fn is_stable(&self) -> bool {
        self.chats > 0 || self.started.elapsed().as_secs() >= 30
    }

    fn summary(&self) -> String {
        format!(
            "{}秒 行:{} chat:{} backfill:{}(切捨:{}) 重複:{} パース失敗:{} show(ok:{} err:{}) キュー溢れ:{} 非チャット:{:?}",
            self.started.elapsed().as_secs(),
            self.lines,
            self.chats,
            self.backfill_seen,
            self.backfill_skipped,
            self.dup,
            self.parse_errors,
            self.show_polls,
            self.show_errors,
            self.queue_full,
            self.non_chat,
        )
    }

    /// 生存中の定期サマリ。「接続は生きているがチャットが来ない」の可視化。
    fn log_progress(&self) {
        tracing::info!("x:{} 接続中 {}", self.broadcast_id, self.summary());
    }
}

impl Drop for SessionStats {
    fn drop(&mut self) {
        tracing::info!("x:{} セッション終了: {}", self.broadcast_id, self.summary());
        // 30秒以上生きて行は届いているのにチャット0件は、プロトコル変化
        // (chatType 体系や行スキーマの変更)の疑いが強い。
        if self.chats == 0 && self.lines > 0 && self.started.elapsed().as_secs() >= 30 {
            tracing::warn!(
                "x:{} 行は受信しているのにチャット0件のままセッション終了(スキーマ変化の疑い)",
                self.broadcast_id,
            );
        }
    }
}

/// UTF-8 文字境界を守って先頭 max 文字へ切り詰める(ログ用)。
fn truncate_chars(s: &str, max: usize) -> String {
    let mut t: String = s.chars().take(max).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// 秒/ミリ秒/ナノ秒が混在しうる epoch 値をミリ秒へ寄せる。
fn normalize_epoch_ms(v: i64) -> i64 {
    if v <= 0 {
        return now_ms();
    }
    if v >= 1_000_000_000_000_000 {
        // ナノ秒(live-chat の ts)。
        v / 1_000_000
    } else if v < 100_000_000_000 {
        // 秒。
        v.saturating_mul(1000)
    } else {
        v
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show_info(broadcaster_id: &str, name: &str) -> ShowInfo {
        ShowInfo {
            broadcaster_id: Some(broadcaster_id.to_string()),
            broadcaster_name: Some(name.to_string()),
            title: Some("t".to_string()),
            viewers: Some(1),
            state: "RUNNING".to_string(),
        }
    }

    #[test]
    fn extracts_broadcast_id_from_urls_and_raw_ids() {
        assert_eq!(
            extract_broadcast_id("https://x.com/i/broadcasts/1yoJMWvbtbtxQ"),
            "1yoJMWvbtbtxQ"
        );
        assert_eq!(
            extract_broadcast_id("https://twitter.com/i/broadcasts/1yoJMWvbtbtxQ?s=20"),
            "1yoJMWvbtbtxQ"
        );
        assert_eq!(extract_broadcast_id("  1yoJMWvbtbtxQ  "), "1yoJMWvbtbtxQ");
    }

    #[test]
    fn normalizes_epoch_units() {
        assert_eq!(normalize_epoch_ms(1_700_000_000), 1_700_000_000_000); // 秒
        assert_eq!(normalize_epoch_ms(1_700_000_000_000), 1_700_000_000_000); // ミリ秒
        assert_eq!(
            normalize_epoch_ms(1_700_000_000_000_000_000),
            1_700_000_000_000
        ); // ナノ秒
    }

    #[test]
    fn chat_line_normalizes_to_message() {
        let src = XSource::new("1yoJMWvbtbtxQ".to_string(), XOverrides::default(), None, None);
        let show = show_info("999", "Host");
        let v: Value = serde_json::from_str(
            r#"{"userId":"1234567890","chatType":1,"message":"こんにちは","ts":"1700000000123000000"}"#,
        )
        .unwrap();
        let msg = src.line_to_chat(&v, &show).expect("chat message");
        assert_eq!(msg.platform, Platform::X);
        assert_eq!(msg.channel, "1yoJMWvbtbtxQ");
        assert_eq!(msg.author.id, "1234567890");
        assert_eq!(msg.author.name, "ユーザー7890");
        assert_eq!(msg.plain_text(), "こんにちは");
        assert_eq!(msg.timestamp_ms, 1_700_000_000_123); // ナノ秒 → ms
        assert_eq!(msg.id, "x-1700000000123000000-1234567890");
        assert!(!msg.author.roles.broadcaster);
    }

    #[test]
    fn broadcaster_gets_display_name_and_role() {
        let src = XSource::new("b1".to_string(), XOverrides::default(), None, None);
        let show = show_info("999", "Host");
        let v: Value = serde_json::from_str(
            r#"{"userId":"999","chatType":1,"message":"hi","ts":"1700000000000000000"}"#,
        )
        .unwrap();
        let msg = src.line_to_chat(&v, &show).expect("chat message");
        assert!(msg.author.roles.broadcaster);
        assert_eq!(msg.author.name, "Host");
    }

    #[test]
    fn numeric_ts_and_user_id_are_accepted() {
        // ts / userId が数値で来る個体にも耐える(寛容パース)。
        let src = XSource::new("b1".to_string(), XOverrides::default(), None, None);
        let show = show_info("999", "Host");
        let v: Value = serde_json::from_str(
            r#"{"userId":42,"chatType":1,"message":"hi","ts":1700000000123}"#,
        )
        .unwrap();
        let msg = src.line_to_chat(&v, &show).expect("chat message");
        assert_eq!(msg.author.id, "42");
        assert_eq!(msg.author.name, "ユーザー42");
        assert_eq!(msg.timestamp_ms, 1_700_000_000_123); // ms はそのまま
    }

    #[test]
    fn empty_message_or_missing_user_is_rejected() {
        let src = XSource::new("b1".to_string(), XOverrides::default(), None, None);
        let show = show_info("999", "Host");
        let v: Value =
            serde_json::from_str(r#"{"userId":"1","chatType":1,"message":"  ","ts":"1"}"#).unwrap();
        assert!(src.line_to_chat(&v, &show).is_none());
        let v: Value =
            serde_json::from_str(r#"{"chatType":1,"message":"hi","ts":"1"}"#).unwrap();
        assert!(src.line_to_chat(&v, &show).is_none());
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        assert_eq!(truncate_chars("あいうえお", 3), "あいう…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn dedup_rejects_repeats_and_evicts_oldest() {
        let mut d = ChatDedup::new(2);
        assert!(d.insert("a"));
        assert!(d.insert("b"));
        assert!(!d.insert("a")); // 既出は false。
        assert!(d.insert("c")); // 容量2超過で最古の "a" が捨てられる。
        assert!(d.insert("a")); // "a" は捨てられたので再び新規扱い。
    }

    #[test]
    fn name_resolver_absorbs_graphql_users() {
        let mut r = NameResolver::new(
            reqwest::Client::new(),
            "http://127.0.0.1/q".to_string(),
            "b".to_string(),
            XAuth {
                auth_token: "a".to_string(),
                csrf_token: "c".to_string(),
            },
        );
        // 実測形: data.users[].result.{rest_id, core:{name, screen_name}}。
        // name 空は screen_name へフォールバック、rest_id 欠落はスキップ。
        let v: Value = serde_json::from_str(
            r#"{"data":{"users":[
                {"result":{"rest_id":"1","core":{"name":"Alice","screen_name":"alice_x"}}},
                {"result":{"rest_id":"2","core":{"name":"  ","screen_name":"bob_x"}}},
                {"result":{"core":{"name":"NoId"}}}
            ]}}"#,
        )
        .unwrap();
        r.absorb(&v);
        assert_eq!(r.cache.get("1").map(String::as_str), Some("Alice"));
        assert_eq!(r.cache.get("2").map(String::as_str), Some("bob_x"));
        assert_eq!(r.cache.len(), 2);
    }

    /// cookie 設定時の名前解決 E2E: live-chat 受信 → liveAtomsUserQuery
    /// (cookie/csrf ヘッダ付き)→ 実名で emit。
    #[tokio::test]
    async fn resolves_usernames_via_graphql_when_cookie_is_set() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16384];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    if path.starts_with("/activate") {
                        let body = r#"{"guest_token":"g1"}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else if path.starts_with("/show") {
                        let body = r#"{"broadcasts":{"b1":{"state":"RUNNING","twitter_user_id":"99","user_display_name":"Host","status":"t","total_watching":"1"}}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else if path.starts_with("/live-chat") {
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream
                            .write_all(b"{\"userId\":\"7\",\"chatType\":1,\"message\":\"hello\",\"ts\":\"1700000000000001000\"}\n")
                            .await;
                        // クライアント側の検証が終わるまで接続を保つ。
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } else if path.starts_with("/userquery") {
                        // reqwest はヘッダ名を小文字で送る。cookie と csrf の実値を検証。
                        let lower = req.to_lowercase();
                        assert!(
                            lower.contains("x-csrf-token: ct-test"),
                            "csrf ヘッダがない: {req}"
                        );
                        assert!(
                            lower.contains("auth_token=at-test"),
                            "cookie がない: {req}"
                        );
                        assert!(path.contains("7"), "userIds に 7 がない: {path}");
                        let body = r#"{"data":{"users":[{"result":{"rest_id":"7","core":{"name":"Seven","screen_name":"seven_x"}}}]}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else {
                        let _ = stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });

        let mut overrides = XOverrides::default();
        let base = format!("http://127.0.0.1:{port}");
        overrides
            .endpoints
            .insert("guestActivateUrl".to_string(), format!("{base}/activate"));
        overrides
            .endpoints
            .insert("broadcastShowUrl".to_string(), format!("{base}/show"));
        overrides
            .endpoints
            .insert("liveChatUrl".to_string(), format!("{base}/live-chat"));
        overrides
            .endpoints
            .insert("userQueryUrl".to_string(), format!("{base}/userquery"));

        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let auth = XAuth {
            auth_token: "at-test".to_string(),
            csrf_token: "ct-test".to_string(),
        };
        let src = XSource::new("b1".to_string(), overrides, Some(auth), None);
        let cancel_run = cancel.clone();
        let run = tokio::spawn(async move { src.run(tx, cancel_run).await });

        let m = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("チャットタイムアウト")
            .expect("channel closed");
        assert_eq!(m.plain_text(), "hello");
        assert_eq!(m.author.id, "7");
        assert_eq!(m.author.name, "Seven"); // GraphQL で実名に解決される。
        assert!(!m.author.roles.broadcaster);

        cancel.cancel();
        let _ = run.await;
    }

    /// fake bootstrap HTTP + fake live-chat ストリームに対する結合テスト。
    /// guest activate → show.json → NDJSON ストリーム受信 → バックフィルの
    /// 上限切り捨てと TTS 抑制 → 非バックフィル到着での flush → ストリーミング
    /// 継続受信、をライブ配信なしで検証する。
    #[tokio::test]
    async fn end_to_end_receives_backfill_and_live_chat_via_fake_servers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                    if path.starts_with("/activate") {
                        let body = r#"{"guest_token":"g1"}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else if path.starts_with("/show") {
                        let body = r#"{"broadcasts":{"b1":{"media_key":"mk1","state":"RUNNING","twitter_user_id":"99","username":"host","user_display_name":"Host","status":"t","total_watching":"42"}}}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(), body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else if path.starts_with("/live-chat") {
                        assert!(
                            path.contains("broadcastId=b1"),
                            "broadcastId クエリがない: {path}"
                        );
                        // content-length なし + connection: close の逐次書き込みで
                        // ストリーミング(EOF 終端)を再現する。
                        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\nconnection: close\r\n\r\n";
                        let _ = stream.write_all(head.as_bytes()).await;
                        // バックフィル 25 件(上限 20 で古い 5 件が切り捨てられる)。
                        for i in 1..=25 {
                            let line = format!(
                                "{{\"userId\":\"1000{i}\",\"chatType\":1,\"message\":\"bf{i}\",\"ts\":\"17000000000000000{i:02}\",\"isBackfill\":true}}\n"
                            );
                            let _ = stream.write_all(line.as_bytes()).await;
                        }
                        // 非チャット行(モデレーション系メタ)は無視されること。
                        let _ = stream
                            .write_all(b"{\"userId\":\"9\",\"chatType\":39,\"message\":\"1700000000000000000\",\"ts\":\"1700000000000000001\"}\n")
                            .await;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        // 非バックフィル(=ライブ新着)。配信者本人のコメント。
                        let _ = stream
                            .write_all(b"{\"userId\":\"99\",\"chatType\":1,\"message\":\"live1\",\"ts\":\"1700000000000001000\"}\n")
                            .await;
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        // ストリーミング継続の検証(接続維持中の追加 push)。
                        let _ = stream
                            .write_all(b"{\"userId\":\"7\",\"chatType\":1,\"message\":\"live2\",\"ts\":\"1700000000000002000\"}\n")
                            .await;
                        // クライアント側の検証が終わるまで接続を保つ。
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } else {
                        let _ = stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                    }
                    let _ = stream.shutdown().await;
                });
            }
        });

        let mut overrides = XOverrides::default();
        let base = format!("http://127.0.0.1:{port}");
        overrides
            .endpoints
            .insert("guestActivateUrl".to_string(), format!("{base}/activate"));
        overrides
            .endpoints
            .insert("broadcastShowUrl".to_string(), format!("{base}/show"));
        overrides
            .endpoints
            .insert("liveChatUrl".to_string(), format!("{base}/live-chat"));

        let (tx, mut rx) = broadcast::channel(64);
        let (meta_tx, mut meta_rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();
        let src = XSource::new("b1".to_string(), overrides, None, Some(meta_tx));
        let cancel_run = cancel.clone();
        let run = tokio::spawn(async move { src.run(tx, cancel_run).await });

        // 接続直後の live 状態(タイトル/視聴者数)がチップへ届く。
        let meta = tokio::time::timeout(Duration::from_secs(10), meta_rx.recv())
            .await
            .expect("metadata タイムアウト")
            .expect("metadata channel closed");
        assert_eq!(meta.platform, Platform::X);
        assert_eq!(meta.live, Some(true));
        assert_eq!(meta.title.as_deref(), Some("t"));
        assert_eq!(meta.concurrent_viewers, Some(42));

        // バックフィル 25 件中、新しい 20 件(bf6..bf25、skip_tts=true)→
        // live1(配信者、skip_tts=false)→ live2 の順で計 22 件届く。
        let mut msgs = Vec::new();
        for i in 0..22 {
            let m = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{}件目タイムアウト", i + 1))
                .expect("channel closed");
            msgs.push(m);
        }
        let texts: Vec<String> = msgs.iter().map(|m| m.plain_text()).collect();
        // 古い 5 件(bf1..bf5)は上限で切り捨て。
        assert!(!texts.contains(&"bf1".to_string()), "texts={texts:?}");
        assert!(!texts.contains(&"bf5".to_string()), "texts={texts:?}");
        assert_eq!(texts[0], "bf6");
        assert_eq!(texts[19], "bf25");
        assert_eq!(texts[20], "live1");
        assert_eq!(texts[21], "live2");
        // バックフィルは TTS 抑制、ライブ新着は読み上げ対象。
        assert!(msgs[..20].iter().all(|m| m.skip_tts));
        assert!(!msgs[20].skip_tts);
        assert!(!msgs[21].skip_tts);
        // 配信者判定と名前解決(twitter_user_id=99 → "Host")。
        assert!(msgs[20].author.roles.broadcaster);
        assert_eq!(msgs[20].author.name, "Host");
        // 匿名ユーザーは userId 下4桁表示。
        assert_eq!(msgs[21].author.name, "ユーザー7");
        assert_eq!(msgs[0].channel, "b1");

        cancel.cancel();
        let _ = run.await;
    }
}
