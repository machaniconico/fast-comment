//! PR2 受け入れテスト: `GET /events`(SSE)(設計書 §1.4 / §5.4)。
//!
//! 実ソケット(127.0.0.1:0)+ 時刻停止。`tokio::time::advance` で tick / keep-alive を進める。
//! 時刻停止中に「実 I/O の完了を待つだけの `.await`」を書くと、ランタイムが暇と見なされて
//! 仮想時刻が勝手に進み keep-alive が誤発火する。そのため待ちはすべて `pump()`
//! (`yield_now` + 実時間 1ms)で行い、ソケットの読み取りは別タスクに任せる。

use crate::common::*;
use axum::http::StatusCode;
use axum::Router;
use fast_comment_lib::api::{self, ApiDeps, ApiFeed, ApiGate};
use fast_comment_lib::model::{ModerationEvent, Platform};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const TICK: Duration = Duration::from_millis(17); // 16ms の tick をちょうど 1 回またぐ

// ---- 時間と待ち ----

async fn pump() {
    for _ in 0..40 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

async fn advance_pump(d: Duration) {
    tokio::time::advance(d).await;
    pump().await;
}

/// 17ms ずつ `n` 回進める(再送や複数イベントが tick をまたいで出ても拾う)。
async fn settle_ticks(n: usize) {
    for _ in 0..n {
        advance_pump(TICK).await;
    }
}

// ---- サーバ ----

struct Srv {
    port: u16,
    app: Router,
    feed: ApiFeed,
    gate: ApiGate,
    token: String,
    shutdown: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

async fn start(capacity: usize) -> Srv {
    let (gate, token) = open_gate();
    let feed = ApiFeed::new(capacity);
    feed.set_enabled(true);
    let shutdown = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let deps = ApiDeps::new(gate.clone(), shutdown.clone()).with_feed(feed.clone());
    let app = api::router(deps, port);
    let serve_app = app.clone();
    let stop = shutdown.clone();
    let handle = tokio::spawn(async move {
        axum::serve(listener, serve_app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .expect("serve");
    });
    Srv { port, app, feed, gate, token, shutdown, handle: Some(handle) }
}

impl Srv {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
    /// ルータへ直接 GET(実ソケットを通さない)。
    async fn get(&self, path: &str) -> Resp {
        send(&self.app, get(path).host(&self.host()).bearer(&self.token)).await
    }
    async fn epoch(&self) -> String {
        self.get("/api/v1/status").await.json()["epoch"].as_str().unwrap().to_string()
    }
    /// 最新 `limit` 件の seq を古い順に。
    async fn cursor_of_nth(&self, nth_from_oldest: usize) -> String {
        let epoch = self.epoch().await;
        let j = self.get("/api/v1/comments?limit=500").await.json();
        let seq = j["items"][nth_from_oldest]["seq"].as_u64().expect("seq");
        format!("{epoch}.{seq}")
    }
    async fn head_cursor(&self) -> String {
        self.get("/api/v1/comments?limit=1").await.json()["nextCursor"].as_str().unwrap().to_string()
    }
    fn client(&self, path_and_query: &str, last_event_id: Option<&str>) -> Sse {
        Sse::spawn(self.port, &self.token, path_and_query, last_event_id)
    }
    async fn open(&self, path_and_query: &str, last_event_id: Option<&str>) -> Sse {
        let sse = self.client(path_and_query, last_event_id);
        sse.ready().await;
        sse
    }
}

// ---- SSE クライアント ----

#[derive(Default)]
struct St {
    status: Option<u16>,
    headers: Vec<(String, String)>,
    buf: Vec<u8>,
    ended: bool,
}

struct Sse {
    st: Arc<Mutex<St>>,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct Ev {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
    comment: Option<String>,
}

impl Ev {
    fn json(&self) -> Value {
        serde_json::from_str(self.data.as_deref().expect("data 行がある")).expect("data は JSON")
    }
    fn is(&self, name: &str) -> bool {
        self.event.as_deref() == Some(name)
    }
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Sse {
    fn spawn(port: u16, token: &str, path_and_query: &str, last_event_id: Option<&str>) -> Sse {
        let st = Arc::new(Mutex::new(St::default()));
        let st2 = st.clone();
        let url = format!("http://127.0.0.1:{port}{path_and_query}");
        let token = token.to_string();
        let leid = last_event_id.map(|s| s.to_string());
        let task = tokio::spawn(async move {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let mut rb = client
                .get(&url)
                .header("authorization", format!("Bearer {token}"))
                .header("accept", "text/event-stream");
            if let Some(v) = leid {
                rb = rb.header("last-event-id", v);
            }
            let mut resp = match rb.send().await {
                Ok(r) => r,
                Err(_) => {
                    st2.lock().unwrap().ended = true;
                    return;
                }
            };
            {
                let mut s = st2.lock().unwrap();
                s.status = Some(resp.status().as_u16());
                s.headers = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                    .collect();
            }
            loop {
                match resp.chunk().await {
                    Ok(Some(b)) => st2.lock().unwrap().buf.extend_from_slice(&b),
                    _ => break,
                }
            }
            st2.lock().unwrap().ended = true;
        });
        Sse { st, task }
    }

    /// 応答ヘッダが届くまで pump する。
    async fn ready(&self) {
        for _ in 0..200 {
            if self.st.lock().unwrap().status.is_some() {
                return;
            }
            pump().await;
        }
        panic!("応答ヘッダが届かない");
    }

    fn status(&self) -> u16 {
        self.st.lock().unwrap().status.expect("ready 済み")
    }
    fn header(&self, name: &str) -> Option<String> {
        self.st
            .lock()
            .unwrap()
            .headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
    fn ended(&self) -> bool {
        self.st.lock().unwrap().ended
    }
    fn raw_body(&self) -> String {
        String::from_utf8_lossy(&self.st.lock().unwrap().buf).into_owned()
    }

    /// 完結したイベント(空行で終わるブロック)を取り出す。未完の末尾は残す。
    fn drain(&self) -> Vec<Ev> {
        let mut s = self.st.lock().unwrap();
        let text = String::from_utf8_lossy(&s.buf).replace("\r\n", "\n");
        let Some(end) = text.rfind("\n\n") else { return vec![] };
        let (done, rest) = text.split_at(end + 2);
        let rest = rest.as_bytes().to_vec();
        let mut out = vec![];
        for block in done.split("\n\n").filter(|b| !b.trim().is_empty()) {
            let mut ev = Ev { event: None, data: None, id: None, comment: None };
            for line in block.lines() {
                if let Some(c) = line.strip_prefix(':') {
                    ev.comment = Some(c.trim().to_string());
                } else if let Some(v) = line.strip_prefix("event:") {
                    ev.event = Some(v.trim_start().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    let v = v.strip_prefix(' ').unwrap_or(v);
                    ev.data = Some(match ev.data.take() {
                        Some(prev) => format!("{prev}\n{v}"),
                        None => v.to_string(),
                    });
                } else if let Some(v) = line.strip_prefix("id:") {
                    ev.id = Some(v.trim_start().to_string());
                }
            }
            out.push(ev);
        }
        s.buf = rest;
        out
    }
}

fn chat_items(evs: &[Ev]) -> Vec<Value> {
    evs.iter()
        .filter(|e| e.is("chat"))
        .flat_map(|e| e.json()["items"].as_array().expect("items").clone())
        .collect()
}

fn item_ids(items: &[Value]) -> Vec<String> {
    items.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

fn names(prefix: &str, range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    range.map(|i| format!("{prefix}{i}")).collect()
}

fn push_n(srv: &Srv, prefix: &str, range: std::ops::RangeInclusive<usize>) {
    for i in range {
        srv.feed.push_chat(&msg(&format!("{prefix}{i}")));
    }
}

const EVENTS: &str = "/api/v1/events";

// ---- 応答の体裁・基本配信 ----

#[tokio::test(start_paused = true)]
async fn events_content_type_is_text_event_stream_and_no_store() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    assert_eq!(sse.status(), 200);
    assert!(
        sse.header("content-type").unwrap_or_default().starts_with("text/event-stream"),
        "content-type: {:?}",
        sse.header("content-type")
    );
    assert!(
        sse.header("cache-control").unwrap_or_default().contains("no-store"),
        "cache-control: {:?}",
        sse.header("cache-control")
    );
}

#[tokio::test(start_paused = true)]
async fn events_deliver_chat_event_with_items_and_cursor_id() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain();

    srv.feed.push_chat(&msg("m1"));
    advance_pump(TICK).await;

    let evs = sse.drain();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert!(evs[0].is("chat"));
    let j = evs[0].json();
    let items = j["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], "m1");
    let seq = items[0]["seq"].as_u64().unwrap();
    assert_eq!(evs[0].id.as_deref(), Some(format!("{}.{seq}", srv.epoch().await).as_str()));
}

#[tokio::test(start_paused = true)]
async fn events_batch_messages_pushed_within_one_tick_into_one_event() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain();

    push_n(&srv, "m", 1..=5);
    advance_pump(TICK).await;

    let evs = sse.drain();
    assert_eq!(evs.len(), 1, "5 件で 1 イベント: {evs:?}");
    assert!(evs[0].is("chat"));
    assert_eq!(item_ids(&chat_items(&evs)), names("m", 1..=5));
}

#[tokio::test(start_paused = true)]
async fn events_send_nothing_on_idle_ticks() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain();

    advance_pump(Duration::from_millis(500)).await;
    let evs = sse.drain();
    assert!(evs.is_empty(), "空の tick では何も送らない: {evs:?}");
}

#[tokio::test(start_paused = true)]
async fn events_send_moderation_after_chat_in_same_tick() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain();

    // 到着順は moderation が先。それでも同じ tick では chat → moderation の順。
    srv.feed.push_moderation(&ModerationEvent::MessageDeleted {
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        message_id: "gone".to_string(),
    });
    srv.feed.push_chat(&msg("m1"));
    advance_pump(TICK).await;

    let evs = sse.drain();
    let order: Vec<_> = evs.iter().map(|e| e.event.clone().unwrap_or_default()).collect();
    assert_eq!(order, vec!["chat", "moderation"], "{evs:?}");
    let m = evs[1].json();
    assert_eq!(m["items"][0]["type"], "messageDeleted");
    assert_eq!(m["items"][0]["messageId"], "gone");
    assert!(evs[1].id.is_none(), "moderation に id は付けない");
}

// ---- 再開 ----

#[tokio::test(start_paused = true)]
async fn events_resume_with_last_event_id_replays_then_goes_live_without_gap_or_duplicate() {
    let srv = start(1000).await;
    push_n(&srv, "m", 1..=10);
    let c3 = srv.cursor_of_nth(2).await;

    let sse = srv.client(EVENTS, Some(&c3));
    // 接続処理と再送の最中に新着を入れる(再送集合に入るか生配信で来るかは実装次第。どちらでも欠落・重複なし)
    push_n(&srv, "m", 11..=13);
    sse.ready().await;
    push_n(&srv, "m", 14..=15);
    settle_ticks(10).await;
    push_n(&srv, "m", 16..=17);
    settle_ticks(5).await;

    let evs = sse.drain();
    assert_eq!(item_ids(&chat_items(&evs)), names("m", 4..=17));
}

#[tokio::test(start_paused = true)]
async fn events_after_query_works_and_header_wins_over_query() {
    let srv = start(1000).await;
    push_n(&srv, "m", 1..=10);
    let c3 = srv.cursor_of_nth(2).await;
    let c7 = srv.cursor_of_nth(6).await;

    let by_query = srv.open(&format!("{EVENTS}?after={c7}"), None).await;
    settle_ticks(5).await;
    assert_eq!(item_ids(&chat_items(&by_query.drain())), names("m", 8..=10));

    let both = srv.open(&format!("{EVENTS}?after={c7}"), Some(&c3)).await;
    settle_ticks(5).await;
    assert_eq!(item_ids(&chat_items(&both.drain())), names("m", 4..=10), "ヘッダが優先");
}

#[tokio::test(start_paused = true)]
async fn events_replay_is_chunked_to_500_items_per_event() {
    let srv = start(2000).await;
    srv.feed.push_chat(&msg("seed"));
    let seed = srv.head_cursor().await;
    push_n(&srv, "m", 1..=1200);

    let sse = srv.open(EVENTS, Some(&seed)).await;
    settle_ticks(20).await;

    let evs = sse.drain();
    let chat: Vec<&Ev> = evs.iter().filter(|e| e.is("chat")).collect();
    assert!(chat.len() >= 3, "1200 件は 3 イベント以上に分かれる: {}", chat.len());
    for e in &chat {
        assert!(e.json()["items"].as_array().unwrap().len() <= 500, "1 イベント 500 件まで");
    }
    assert_eq!(item_ids(&chat_items(&evs)), names("m", 1..=1200));
}

#[tokio::test(start_paused = true)]
async fn events_stale_cursor_emits_lagged_before_replay() {
    let srv = start(1000).await;
    srv.feed.push_chat(&msg("seed"));
    let seed = srv.head_cursor().await;
    push_n(&srv, "m", 1..=1500);

    let sse = srv.open(EVENTS, Some(&seed)).await;
    settle_ticks(20).await;

    let evs = sse.drain();
    assert!(evs.first().map(|e| e.is("lagged")).unwrap_or(false), "先頭は lagged: {:?}", evs.first());
    assert!(evs[0].json()["dropped"].is_u64(), "dropped は数値: {:?}", evs[0].data);
    assert!(evs.iter().skip(1).all(|e| e.is("chat")), "lagged の後は chat の再送");
    // バッファに残る最新 1000 件が欠落なく続く
    assert_eq!(item_ids(&chat_items(&evs)), names("m", 501..=1500));
}

// ---- 溢れ ----

#[tokio::test(start_paused = true)]
async fn events_overflow_within_one_tick_emits_lagged_with_dropped_count() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain();

    push_n(&srv, "m", 1..=300); // 時刻は進めない
    settle_ticks(2).await;

    let evs = sse.drain();
    assert!(evs[0].is("lagged"), "先頭は lagged: {evs:?}");
    assert_eq!(evs[0].json()["dropped"], 44);
    assert_eq!(evs.iter().filter(|e| e.is("lagged")).count(), 1);
    let items = chat_items(&evs);
    assert_eq!(items.len(), 256);
    assert_eq!(item_ids(&items), names("m", 45..=300), "古い方から捨てる");
}

// ---- keep-alive ----

#[tokio::test(start_paused = true)]
async fn events_keepalive_comment_is_sent_after_15s_idle() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    sse.drain(); // 接続直後に何か出す実装でも、ここで捨てる

    advance_pump(Duration::from_secs(14)).await;
    assert!(sse.drain().is_empty(), "15 秒前は何も来ない");

    advance_pump(Duration::from_millis(1100)).await;
    let evs = sse.drain();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert_eq!(evs[0].comment.as_deref(), Some("ping"));
    assert!(evs[0].event.is_none() && evs[0].data.is_none());
}

// ---- 同時接続数 ----

#[tokio::test(start_paused = true)]
async fn events_ninth_stream_is_429_too_many_streams() {
    let srv = start(1000).await;
    let mut held = Vec::new();
    for i in 0..8 {
        let s = srv.open(EVENTS, None).await;
        assert_eq!(s.status(), 200, "{} 本目", i + 1);
        held.push(s);
    }
    let ninth = srv.open(EVENTS, None).await;
    assert_eq!(ninth.status(), 429);
    pump().await;
    let body: Value = serde_json::from_str(&ninth.raw_body()).expect("本文は JSON");
    assert_eq!(body["error"]["code"], "too_many_streams");
}

#[tokio::test(start_paused = true)]
async fn events_slot_is_released_when_client_disconnects() {
    let srv = start(1000).await;
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(srv.open(EVENTS, None).await);
    }
    drop(held.pop()); // 8 本目を切る
    // 切断はサーバが次に書いた時に分かる。keep-alive を 2 周させる。
    advance_pump(Duration::from_secs(16)).await;
    advance_pump(Duration::from_secs(16)).await;

    let again = srv.open(EVENTS, None).await;
    assert_eq!(again.status(), 200, "枠が返っている");
}

// ---- 直ちに切る条件 ----

#[tokio::test(start_paused = true)]
async fn events_stream_ends_when_gate_closes() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    assert!(!sse.ended());

    srv.gate.close();
    advance_pump(Duration::from_millis(50)).await;
    assert!(sse.ended(), "ゲートを閉じたら切れる");
}

