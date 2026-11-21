// registered in main.rs by story TC3-301 (earliest user: twitcasting_api.rs / twitcasting_budget.rs)
//! PR3'(ツイキャス公式 API)受け入れテストの共通小道具。公開契約(pr3-acceptance.md §1)だけに依存する。
//! - 偽サーバは `127.0.0.1:0` の axum を、接続を落とせる TCP 中継の背後に置く(`set_down` でネットワーク断を作れる)。
//! - 実時間を使う。時間のかかる待ちは `wait_until` で事象を待ち、固定 sleep は短い静止確認(<=200ms)だけにする。
//! - ログ捕捉は `capture_logs()`。テストは `#[tokio::test(flavor = "current_thread")]` で呼ぶこと。
#![allow(dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::Router;
use fast_comment_lib::integrations::MemorySecretStore;
use fast_comment_lib::model::{ChatMessage, LiveEvent};
use fast_comment_lib::sources::twitcasting::{TwitcastingOverrides, TwitcastingSource, UnofficialTuning};
use fast_comment_lib::sources::twitcasting_api::{ClientIdFn, RateBudget, TwitcastingApi};
use fast_comment_lib::sources::twitcasting_auth::{AuthOptions, TwitcastingAuth, TwitcastingEndpoints};
use fast_comment_lib::sources::Source;
use fast_comment_lib::stats::YoutubeMetadataUpdate;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, Notify};
use tokio_util::sync::CancellationToken;

pub const CLIENT: &str = "test-client-id";
pub const TOK_A: &str = "tok-A-SECRET";
pub const TOK_B: &str = "tok-B-SECRET";
pub const TOK_C: &str = "tok-C-SECRET";
pub const USER_ID: &str = "1234567";
/// 実時間の揺れに対する余裕(契約の「N 以内」を測る時だけ足す)。
pub const SLACK: Duration = Duration::from_millis(500);

pub fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

pub fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

pub fn epoch_s() -> u64 {
    epoch_ms() / 1000
}

pub async fn settle(n_ms: u64) {
    tokio::time::sleep(ms(n_ms)).await;
}

/// 条件が真になるまで 5ms 刻みで待つ。時間切れは panic(待ちの上限であって sleep ではない)。
pub async fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    loop {
        if f() {
            return;
        }
        if Instant::now() >= end {
            panic!("timeout waiting for: {what}");
        }
        tokio::time::sleep(ms(5)).await;
    }
}

// ---------------------------------------------------------------- ログ捕捉

#[derive(Clone, Default)]
pub struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl LogBuf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    /// 溜まった分を捨てる(セットアップ中のログを検査対象から外す)。
    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
    /// 溜まった分を取り出して空にする。
    pub fn take(&self) -> String {
        let mut g = self.0.lock().unwrap();
        let s = String::from_utf8_lossy(&g).into_owned();
        g.clear();
        s
    }
    pub fn lines(&self) -> Vec<String> {
        self.text().lines().map(str::to_owned).collect()
    }
}

impl std::io::Write for LogBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
    type Writer = LogBuf;
    fn make_writer(&'a self) -> LogBuf {
        self.clone()
    }
}

/// このスレッドに限って、アプリ(`fast_comment_lib`)のログだけを全レベルで捕捉する(依存クレートの DEBUG は除く)。戻り値のガードを落とすまで有効。
pub fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info,fast_comment_lib=trace"))
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}

/// `haystack` に `needles` のどれも含まれないこと。
pub fn assert_absent(place: &str, haystack: &str, needles: &[&str]) {
    for n in needles {
        assert!(!haystack.contains(n), "{place} に {n:?} が含まれている: {haystack}");
    }
}

// ---------------------------------------------------------------- 偽サーバ

#[derive(Clone, Debug)]
pub struct Req {
    pub seq: usize,
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// キーは小文字。
    pub headers: HashMap<String, String>,
    pub at: Instant,
    pub epoch_ms: u64,
}

