//! 読み上げワーカー。単一キュー、epoch ゲートと WAV 長による一件先読み。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::queue::{PushResult, TtsJob, TtsQueueCore};
use super::{
    SpeakContext, SpeechOutput, TtsDispatcher, TtsQueueItem, TtsQueueStatePayload, TtsSink,
};
use crate::{config::TtsConfig, model::ChatMessage};

#[derive(Default)]
pub struct TtsShared {
    pub core: Mutex<TtsQueueCore>,
    pub wake: Notify,
    last_drop_notice: Mutex<Option<Instant>>,
}

impl TtsShared {
    fn dropped(&self, sink: &dyn TtsSink, count: usize) {
        if count == 0 {
            return;
        }
        let now = Instant::now();
        let mut last = self.last_drop_notice.lock().unwrap();
        if last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(30)) {
            return;
        }
        *last = Some(now);
        sink.notice(
            "info",
            format!("読み上げの待機上限を超えたコメントを {count} 件スキップしました"),
        );
    }

    pub fn enqueue(&self, sink: &dyn TtsSink, msg: ChatMessage, max_pending: usize) -> PushResult {
        let job = TtsJob {
            item: TtsQueueItem::from_message(&msg),
            msg,
            enqueued_at: Instant::now(),
        };
        let result = {
            let mut core = self.core.lock().unwrap();
            let result = core.push(job, max_pending);
            sink.queue_state(core.snapshot());
            result
        };
        if let PushResult::Queued { dropped_oldest } = result {
            self.dropped(sink, dropped_oldest);
            self.wake.notify_one();
        }
        result
    }

    pub fn clear(&self, sink: &dyn TtsSink) {
        {
            let mut core = self.core.lock().unwrap();
            core.clear();
            sink.cancel();
            sink.queue_state(core.snapshot());
        }
        self.wake.notify_one();
    }

    pub fn set_paused(&self, sink: &dyn TtsSink, paused: bool) {
        {
            let mut core = self.core.lock().unwrap();
            core.set_paused(paused);
            if paused {
                sink.cancel();
            }
            sink.queue_state(core.snapshot());
        }
        self.wake.notify_one();
    }

    pub fn skip_current(&self, sink: &dyn TtsSink) {
        {
            let mut core = self.core.lock().unwrap();
            core.skip_current(Instant::now());
            sink.cancel();
            sink.queue_state(core.snapshot());
        }
        self.wake.notify_one();
    }

    pub fn snapshot(&self) -> TtsQueueStatePayload {
        self.core.lock().unwrap().snapshot()
    }
}