#[tokio::test(start_paused = true)]
async fn events_stream_ends_when_token_is_regenerated() {
    let srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    assert!(!sse.ended());

    srv.gate.open(&api::generate_token()); // 再生成 = 新しいトークンで開き直す
    advance_pump(Duration::from_millis(50)).await;
    assert!(sse.ended(), "トークン再生成で旧接続は切れる");
}

#[tokio::test(start_paused = true)]
async fn events_stream_ends_when_shutdown_token_is_cancelled() {
    let mut srv = start(1000).await;
    let sse = srv.open(EVENTS, None).await;
    pump().await;
    assert!(!sse.ended());

    srv.shutdown.cancel();
    pump().await;
    let handle = srv.handle.take().unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("graceful shutdown は 2 秒以内に終わる")
        .expect("serve タスクは panic しない");
    drop(sse);
}

// ---- 入力検査 ----

#[tokio::test(start_paused = true)]
async fn events_malformed_last_event_id_is_400() {
    let env = feed_env(1000);
    for bad in ["abc", ".5", "a1b2c3d4e5f6.x"] {
        let r = tokio::time::timeout(
            Duration::from_secs(5),
            send(&env.app, get(EVENTS).bearer(&env.token).header("last-event-id", bad)),
        )
        .await
        .expect("400 はストリームを開かず即返る");
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "Last-Event-ID: {bad}");
        assert_eq!(r.error_code(), "invalid_request", "Last-Event-ID: {bad}");
    }
}