impl Req {
    pub fn authorization(&self) -> Option<&str> {
        self.headers.get("authorization").map(String::as_str)
    }
    pub fn slice_id(&self) -> Option<String> {
        self.query.get("slice_id").cloned()
    }
    pub fn offset(&self) -> u32 {
        self.query.get("offset").and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    /// `/movies/{id}/comments` の id。
    pub fn movie_id(&self) -> String {
        self.path.split('/').nth(2).unwrap_or("").to_string()
    }
}

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub hold: Option<Arc<Notify>>,
    pub delay: Duration,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<String>) -> Reply {
        Reply { status, body: body.into(), headers: Vec::new(), hold: None, delay: Duration::ZERO }
    }
    pub fn json(status: u16, v: Value) -> Reply {
        Reply::new(status, v.to_string()).header("content-type", "application/json")
    }
    pub fn html(body: &str) -> Reply {
        Reply::new(200, body).header("content-type", "text/html; charset=utf-8")
    }
    pub fn status(mut self, s: u16) -> Reply {
        self.status = s;
        self
    }
    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
    /// `notify_one()` が呼ばれるまで応答を返さない。
    pub fn hold(mut self, n: Arc<Notify>) -> Reply {
        self.hold = Some(n);
        self
    }
    pub fn delayed(mut self, d: Duration) -> Reply {
        self.delay = d;
        self
    }
}

pub type Responder = Box<dyn FnMut(&Req) -> Reply + Send>;

struct Inner {
    reqs: Mutex<Vec<Req>>,
    responder: Mutex<Responder>,
}

fn verify_reply() -> Reply {
    Reply::json(
        200,
        json!({
            "app": { "client_id": CLIENT },
            "user": { "id": USER_ID, "screen_id": "fake_screen", "name": "Fake Name" }
        }),
    )
}

async fn handle(
    State(inner): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    let query = uri
        .query()
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect::<HashMap<_, _>>())
        .unwrap_or_default();
    let headers = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect::<HashMap<_, _>>();
    let req = {
        let mut reqs = inner.reqs.lock().unwrap();
        let r = Req {
            seq: reqs.len(),
            method: method.to_string(),
            path: uri.path().to_string(),
            query,
            headers,
            at: Instant::now(),
            epoch_ms: epoch_ms(),
        };
        reqs.push(r.clone());
        r
    };
    let reply = if req.path == "/verify_credentials" {
        verify_reply()
    } else {
        let mut responder = inner.responder.lock().unwrap();
        (*responder)(&req)
    };
    if let Some(n) = &reply.hold {
        n.notified().await;
    }
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    let mut b = axum::http::Response::builder().status(StatusCode::from_u16(reply.status).unwrap());
    for (k, v) in &reply.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    b.body(axum::body::Body::from(reply.body)).unwrap()
}

/// 受信を記録する偽サーバ。`/verify_credentials` は常に成功させる(ログイン用)。
pub struct FakeApi {
    pub base: String,
    inner: Arc<Inner>,
    down: Arc<AtomicBool>,
    conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    attempts: Arc<Mutex<Vec<Instant>>>,
}

impl FakeApi {
    pub async fn start() -> FakeApi {
        let inner = Arc::new(Inner {
            reqs: Mutex::new(Vec::new()),
            responder: Mutex::new(Box::new(|_: &Req| Reply::json(404, json!({"error": {"code": 404}})))),
        });
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend_addr = backend.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(backend, app).await;
        });
        let front = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", front.local_addr().unwrap());
        let down = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let attempts: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let (down, conns, attempts) = (down.clone(), conns.clone(), attempts.clone());
            tokio::spawn(async move {
                loop {
                    let Ok((mut client, _)) = front.accept().await else { break };
                    attempts.lock().unwrap().push(Instant::now());
                    if down.load(Ordering::SeqCst) {
                        drop(client);
                        continue;
                    }
                    let h = tokio::spawn(async move {
                        if let Ok(mut server) = tokio::net::TcpStream::connect(backend_addr).await {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                        }
                    });
                    conns.lock().unwrap().push(h.abort_handle());
                }
            });
        }
        FakeApi { base, inner, down, conns, attempts }
    }

    pub fn set_responder<F: FnMut(&Req) -> Reply + Send + 'static>(&self, f: F) {
        *self.inner.responder.lock().unwrap() = Box::new(f);
    }

    /// true: 既存の接続を切り、新規接続を受けて即閉じる(ネットワーク断)。false: 復旧。
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
        if down {
            for h in self.conns.lock().unwrap().drain(..) {
                h.abort();
            }
        }
    }

    /// `/verify_credentials` を除く受信の全記録(受信順)。
    pub fn requests(&self) -> Vec<Req> {
        self.inner.reqs.lock().unwrap().iter().filter(|r| r.path != "/verify_credentials").cloned().collect()
    }

    pub fn count(&self, pred: impl Fn(&Req) -> bool) -> usize {
        self.requests().iter().filter(|r| pred(r)).count()
    }

    pub fn total(&self) -> usize {
        self.requests().len()
    }

    /// 新規 TCP 接続の試行時刻(down 中のものを含む)。
    pub fn connection_attempts(&self) -> Vec<Instant> {
        self.attempts.lock().unwrap().clone()
    }

    /// `host:port`(URL の漏れ検査用)。
    pub fn host(&self) -> String {
        self.base.trim_start_matches("http://").to_string()
    }

    /// `/movies/{id}/comments` の受信(受信順)。
    pub fn comment_calls(&self) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.starts_with("/movies/") && r.path.ends_with("/comments")).collect()
    }

    pub fn user_calls(&self) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.starts_with("/users/") && !r.path.ends_with("/current_live")).collect()
    }

    pub fn live_calls(&self) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.path.ends_with("/current_live")).collect()
    }
}