pub async fn run_tts_worker(
    shared: Arc<TtsShared>,
    sink: Arc<dyn TtsSink>,
    mut dispatcher: TtsDispatcher,
    config_fn: impl Fn() -> TtsConfig,
    cancel: CancellationToken,
) {
    let mut warned_duration = false;
    'jobs: loop {
        if cancel.is_cancelled() {
            break;
        }
        let cfg = config_fn();
        let max_wait = (cfg.options.queue.max_wait_sec != 0)
            .then(|| Duration::from_secs(cfg.options.queue.max_wait_sec as u64));
        let (popped, expired) = {
            let mut core = shared.core.lock().unwrap();
            let result = core.pop_fresh(Instant::now(), max_wait);
            sink.queue_state(core.snapshot());
            result
        };
        shared.dropped(sink.as_ref(), expired);
        let Some(popped) = popped else {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = shared.wake.notified() => continue,
            }
        };
        dispatcher.update_config(cfg);
        let output = {
            let speech = dispatcher.speak(
                &popped.job.msg,
                SpeakContext {
                    pending: popped.pending,
                },
            );
            tokio::pin!(speech);
            loop {
                if !shared.core.lock().unwrap().is_current(popped.epoch) {
                    continue 'jobs;
                }
                tokio::select! {
                    _ = cancel.cancelled() => break 'jobs,
                    _ = shared.wake.notified() => {
                        // Enqueue と Notify の残 permit はキャンセルではない。
                        if !shared.core.lock().unwrap().is_current(popped.epoch) { continue 'jobs; }
                    }
                    output = &mut speech => break output,
                }
            }
        };
        let notices = dispatcher.take_notices();
        if matches!(output, SpeechOutput::Audio { .. }) {
            loop {
                let until = {
                    let core = shared.core.lock().unwrap();
                    if !core.is_current(popped.epoch) {
                        continue 'jobs;
                    }
                    core.play_until()
                };
                let Some(until) = until.filter(|t| *t > Instant::now()) else {
                    break;
                };
                tokio::select! {
                    _ = cancel.cancelled() => break 'jobs,
                    _ = shared.wake.notified() => {},
                    _ = tokio::time::sleep_until(until.into()) => {},
                }
            }
        }
        if cancel.is_cancelled() {
            break;
        }
        if matches!(output, SpeechOutput::Audio { duration: None, .. }) && !warned_duration {
            tracing::warn!("WAV 長を取得できないため読み上げの再生待ちを省略します");
            warned_duration = true;
        }
        // キャンセル側と同じロックで検証と送出を不可分にする。ここに await は置かない。
        let mut core = shared.core.lock().unwrap();
        if !core.is_current(popped.epoch) {
            continue;
        }
        for notice in notices {
            sink.notice(notice.level, notice.message);
        }
        match output {
            SpeechOutput::Audio {
                wav_base64,
                duration,
            } => {
                let wait = duration
                    .map(|d| d.saturating_add(Duration::from_millis(50)))
                    .unwrap_or_default();
                if core.try_begin_playback(popped.epoch, Instant::now(), wait) {
                    sink.audio(wav_base64);
                }
            }
            SpeechOutput::WebSpeech(p) => sink.web_speech(p),
            SpeechOutput::Nothing | SpeechOutput::Sent => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TtsBackendKind;
    use crate::tts::{voicevox::test_support::spawn_fake_engine, WebSpeechPayload};
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use serde_json::json;
    use tokio::sync::Semaphore;

    #[derive(Default)]
    struct Sink {
        audio: Mutex<Vec<Instant>>,
        audio_payloads: Mutex<Vec<String>>,
        speech: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
        cancels: Mutex<usize>,
    }
    impl TtsSink for Sink {
        fn audio(&self, payload: String) {
            self.audio_payloads.lock().unwrap().push(payload);
            self.audio.lock().unwrap().push(Instant::now());
        }
        fn web_speech(&self, p: WebSpeechPayload) {
            self.speech.lock().unwrap().push(p.text);
        }
        fn cancel(&self) {
            *self.cancels.lock().unwrap() += 1;
        }
        fn notice(&self, _: &'static str, message: String) {
            self.notices.lock().unwrap().push(message);
        }
        fn queue_state(&self, _: TtsQueueStatePayload) {}
    }
    fn msg(id: &str) -> ChatMessage {
        serde_json::from_value(json!({
            "id": id, "platform": "twitch", "channel": "channel",
            "author": {"id":"user", "name":"Alice"},
            "fragments": [{"type":"text", "text":id}], "kind":"normal", "timestampMs":0
        }))
        .unwrap()
    }
    fn config(url: &str) -> TtsConfig {
        let mut cfg = TtsConfig::default();
        cfg.backend = TtsBackendKind::Voicevox;
        cfg.options.voicevox_url = url.into();
        cfg.options.read_name = false;
        cfg
    }
    fn wav(ms: u32) -> Vec<u8> {
        let size = ms * 2;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + size).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1000u32.to_le_bytes());
        b.extend_from_slice(&2000u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&size.to_le_bytes());
        b.resize(44 + size as usize, 0);
        b
    }
    async fn until(mut f: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("condition timed out");
    }
    struct Running {
        shared: Arc<TtsShared>,
        sink: Arc<Sink>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<()>,
    }
    impl Running {
        fn start(cfg: TtsConfig, shared: Arc<TtsShared>, sink: Arc<Sink>) -> Self {
            let cancel = CancellationToken::new();
            let task = tokio::spawn(run_tts_worker(
                shared.clone(),
                sink.clone(),
                TtsDispatcher::new(cfg.clone()),
                move || cfg.clone(),
                cancel.clone(),
            ));
            Self {
                shared,
                sink,
                cancel,
                task,
            }
        }
        fn push(&self, id: &str) {
            self.shared.enqueue(self.sink.as_ref(), msg(id), 64);
        }
        async fn stop(self) {
            self.cancel.cancel();
            self.task.await.unwrap();
        }
    }
    struct Gate {
        entered: Notify,
        release: Semaphore,
        calls: Mutex<usize>,
        fail: bool,
    }
    async fn gated_engine(fail: bool) -> (String, Arc<Gate>, tokio::task::JoinHandle<()>) {
        let gate = Arc::new(Gate {
            entered: Notify::new(),
            release: Semaphore::new(0),
            calls: Mutex::new(0),
            fail,
        });
        let app = Router::new()
            .route(
                "/audio_query",
                post(|| async { Json(json!({"speedScale":1.0})) }),
            )
            .route(
                "/synthesis",
                post(|State(g): State<Arc<Gate>>| async move {
                    let first = {
                        let mut n = g.calls.lock().unwrap();
                        *n += 1;
                        *n == 1
                    };
                    if first {
                        g.entered.notify_one();
                        g.release.acquire().await.unwrap().forget();
                    }
                    if g.fail {
                        (StatusCode::INTERNAL_SERVER_ERROR, Vec::new())
                    } else {
                        {
                            let mut audio = wav(1);
                            *audio.last_mut().unwrap() = if first { 1 } else { 2 };
                            (StatusCode::OK, audio)
                        }
                    }
                }),
            )
            .with_state(gate.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, gate, task)
    }
    async fn cancellation_case(action: &str, fail: bool) {
        let (url, gate, server) = gated_engine(fail).await;
        let r = Running::start(config(&url), Arc::default(), Arc::default());
        r.push("first");
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .unwrap();
        r.push("second");
        if action == "skip" {
            r.push("third");
        }
        match action {
            "clear" => r.shared.clear(r.sink.as_ref()),
            "skip" => r.shared.skip_current(r.sink.as_ref()),
            "pause" => {
                r.shared.set_paused(r.sink.as_ref(), true);
                assert_eq!(
                    r.shared.enqueue(r.sink.as_ref(), msg("rejected"), 64),
                    PushResult::RejectedPaused
                );
            }
            _ => unreachable!(),
        }
        gate.release.add_permits(1);
        // With the first request released, the worker must still gate its result.
        if action == "pause" {
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(r.sink.audio.lock().unwrap().is_empty());
            assert!(r.sink.speech.lock().unwrap().is_empty());
            r.shared.set_paused(r.sink.as_ref(), false);
        }
        if action != "skip" {
            r.push("next");
        }
        if fail {
            until(|| r.sink.speech.lock().unwrap().len() == 1).await;
            assert_eq!(*r.sink.speech.lock().unwrap(), ["next"]);
        } else {
            let expected = if action == "skip" { 2 } else { 1 };
            until(|| r.sink.audio.lock().unwrap().len() == expected).await;
            let mut audio = wav(1);
            *audio.last_mut().unwrap() = 2;
            let expected_payload = crate::tts::voicevox::base64_encode::encode(&audio);
            assert!(r
                .sink
                .audio_payloads
                .lock()
                .unwrap()
                .iter()
                .all(|p| p == &expected_payload));
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            r.sink.audio.lock().unwrap().len(),
            if action == "skip" {
                2
            } else {
                usize::from(!fail)
            }
        );
        assert_eq!(
            *gate.calls.lock().unwrap(),
            if action == "skip" { 3 } else { 2 }
        );
        assert_eq!(*r.sink.cancels.lock().unwrap(), 1);
        r.stop().await;
        server.abort();
    }
    #[tokio::test]
    async fn cancel_during_synthesis_emits_no_audio() {
        cancellation_case("clear", false).await;
    }
    #[tokio::test]
    async fn skip_during_synthesis_drops_only_that_item() {
        cancellation_case("skip", false).await;
    }
    #[tokio::test]
    async fn pause_during_synthesis_emits_no_audio_and_rejects_new() {
        cancellation_case("pause", false).await;
    }
    #[tokio::test]
    async fn fallback_webspeech_is_not_emitted_after_clear() {
        cancellation_case("clear", true).await;
    }

    #[tokio::test]
    async fn second_audio_waits_for_first_duration() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().wav = wav(200);
        let r = Running::start(config(&fake.url), Arc::default(), Arc::default());
        r.push("first");
        r.push("second");
        until(|| r.sink.audio.lock().unwrap().len() == 2).await;
        let times = r.sink.audio.lock().unwrap().clone();
        // 50ms の余裕は IPC 送出時間も吸収するため、WAV 長の下限を検証する。
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(200));
        r.stop().await;
    }
    #[tokio::test]
    async fn skip_during_playback_releases_next_immediately() {
        let fake = spawn_fake_engine().await;
        fake.state.lock().unwrap().wav = wav(5000);
        let r = Running::start(config(&fake.url), Arc::default(), Arc::default());
        r.push("first");
        r.push("second");
        until(|| {
            r.sink.audio.lock().unwrap().len() == 1
                && fake.state.lock().unwrap().syntheses.len() == 2
        })
        .await;
        let skipped = Instant::now();
        r.shared.skip_current(r.sink.as_ref());
        until(|| r.sink.audio.lock().unwrap().len() == 2).await;
        assert!(skipped.elapsed() < Duration::from_secs(2));
        assert_eq!(*r.sink.cancels.lock().unwrap(), 1);
        r.stop().await;
    }
    #[tokio::test]
    async fn expired_items_are_skipped_oldest_first() {
        let fake = spawn_fake_engine().await;
        let shared = Arc::new(TtsShared::default());
        let sink = Arc::new(Sink::default());
        for (id, age) in [("old1", 10), ("old2", 5), ("fresh", 0)] {
            shared.core.lock().unwrap().push(
                TtsJob {
                    msg: msg(id),
                    item: TtsQueueItem::from_message(&msg(id)),
                    enqueued_at: Instant::now() - Duration::from_secs(age),
                },
                64,
            );
        }
        let mut cfg = config(&fake.url);
        cfg.options.queue.max_wait_sec = 1;
        let r = Running::start(cfg, shared, sink);
        until(|| r.sink.audio.lock().unwrap().len() == 1).await;
        let queries = fake.state.lock().unwrap().queries.clone();
        assert_eq!(
            queries.iter().map(|q| q.0.as_str()).collect::<Vec<_>>(),
            ["fresh"]
        );
        assert_eq!(r.sink.notices.lock().unwrap().len(), 1);
        assert!(r.sink.notices.lock().unwrap()[0].contains("2 件"));
        r.stop().await;
    }
    #[tokio::test]
    async fn queue_overflow_drops_oldest_and_keeps_newest() {
        let fake = spawn_fake_engine().await;
        let shared = Arc::new(TtsShared::default());
        let sink = Arc::new(Sink::default());
        for id in ["old1", "old2", "new1", "new2"] {
            shared.enqueue(sink.as_ref(), msg(id), 2);
        }
        assert_eq!(sink.notices.lock().unwrap().len(), 1); // 30 秒スロットル
        let r = Running::start(config(&fake.url), shared, sink);
        until(|| r.sink.audio.lock().unwrap().len() == 2).await;
        let queries = fake.state.lock().unwrap().queries.clone();
        assert_eq!(
            queries.iter().map(|q| q.0.as_str()).collect::<Vec<_>>(),
            ["new1", "new2"]
        );
        r.stop().await;
    }
    #[tokio::test]
    async fn pending_count_reaches_auto_speed() {
        let fake = spawn_fake_engine().await;
        let shared = Arc::new(TtsShared::default());
        let sink = Arc::new(Sink::default());
        for id in ["first", "second", "third"] {
            shared.enqueue(sink.as_ref(), msg(id), 64);
        }
        let mut cfg = config(&fake.url);
        cfg.options.auto_speed.enabled = true;
        cfg.options.auto_speed.queue_threshold = 0;
        cfg.options.auto_speed.per_queued = 0.2;
        cfg.options.auto_speed.length_threshold = 0;
        cfg.options.auto_speed.max_speed = 2.0;
        let r = Running::start(cfg, shared, sink);
        until(|| r.sink.audio.lock().unwrap().len() == 3).await;
        let speeds: Vec<f64> = fake
            .state
            .lock()
            .unwrap()
            .syntheses
            .iter()
            .map(|(_, q)| q["speedScale"].as_f64().unwrap())
            .collect();
        for (actual, expected) in speeds.iter().zip([1.4, 1.2, 1.0]) {
            assert!((actual - expected).abs() < 0.0001);
        }
        r.stop().await;
    }
}
