use super::{
    delivery::{classify, Decision, Outcome},
    payload,
    rules::{self, Dedup},
    sign, Matched, OutboundRequest, Sender, Trigger,
};
use crate::{
    integrations::{Integrations, SecretStore, WebhookDef, WebhookFormat, WebhookSecret},
    model::ChatMessage,
};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::Notify,
    time::{sleep_until, Instant},
};
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LastResult {
    pub at_ms: u64,
    pub http_status: Option<u16>,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookStatus {
    pub id: String,
    pub state: String,
    pub paused_reason: Option<String>,
    pub delivered: u64,
    pub failed: u64,
    pub dropped: u64,
    pub uncertain: u64,
    pub queued: usize,
    pub last: Option<LastResult>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TestSendResult {
    pub ok: bool,
    pub status: Option<u16>,
    pub error_kind: Option<String>,
}
#[derive(Clone)]
struct Clock {
    origin: Instant,
    epoch_ms: u64,
}
impl Clock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }
    }
    fn now_ms(&self) -> u64 {
        self.epoch_ms
            .saturating_add(self.origin.elapsed().as_millis() as u64)
    }
}
struct Item {
    at: Instant,
    request: OutboundRequest,
}
struct WorkState {
    queue: VecDeque<Item>,
    status: WebhookStatus,
}
struct Worker {
    def: WebhookDef,
    secret: Option<WebhookSecret>,
    rules: rules::CompiledRules,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    state: Mutex<WorkState>,
    wake: Notify,
    cancel: CancellationToken,
}
impl Worker {
    fn active(&self) -> bool {
        !self.cancel.is_cancelled() && self.state.lock().unwrap().status.state == "active"
    }
    fn stop(&self) {
        self.cancel.cancel();
        let mut s = self.state.lock().unwrap();
        s.status.dropped += s.queue.len() as u64;
        s.queue.clear();
        s.status.queued = 0;
    }
}
struct RuntimeInner<S: SecretStore, T: Sender> {
    integrations: Integrations<S>,
    sender: T,
    workers: Mutex<HashMap<String, Arc<Worker>>>,
    dedup: Mutex<Dedup>,
    reload_lock: tokio::sync::Mutex<()>,
    clock: Clock,
    changed: Arc<Notify>,
}
impl<S: SecretStore, T: Sender> Drop for RuntimeInner<S, T> {
    fn drop(&mut self) {
        for w in self.workers.get_mut().unwrap().values() {
            w.stop();
        }
    }
}
#[derive(Clone)]
pub struct WebhookRuntime<S: SecretStore, T: Sender>(Arc<RuntimeInner<S, T>>);
impl<S: SecretStore, T: Sender> WebhookRuntime<S, T> {
    pub fn new(integrations: Integrations<S>, sender: T) -> Self {
        Self(Arc::new(RuntimeInner {
            integrations,
            sender,
            workers: Mutex::new(HashMap::new()),
            dedup: Mutex::new(Dedup::new(8192)),
            reload_lock: tokio::sync::Mutex::new(()),
            clock: Clock::new(),
            changed: Arc::new(Notify::new()),
        }))
    }
    pub async fn reload(&self) {
        let _guard = self.0.reload_lock.lock().await;
        let snapshot: Vec<_> = self
            .0
            .integrations
            .webhook_snapshot()
            .into_iter()
            .map(|(def, secret)| {
                let rules = rules::CompiledRules::new(&def);
                (def, secret, rules)
            })
            .collect();
        let mut workers = self.0.workers.lock().unwrap();
        workers.retain(|id, w| {
            let keep = snapshot.iter().any(|(d, _, _)| &d.id == id);
            if !keep {
                w.stop();
            }
            keep
        });
        for (def, secret, rules) in snapshot {
            let desired_state = if secret.is_none() {
                "noUrl"
            } else if def.paused.is_some() {
                "paused"
            } else if !def.enabled {
                "disabled"
            } else {
                "active"
            };
            // 自動停止後は定義が同じでも status とタスクが変わっている。
            // 保存に失敗した停止・終了済みタスクも、再開時には作り直す。
            if workers.get(&def.id).is_some_and(|w| {
                w.def == def
                    && w.secret == secret
                    && w.state.lock().unwrap().status.state == desired_state
                    && (desired_state != "active"
                        || (!w.cancel.is_cancelled()
                            && w.task
                                .lock()
                                .unwrap()
                                .as_ref()
                                .is_some_and(|t| !t.is_finished())))
            }) {
                continue;
            }
            let previous = workers.remove(&def.id).map(|w| {
                w.stop();
                let status = w.state.lock().unwrap().status.clone();
                status
            });
            let mut status = previous.unwrap_or(WebhookStatus {
                id: def.id.clone(),
                state: String::new(),
                paused_reason: None,
                delivered: 0,
                failed: 0,
                dropped: 0,
                uncertain: 0,
                queued: 0,
                last: None,
            });
            status.state = desired_state.into();
            status.paused_reason = def.paused.as_ref().map(|p| p.reason.clone());
            let w = Arc::new(Worker {
                def: def.clone(),
                secret,
                rules,
                task: Mutex::new(None),
                state: Mutex::new(WorkState {
                    queue: VecDeque::new(),
                    status,
                }),
                wake: Notify::new(),
                cancel: CancellationToken::new(),
            });
            if w.active() {
                let (worker, integrations, sender, clock, changed) = (
                    w.clone(),
                    self.0.integrations.clone(),
                    self.0.sender.clone(),
                    self.0.clock.clone(),
                    self.0.changed.clone(),
                );
                let task = tokio::spawn(async move {
                    tokio::select! { biased; _ = worker.cancel.cancelled() => {}, _ = run_worker(worker.clone(), integrations, sender, clock, changed) => {} }
                });
                *w.task.lock().unwrap() = Some(task.abort_handle());
            }
            workers.insert(def.id, w);
        }
        self.0.changed.notify_one();
    }
    pub async fn changed(&self) {
        self.0.changed.notified().await;
    }
    pub fn has_active_webhooks(&self) -> bool {
        self.0.workers.lock().unwrap().values().any(|w| w.active())
    }
    pub fn handle_message(&self, msg: &ChatMessage) {
        if msg.injected_test || msg.skip_tts || msg.kind == crate::model::MessageKind::System {
            return;
        }
        if !self
            .0
            .dedup
            .lock()
            .unwrap()
            .first_seen(msg.platform, &msg.id)
        {
            return;
        }
        let workers: Vec<_> = self.0.workers.lock().unwrap().values().cloned().collect();
        for w in workers {
            if !w.active() {
                continue;
            }
            let Some(matched) = w.rules.decide(&w.def, msg) else {
                continue;
            };
            let Some(secret) = &w.secret else {
                continue;
            };
            let item = Item {
                at: Instant::now(),
                request: request(&w.def, secret, &matched, msg, self.0.clock.now_ms()),
            };
            let mut s = w.state.lock().unwrap();
            if s.status.state != "active" || w.cancel.is_cancelled() {
                continue;
            }
            if s.queue.len() == 100 {
                s.queue.pop_front();
                s.status.dropped += 1;
            }
            s.queue.push_back(item);
            s.status.queued = s.queue.len();
            drop(s);
            w.wake.notify_one();
        }
    }
    pub fn status(&self) -> Vec<WebhookStatus> {
        let workers = self.0.workers.lock().unwrap();
        // 一覧と同じ設定順で返す。
        self.0
            .integrations
            .list_webhooks()
            .iter()
            .filter_map(|v| {
                workers
                    .get(&v.definition.id)
                    .map(|w| w.state.lock().unwrap().status.clone())
            })
            .collect()
    }
    pub fn shutdown(&self) {
        for w in self.0.workers.lock().unwrap().values() {
            w.stop();
        }
    }
    pub async fn test_send(&self, id: &str) -> TestSendResult {
        let Some((def, Some(secret))) = self
            .0
            .integrations
            .webhook_snapshot()
            .into_iter()
            .find(|(d, _)| d.id == id)
        else {
            return TestSendResult {
                ok: false,
                status: None,
                error_kind: Some("noUrl".into()),
            };
        };
        // 規則・キューと独立した、秘密を含まない固定のテストコメント。
        let msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "id":"webhook-test", "platform":"youtube", "channel":"", "kind":"normal", "timestampMs":self.0.clock.now_ms(),
            "author":{"id":"fast-comment", "name":"fast-comment"}, "fragments":[{"type":"text","text":"Webhook テスト送信"}]
        })).expect("fixed test message");
        let out = self
            .0
            .sender
            .send(request(
                &def,
                &secret,
                &Matched {
                    trigger: Trigger::Test,
                    keyword: None,
                },
                &msg,
                self.0.clock.now_ms(),
            ))
            .await;
        let ok = classify(def.format, &out) == Decision::Delivered;
        TestSendResult {
            ok,
            status: http_status(&out),
            error_kind: if ok { None } else { Some(kind(&out).into()) },
        }
    }
}
fn request(
    def: &WebhookDef,
    secret: &WebhookSecret,
    matched: &Matched,
    msg: &ChatMessage,
    now: u64,
) -> OutboundRequest {
    let event_id = uuid::Uuid::new_v4().to_string();
    let v = if def.format == WebhookFormat::Discord {
        payload::discord(matched, msg)
    } else {
        payload::generic(&event_id, matched, msg, now)
    };
    let body = serde_json::to_vec(&v).expect("JSON payload");
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "User-Agent".into(),
            format!("fast-comment/{}", env!("CARGO_PKG_VERSION")),
        ),
    ];
    if def.format == WebhookFormat::Generic {
        let ts = now / 1000;
        headers.extend([
            ("X-FastComment-Event-Id".into(), event_id),
            ("X-FastComment-Timestamp".into(), ts.to_string()),
            (
                "X-FastComment-Signature".into(),
                sign::signature_header(
                    secret
                        .signing_secret
                        .as_deref()
                        .unwrap_or_default()
                        .as_bytes(),
                    ts,
                    &body,
                ),
            ),
        ]);
    }
    OutboundRequest::new(&secret.url, headers, body)
}
fn http_status(out: &Outcome) -> Option<u16> {
    if let Outcome::Status { code, .. } = out {
        Some(*code)
    } else {
        None
    }
}
fn kind(out: &Outcome) -> &'static str {
    match out {
        Outcome::ConnectFailed => "connectFailed",
        Outcome::TimedOutAfterSend => "timedOutAfterSend",
        Outcome::ReadFailed => "readFailed",
        Outcome::Status {
            code: 200..=299, ..
        } => "delivered",
        Outcome::Status { .. } => "httpError",
    }
}
fn jitter(base: u64) -> Duration {
    let mut bytes = [0u8; 2];
    let factor = if getrandom::getrandom(&mut bytes).is_ok() {
        0.8 + u16::from_le_bytes(bytes) as f64 / u16::MAX as f64 * 0.4
    } else {
        1.0
    };
    Duration::from_secs_f64(base as f64 * factor)
}
async fn run_worker<S: SecretStore, T: Sender>(
    w: Arc<Worker>,
    integrations: Integrations<S>,
    sender: T,
    clock: Clock,
    changed: Arc<Notify>,
) {
    let mut tokens = 5.0f64;
    let mut refill_at = Instant::now();
    let mut ready_at = Instant::now();
    let mut consecutive = 0;
    loop {
        // notify_one の permit は pop と待機の間でも失われない。
        let item = {
            let mut s = w.state.lock().unwrap();
            let item = s.queue.pop_front();
            s.status.queued = s.queue.len();
            item
        };
        let Some(item) = item else {
            w.wake.notified().await;
            continue;
        };
        let expires = item.at + Duration::from_secs(600);
        let mut attempt = 0usize;
        loop {
            if ready_at > Instant::now() {
                sleep_until(ready_at.min(expires)).await;
            }
            let now = Instant::now();
            tokens = (tokens + (now - refill_at).as_secs_f64() / 2.0).min(5.0);
            refill_at = now;
            if tokens < 1.0 {
                sleep_until((now + Duration::from_secs_f64((1.0 - tokens) * 2.0)).min(expires))
                    .await;
            }
            let now = Instant::now();
            if now >= expires {
                w.state.lock().unwrap().status.dropped += 1;
                break;
            }
            tokens = (tokens + (now - refill_at).as_secs_f64() / 2.0).min(5.0);
            refill_at = now;
            tokens -= 1.0;
            let out = sender.send(item.request.clone()).await;
            attempt += 1;
            {
                let mut s = w.state.lock().unwrap();
                s.status.last = Some(LastResult {
                    at_ms: clock.now_ms(),
                    http_status: http_status(&out),
                    kind: kind(&out).into(),
                });
            }
            if w.def.format == WebhookFormat::Discord {
                if let Outcome::Status {
                    remaining_zero_reset_after: Some(d),
                    ..
                } = &out
                {
                    let now = Instant::now();
                    // 送り先全体の待ちは、現在項目の寿命で短縮しない。
                    ready_at = ready_at.max(now + (*d).min(Duration::from_secs(600)));
                }
            }
            let mut pause = None;
            match classify(w.def.format.clone(), &out) {
                Decision::Delivered => {
                    w.state.lock().unwrap().status.delivered += 1;
                    consecutive = 0;
                    break;
                }
                Decision::Retry { wait } => {
                    // 429 の待機は最後の試行でも後続の配送へ引き継ぐ。
                    let delay = wait.unwrap_or_else(|| jitter([2, 8, 30][(attempt - 1).min(2)]));
                    let now = Instant::now();
                    let retry_at = now + delay.min(Duration::from_secs(600));
                    if matches!(out, Outcome::Status { code: 429, .. }) {
                        ready_at = ready_at.max(retry_at);
                    }
                    if attempt < 4 {
                        if retry_at >= expires {
                            w.state.lock().unwrap().status.dropped += 1;
                            break;
                        }
                        ready_at = ready_at.max(retry_at);
                        continue;
                    }
                    w.state.lock().unwrap().status.failed += 1;
                    consecutive += 1;
                    if consecutive >= 10 {
                        pause = Some("unreachable");
                    }
                }
                Decision::Drop { uncertain } => {
                    let mut s = w.state.lock().unwrap();
                    if uncertain {
                        s.status.uncertain += 1;
                        if let Some(last) = &mut s.status.last {
                            last.kind = "uncertain".into();
                        }
                    } else {
                        s.status.failed += 1;
                    }
                    // 成功した配送だけが連続失敗数を戻す。
                }
                Decision::DropAndPause { reason } => {
                    w.state.lock().unwrap().status.failed += 1;
                    pause = Some(reason);
                }
            }
            if let Some(reason) = pause {
                {
                    let mut s = w.state.lock().unwrap();
                    s.status.state = "paused".into();
                    s.status.paused_reason = Some(reason.into());
                    s.status.dropped += s.queue.len() as u64;
                    s.queue.clear();
                    s.status.queued = 0;
                }
                changed.notify_one();
                if integrations
                    .pause_webhook(
                        &w.def,
                        w.secret.as_ref().expect("active worker has credentials"),
                        reason,
                        clock.now_ms(),
                    )
                    .await
                    .is_err()
                {
                    tracing::warn!("Webhook 停止状態の保存に失敗しました");
                }
                return;
            }
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::{MemorySecretStore, WebhookInput, WebhookTriggers};
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<OutboundRequest>>>);
    impl Sender for Capture {
        async fn send(&self, request: OutboundRequest) -> Outcome {
            self.0.lock().unwrap().push(request);
            tokio::time::sleep(Duration::from_secs(10)).await;
            Outcome::Status {
                code: 204,
                header_retry_after: None,
                body_retry_after: None,
                remaining_zero_reset_after: None,
            }
        }
    }
    fn message(id: &str) -> ChatMessage {
        serde_json::from_value(serde_json::json!({"id":id,"platform":"youtube","channel":"test","author":{"id":"a","name":"A"},"kind":"superChat","timestampMs":1,"fragments":[{"type":"text","text":"ありがとう"}]})).unwrap()
    }
    fn input(url: &str) -> WebhookInput {
        WebhookInput {
            id: None,
            name: "通知".into(),
            format: WebhookFormat::Generic,
            enabled: true,
            triggers: WebhookTriggers {
                donation: true,
                ..Default::default()
            },
            keywords: vec![],
            url: Some(url.into()),
        }
    }
    #[tokio::test(start_paused = true)]
    async fn injected_history_and_duplicates_never_reach_sender_on_any_target() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let sender = Capture::default();
        let rt = WebhookRuntime::new(it.clone(), sender.clone());
        for url in ["https://a.example.test/hook", "https://b.example.test/hook"] {
            it.save_webhook(input(url)).await.unwrap();
        }
        rt.reload().await;
        let mut m = message("same");
        m.injected_test = true;
        rt.handle_message(&m);
        m.injected_test = false;
        m.skip_tts = true;
        rt.handle_message(&m);
        m.skip_tts = false;
        rt.handle_message(&m);
        rt.handle_message(&m);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(sender.0.lock().unwrap().len(), 2);
        assert!(rt.status().iter().all(|s| s.delivered == 1));
    }
    #[tokio::test(start_paused = true)]
    async fn reload_preserves_unmodified_workers_and_shutdown_discards_queue() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let sender = Capture::default();
        let rt = WebhookRuntime::new(it.clone(), sender.clone());
        it.save_webhook(input("https://a.example.test/hook"))
            .await
            .unwrap();
        rt.reload().await;
        rt.handle_message(&message("one"));
        rt.handle_message(&message("two"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        rt.reload().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(sender.0.lock().unwrap().len(), 2);
        assert_eq!(rt.status()[0].delivered, 2);
        rt.handle_message(&message("three"));
        rt.handle_message(&message("four"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        rt.shutdown();
        assert!(!rt.has_active_webhooks());
        rt.handle_message(&message("five"));
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(sender.0.lock().unwrap().len(), 3);
        assert_eq!(rt.status()[0].queued, 0);
        assert_eq!(rt.status()[0].dropped, 1);
    }
    #[derive(Clone, Default)]
    struct Script {
        outcomes: Arc<Mutex<VecDeque<Outcome>>>,
        calls: Arc<Mutex<usize>>,
    }
    impl Sender for Script {
        async fn send(&self, _: OutboundRequest) -> Outcome {
            *self.calls.lock().unwrap() += 1;
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| status(204))
        }
    }
    fn status(code: u16) -> Outcome {
        Outcome::Status {
            code,
            header_retry_after: None,
            body_retry_after: None,
            remaining_zero_reset_after: None,
        }
    }
    async fn settle() {
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
    }
    #[tokio::test(start_paused = true)]
    async fn regression_resume_restarts_auto_stopped_workers() {
        for code in [401, 404, 302] {
            for save_fails in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
                let id = it
                    .save_webhook(input("https://a.example.test/hook"))
                    .await
                    .unwrap()
                    .definition
                    .id;
                let sender = Script::default();
                sender.outcomes.lock().unwrap().push_back(status(code));
                let rt = WebhookRuntime::new(it.clone(), sender.clone());
                rt.reload().await;
                let path = dir.path().join(crate::integrations::FILE_NAME);
                if save_fails {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::create_dir(&path).unwrap();
                }
                rt.handle_message(&message("a"));
                settle().await;
                assert_eq!(rt.status()[0].state, "paused");
                if save_fails {
                    std::fs::remove_dir(&path).unwrap();
                }
                it.resume_webhook(&id).await.unwrap();
                rt.reload().await;
                rt.handle_message(&message("b"));
                settle().await;
                assert_eq!(
                    *sender.calls.lock().unwrap(),
                    2,
                    "code={code}, save_fails={save_fails}"
                );
                assert_eq!(rt.status()[0].state, "active");
            }
        }
    }
    #[tokio::test(start_paused = true)]
    async fn regression_huge_waits_expire_without_blocking_new_deliveries() {
        for source in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
            let mut def = input("https://discord.com/api/webhooks/id/token");
            def.format = WebhookFormat::Discord;
            it.save_webhook(def).await.unwrap();
            let sender = Script::default();
            let huge = Some(Duration::from_secs(1_000_000_000));
            sender.outcomes.lock().unwrap().push_back(Outcome::Status {
                code: if source == 2 { 204 } else { 429 },
                header_retry_after: if source == 0 { huge } else { None },
                body_retry_after: if source == 1 { huge } else { None },
                remaining_zero_reset_after: if source == 2 { huge } else { None },
            });
            let rt = WebhookRuntime::new(it, sender.clone());
            rt.reload().await;
            rt.handle_message(&message("old"));
            settle().await;
            tokio::time::advance(Duration::from_secs(601)).await;
            settle().await;
            rt.handle_message(&message("new"));
            settle().await;
            assert_eq!(*sender.calls.lock().unwrap(), 2, "source={source}");
            assert_eq!(rt.status()[0].state, "active");
        }
    }
    #[tokio::test(start_paused = true)]
    async fn regression_only_success_resets_consecutive_failures() {
        for interruption in [status(400), Outcome::ReadFailed, status(204)] {
            let dir = tempfile::tempdir().unwrap();
            let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
            let mut def = input("https://discord.com/api/webhooks/id/token");
            def.format = WebhookFormat::Discord;
            it.save_webhook(def).await.unwrap();
            let sender = Script::default();
            let success = interruption == status(204);
            let mut outcomes = vec![status(503); 9 * 4];
            outcomes.push(interruption);
            outcomes.extend(vec![status(503); 4]);
            *sender.outcomes.lock().unwrap() = outcomes.into();
            let rt = WebhookRuntime::new(it, sender);
            rt.reload().await;
            for i in 0..11 {
                rt.handle_message(&message(&i.to_string()));
                settle().await;
                for _ in 0..4 {
                    tokio::time::advance(Duration::from_secs(40)).await;
                    settle().await;
                }
            }
            assert_eq!(
                rt.status()[0].state,
                if success { "active" } else { "paused" }
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn regression_keywords_compile_only_on_reload() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let mut def = input("https://a.example.test/hook");
        def.triggers.donation = false;
        def.triggers.keyword = true;
        def.keywords = vec!["ありがとう".into(), "a.b".into()];
        it.save_webhook(def).await.unwrap();
        let sender = Script::default();
        let rt = WebhookRuntime::new(it, sender.clone());
        rt.reload().await;
        let before = rules::COMPILES.with(|n| n.get());
        for i in 0..3 {
            rt.handle_message(&message(&i.to_string()));
        }
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 3);
        assert_eq!(rules::COMPILES.with(|n| n.get()), before);
    }
    #[tokio::test(start_paused = true)]
    async fn unrelated_save_must_not_resume_after_pause_persistence_failure() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        it.save_webhook(input("https://example.test/hook"))
            .await
            .unwrap();
        let sender = Script::default();
        sender.outcomes.lock().unwrap().push_back(status(404));
        let rt = WebhookRuntime::new(it.clone(), sender.clone());
        rt.reload().await;
        let path = dir.path().join(crate::integrations::FILE_NAME);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        rt.handle_message(&message("a"));
        settle().await;
        assert_eq!(rt.status()[0].state, "paused");
        std::fs::remove_dir(&path).unwrap();
        it.save_webhook(input("https://example.test/hook"))
            .await
            .unwrap(); // 元の送り先は再開せず、別の送り先だけを保存する。
        rt.reload().await;
        rt.handle_message(&message("b"));
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 2);
        assert_eq!(
            rt.status()[0].state,
            "paused",
            "unrelated save must not resume an auto-paused worker"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn expiring_item_must_not_shorten_retry_after_for_new_item() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        it.save_webhook(input("https://example.test/hook"))
            .await
            .unwrap();
        let sender = Script::default();
        for seconds in [590, 30] {
            sender.outcomes.lock().unwrap().push_back(Outcome::Status {
                code: 429,
                header_retry_after: Some(Duration::from_secs(seconds)),
                body_retry_after: None,
                remaining_zero_reset_after: None,
            });
        }
        let rt = WebhookRuntime::new(it, sender.clone());
        rt.reload().await;
        rt.handle_message(&message("old"));
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 1);
        tokio::time::advance(Duration::from_secs(590)).await;
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 2);
        rt.handle_message(&message("new"));
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(
            *sender.calls.lock().unwrap(),
            2,
            "Retry-After 30 received at t=590 must block new deliveries until t=620 (now t=600)"
        );
        tokio::time::advance(Duration::from_secs(20)).await;
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 3);
        assert_eq!(rt.status()[0].delivered, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expiring_item_must_not_shorten_discord_reset_after_for_new_item() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let mut def = input("https://discord.com/api/webhooks/id/token");
        def.format = WebhookFormat::Discord;
        it.save_webhook(def).await.unwrap();
        let sender = Script::default();
        for seconds in [590, 30] {
            sender.outcomes.lock().unwrap().push_back(Outcome::Status {
                code: 204,
                header_retry_after: None,
                body_retry_after: None,
                remaining_zero_reset_after: Some(Duration::from_secs(seconds)),
            });
        }
        let rt = WebhookRuntime::new(it, sender.clone());
        rt.reload().await;
        rt.handle_message(&message("first"));
        rt.handle_message(&message("old"));
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 1);
        tokio::time::advance(Duration::from_secs(590)).await;
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 2);
        rt.handle_message(&message("new"));
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 2);
        tokio::time::advance(Duration::from_secs(20)).await;
        settle().await;
        assert_eq!(*sender.calls.lock().unwrap(), 3);
        assert_eq!(rt.status()[0].delivered, 3);
    }
}