// ---------------------------------------------------------------- 公式 API の偽応答

pub fn comment(id: u64) -> Value {
    json!({
        "id": id.to_string(),
        "message": format!("msg-{id}"),
        "created": 1_700_000_000u64 + id,
        "from_user": {
            "id": format!("u{id}"),
            "screen_id": format!("s{id}"),
            "name": format!("N{id}"),
            "image": "https://img.example/a.png"
        }
    })
}

/// 新しい順(hi から lo へ)のコメント配列を返す応答。
pub fn page_reply(hi: u64, lo: u64) -> Reply {
    let items: Vec<Value> = (lo..=hi).rev().map(comment).collect();
    Reply::json(200, json!({ "comments": items }))
}

/// 与えた順のまま返す応答。
pub fn comments_reply(ids: &[u64]) -> Reply {
    let items: Vec<Value> = ids.iter().map(|i| comment(*i)).collect();
    Reply::json(200, json!({ "comments": items }))
}

pub fn empty_reply() -> Reply {
    Reply::json(200, json!({ "comments": [] }))
}

pub fn live_reply(movie: &str, title: &str, viewers: u64) -> Reply {
    Reply::json(
        200,
        json!({
            "movie": { "id": movie, "is_live": true, "title": title, "current_view_count": viewers },
            "broadcaster": { "id": "1", "screen_id": "alice", "name": "Alice" }
        }),
    )
}

pub fn offline_reply() -> Reply {
    Reply::json(404, json!({ "error": { "code": 404 } }))
}

pub fn error_reply(status: u16, code: u32) -> Reply {
    Reply::json(status, json!({ "error": { "code": code, "message": "BODY-MARKER" } }))
}

pub fn ids_of(msgs: &[ChatMessage]) -> Vec<String> {
    msgs.iter().map(|m| m.id.clone()).collect()
}

/// `tc-<lo>` から `tc-<hi>` までの昇順。
pub fn tc_ids(lo: u64, hi: u64) -> Vec<String> {
    (lo..=hi).map(|i| format!("tc-{i}")).collect()
}

type Route = Mutex<Responder>;

/// 公式 API の 3 経路(user / current_live / comments)を、経路ごとの差し替え可能な応答にする。
pub struct Script {
    user: Route,
    live: Route,
    comments: Route,
}

impl Script {
    /// 既定: user は id=1、current_live は 404(未配信)、comments は空。
    pub fn new() -> Arc<Script> {
        Arc::new(Script {
            user: Mutex::new(Box::new(|_: &Req| {
                Reply::json(200, json!({"user": {"id": "1", "screen_id": "alice", "name": "Alice"}}))
            })),
            live: Mutex::new(Box::new(|_: &Req| offline_reply())),
            comments: Mutex::new(Box::new(|_: &Req| empty_reply())),
        })
    }
    pub fn on_user<F: FnMut(&Req) -> Reply + Send + 'static>(&self, f: F) {
        *self.user.lock().unwrap() = Box::new(f);
    }
    pub fn on_live<F: FnMut(&Req) -> Reply + Send + 'static>(&self, f: F) {
        *self.live.lock().unwrap() = Box::new(f);
    }
    pub fn on_comments<F: FnMut(&Req) -> Reply + Send + 'static>(&self, f: F) {
        *self.comments.lock().unwrap() = Box::new(f);
    }
    pub fn install(self: &Arc<Self>, fake: &FakeApi) {
        let s = self.clone();
        fake.set_responder(move |r: &Req| s.route(r));
    }
    fn route(&self, r: &Req) -> Reply {
        let seg: Vec<&str> = r.path.split('/').collect();
        let route = match seg.as_slice() {
            ["", "users", _] => &self.user,
            ["", "users", _, "current_live"] => &self.live,
            ["", "movies", _, "comments"] => &self.comments,
            _ => return Reply::json(404, json!({"error": {"code": 404}})),
        };
        let mut f = route.lock().unwrap();
        (*f)(r)
    }
}

