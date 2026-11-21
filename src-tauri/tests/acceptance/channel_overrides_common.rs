// registered in main.rs by story OV-001 (the first of OV-001..OV-003 to land; the others rely on it already being there)
//! 枠ごとの上書き(OV-000)の受け入れテスト共通の小道具。`.omc/plans/channel-overrides-acceptance.md` §1 の公開面だけに依存する。
#![allow(dead_code)]

use fast_comment_lib::config::{commit_config, AppConfig, ChannelConfig, ChannelOverrides, ChannelTtsVoice};
use serde_json::{json, Value};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `overrides` を持たない旧形式の行(`platform` は "twitch" など小文字)。
pub fn chan(platform: &str, identifier: &str) -> ChannelConfig {
    serde_json::from_value(json!({"platform": platform, "identifier": identifier, "enabled": true}))
        .expect("チャンネル行として読める")
}

/// `chan` に overrides を足した行(JSON 経由なので ChannelConfig の構造体リテラルに依存しない)。
pub fn chan_with(platform: &str, identifier: &str, overrides: Value) -> ChannelConfig {
    serde_json::from_value(json!({
        "platform": platform, "identifier": identifier, "enabled": true, "overrides": overrides
    }))
    .expect("overrides 付きのチャンネル行として読める")
}

pub fn cfg_with(channels: Vec<ChannelConfig>) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.channels = channels;
    cfg
}

pub fn off() -> ChannelOverrides {
    ChannelOverrides { tts_enabled: Some(false), tts_voice: None }
}

pub fn voice_ov(voicevox: Option<u32>, bouyomi: Option<i16>, web: Option<&str>) -> ChannelOverrides {
    ChannelOverrides {
        tts_enabled: None,
        tts_voice: Some(ChannelTtsVoice {
            voicevox_speaker: voicevox,
            bouyomi_voice: bouyomi,
            web_speech_voice: web.map(str::to_owned),
        }),
    }
}

pub fn overrides_of<'a>(cfg: &'a AppConfig, idx: usize) -> &'a ChannelOverrides {
    &cfg.channels[idx].overrides
}

/// `channels` を一度保存して revision=1 にした状態(ディスク・メモリ・watch が一致)。
/// 受信側 `Receiver` も返す(watch の変化を見る用)。
pub fn baseline_with(
    channels: Vec<ChannelConfig>,
) -> (
    tempfile::TempDir,
    AtomicU64,
    tokio::sync::watch::Sender<AppConfig>,
    tokio::sync::watch::Receiver<AppConfig>,
    AppConfig,
) {
    let dir = tempfile::tempdir().unwrap();
    let rev = AtomicU64::new(0);
    let mut cur = AppConfig::default();
    let (tx, rx) = tokio::sync::watch::channel(cur.clone());
    let r = commit_config(dir.path(), &rev, &tx, &mut cur, cfg_with(channels), None).expect("初回保存");
    assert_eq!(r, 1);
    (dir, rev, tx, rx, cur)
}

pub fn disk_json(dir: &std::path::Path) -> Value {
    serde_json::from_slice(&std::fs::read(AppConfig::path_in(dir)).unwrap()).unwrap()
}

/// 待ちの上限つきで 1 件受け取る(固定 sleep ではない)。
pub async fn recv_within<T: Clone>(rx: &mut tokio::sync::broadcast::Receiver<T>, what: &str) -> T {
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Ok(Ok(v)) => v,
        other => panic!("{what}: 受け取れなかった ({:?})", other.map(|r| r.map(|_| ()))),
    }
}

// ---------------------------------------------------------------- ログ捕捉(twitcasting_common と同じ形。別系統なので複製)

#[derive(Clone, Default)]
pub struct LogBuf(Arc<Mutex<Vec<u8>>>);

impl LogBuf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

/// このスレッドに限って、アプリのログだけを全レベルで捕捉する。`#[tokio::test(flavor = "current_thread")]` で呼ぶこと。
pub fn capture_logs() -> (LogBuf, tracing::subscriber::DefaultGuard) {
    let buf = LogBuf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info,fast_comment_lib=trace"))
        .with_ansi(false)
        .with_writer(buf.clone())
        .finish();
    (buf, tracing::subscriber::set_default(subscriber))
}