// ---------------------------------------------------------------- 認証つきの環境

pub fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

pub fn endpoints(base: &str) -> TwitcastingEndpoints {
    TwitcastingEndpoints { authorize: "http://127.0.0.1:9/oauth2/authorize".into(), api_base: base.into() }
}

pub fn client_id_fn() -> ClientIdFn {
    Arc::new(|| Some(CLIENT.to_string()))
}

pub type Auth = Arc<TwitcastingAuth<MemorySecretStore>>;
pub type Api = Arc<TwitcastingApi<MemorySecretStore>>;

pub struct Rig {
    pub fake: FakeApi,
    pub auth: Auth,
    pub store: MemorySecretStore,
    pub port: u16,
    pub api: Api,
}

/// 未ログイン(`load` 済みで保存なし)。予算は実質無制限(速い周期のテストが予算で止まらないように)。
pub async fn rig() -> Rig {
    rig_with(RateBudget::with_capacity(100_000)).await
}

/// 既定の予算(54 回/分)。予算そのものを調べるテスト用。
pub async fn rig_default_budget() -> Rig {
    rig_with(RateBudget::new()).await
}

pub async fn rig_with(budget: RateBudget) -> Rig {
    let fake = FakeApi::start().await;
    let store = MemorySecretStore::new();
    let port = free_port();
    let options = AuthOptions { port, login_timeout: Duration::from_secs(10), read_timeout: Duration::from_secs(2) };
    let auth = TwitcastingAuth::new(store.clone(), endpoints(&fake.base), options);
    auth.load(Some(CLIENT), epoch_ms()).await;
    let api = TwitcastingApi::with_budget(auth.clone(), endpoints(&fake.base), client_id_fn(), budget);
    Rig { fake, auth, store, port, api }
}

pub async fn rig_logged_in() -> Rig {
    let r = rig().await;
    login(&r.auth, r.port, TOK_A).await;
    r
}

pub async fn rig_logged_in_default_budget() -> Rig {
    let r = rig_default_budget().await;
    login(&r.auth, r.port, TOK_A).await;
    r
}

/// 実フロー(認可 URL → ブラウザ役の POST → 検証)でログインする。ログイン済みでも再ログインとして使える。
pub async fn login(auth: &Auth, port: u16, token: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let a = auth.clone();
    let h = tokio::spawn(async move {
        a.connect(CLIENT, epoch_ms(), move |url| {
            let _ = tx.send(url);
            Ok(())
        })
        .await
    });
    let url = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("open_browser が呼ばれる")
        .expect("open_browser が呼ばれる前に connect が終わった");
    let state = url::Url::parse(&url)
        .expect("認可 URL は URL")
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("認可 URL に state がある");
    let body = format!("access_token={token}&token_type=bearer&expires_in=15552000&state={state}");
    let request = format!(
        "POST /twitcasting/token HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("待受に接続できる");
    let _ = s.write_all(request.as_bytes()).await;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    let r = tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .expect("connect が終わる")
        .expect("タスクが落ちない");
    assert!(r.is_ok(), "ログインが成功する");
}

// ---------------------------------------------------------------- Source の実行と収集

pub fn unofficial_tuning() -> UnofficialTuning {
    UnofficialTuning { offline: ms(30), unreadable_recheck: ms(30), not_found_retry: ms(200), jitter: false }
}

/// 非公式の偽サイト 1 つ(視聴ページ・eventpubsuburl・listall)を指す overrides。
pub fn overrides_for(base: &str) -> TwitcastingOverrides {
    TwitcastingOverrides {
        endpoints: HashMap::from([
            ("userPageBaseUrl".to_string(), base.to_string()),
            ("eventPubsubUrl".to_string(), format!("{base}/eventpubsuburl.php")),
            ("commentHistoryUrl".to_string(), base.to_string()),
        ]),
        ..Default::default()
    }
}

pub fn unofficial_source(id: &str, site_base: &str, chan: &Chan) -> TwitcastingSource {
    TwitcastingSource::new(id.to_string(), overrides_for(site_base), Some(chan.meta_tx()))
        .with_live_tx(Some(chan.live_tx()))
        .with_tuning(unofficial_tuning())
}

/// `ChatMessage` / `YoutubeMetadataUpdate` / `LiveEvent` の受け皿。作った時点から受信する。
pub struct Chan {
    tx: broadcast::Sender<ChatMessage>,
    meta_tx: mpsc::Sender<YoutubeMetadataUpdate>,
    live_tx: mpsc::Sender<LiveEvent>,
    msgs: Arc<Mutex<Vec<ChatMessage>>>,
    metas: Arc<Mutex<Vec<YoutubeMetadataUpdate>>>,
    lives: Arc<Mutex<Vec<LiveEvent>>>,
}

impl Chan {
    pub fn new() -> Chan {
        let (tx, mut rx) = broadcast::channel::<ChatMessage>(16384);
        let (meta_tx, mut meta_rx) = mpsc::channel::<YoutubeMetadataUpdate>(16384);
        let (live_tx, mut live_rx) = mpsc::channel::<LiveEvent>(16384);
        let msgs = Arc::new(Mutex::new(Vec::new()));
        let metas = Arc::new(Mutex::new(Vec::new()));
        let lives = Arc::new(Mutex::new(Vec::new()));
        {
            let msgs = msgs.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(m) => msgs.lock().unwrap().push(m),
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        {
            let metas = metas.clone();
            tokio::spawn(async move {
                while let Some(m) = meta_rx.recv().await {
                    metas.lock().unwrap().push(m);
                }
            });
        }
        {
            let lives = lives.clone();
            tokio::spawn(async move {
                while let Some(e) = live_rx.recv().await {
                    lives.lock().unwrap().push(e);
                }
            });
        }
        Chan { tx, meta_tx, live_tx, msgs, metas, lives }
    }

    pub fn meta_tx(&self) -> mpsc::Sender<YoutubeMetadataUpdate> {
        self.meta_tx.clone()
    }
    pub fn live_tx(&self) -> mpsc::Sender<LiveEvent> {
        self.live_tx.clone()
    }
    pub fn msgs(&self) -> Vec<ChatMessage> {
        self.msgs.lock().unwrap().clone()
    }
    pub fn msgs_handle(&self) -> Arc<Mutex<Vec<ChatMessage>>> {
        self.msgs.clone()
    }
    pub fn msg_ids(&self) -> Vec<String> {
        ids_of(&self.msgs())
    }
    pub fn metas(&self) -> Vec<YoutubeMetadataUpdate> {
        self.metas.lock().unwrap().clone()
    }
    /// `notice` を持つ更新の notice を受信順に。
    pub fn notices(&self) -> Vec<String> {
        self.metas().into_iter().filter_map(|m| m.notice).collect()
    }
    pub fn has_notice(&self, n: &str) -> bool {
        self.notices().iter().any(|x| x == n)
    }
    pub fn lives(&self) -> Vec<LiveEvent> {
        self.lives.lock().unwrap().clone()
    }
    pub fn started_count(&self) -> usize {
        self.lives().iter().filter(|e| matches!(e, LiveEvent::Started(_))).count()
    }

    /// Source を別タスクで走らせる。
    pub fn run(&self, src: impl Source + 'static) -> Running {
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move { src.run(tx, c).await });
        Running { cancel, handle }
    }
}

pub struct Running {
    pub cancel: CancellationToken,
    pub handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    /// cancel して 1 秒以内に `Ok(())` で終わることを確かめ、かかった時間を返す。
    pub async fn stop(self) -> Duration {
        let t = Instant::now();
        self.cancel.cancel();
        let r = tokio::time::timeout(Duration::from_secs(1), self.handle)
            .await
            .expect("cancel から 1 秒以内に止まる")
            .expect("タスクが panic しない");
        assert!(r.is_ok(), "cancel で Ok(()) を返す");
        t.elapsed()
    }
}
