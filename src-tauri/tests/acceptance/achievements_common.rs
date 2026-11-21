// registered in main.rs by story ACH-001 (the first of ACH-001..ACH-003 to land; the others rely on it already being there)
//! 実績(ACH-000)の受け入れテスト共通の小道具。`.omc/plans/achievements-acceptance.md` §1 の公開契約だけに依存する。
//! 時刻はすべて引数 `now_ms` で与え、`SystemTime` には頼らない。I/O の失敗は `StoreIo` の差し替えで起こす。
#![allow(dead_code)]

use crate::common::msg;
use fast_comment_lib::achievements::engine::AchievementUnlocked;
use fast_comment_lib::achievements::model::{
    Achievement, Announce, Condition, CurrencyMode, Metric, SubjectKind, Tier, Window,
};
use fast_comment_lib::achievements::store::{AchievementDefs, FsIo, StoreIo};
use fast_comment_lib::model::{Amount, Badge, ChatMessage, LiveEvent, LiveStream, MessageKind, Platform};
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 基準時刻(ms)。
pub const T0: u64 = 1_000_000_000;

// ---- 定義の組み立て ----

pub fn cond(window: Window, metric: Metric, threshold: f64, currency: CurrencyMode) -> Condition {
    Condition { window, metric, threshold, currency, platforms: vec![] }
}

pub fn make(id: &str, subject: SubjectKind, tier: Tier, condition: Option<Condition>) -> Achievement {
    Achievement {
        id: id.to_string(),
        name: format!("name-{id}"),
        description: format!("desc-{id}"),
        icon: "star".to_string(),
        tier,
        subject,
        enabled: true,
        builtin_id: None,
        edited: false,
        condition,
        announce: Announce { tts: false },
    }
}

/// 視聴者・Lifetime・Bronze の回数系実績。
pub fn viewer_count(id: &str, metric: Metric, threshold: f64) -> Achievement {
    make(id, SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::Lifetime, metric, threshold, CurrencyMode::None)))
}

/// 視聴者・Lifetime・Bronze の金額系実績。
pub fn viewer_amount(id: &str, threshold: f64, currency: CurrencyMode) -> Achievement {
    make(id, SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::Lifetime, Metric::DonationAmount, threshold, currency)))
}

/// 視聴者・PerStream の回数系実績。
pub fn viewer_per_stream(id: &str, metric: Metric, threshold: f64) -> Achievement {
    make(id, SubjectKind::Viewer, Tier::Bronze, Some(cond(Window::PerStream, metric, threshold, CurrencyMode::None)))
}

/// 配信者の回数系実績。
pub fn streamer_count(id: &str, window: Window, metric: Metric, threshold: f64) -> Achievement {
    make(id, SubjectKind::Streamer, Tier::Bronze, Some(cond(window, metric, threshold, CurrencyMode::None)))
}

pub fn platinum(id: &str, subject: SubjectKind) -> Achievement {
    make(id, subject, Tier::Platinum, None)
}

pub fn with_platforms(mut a: Achievement, platforms: Vec<Platform>) -> Achievement {
    a.condition.as_mut().expect("条件つき").platforms = platforms;
    a
}

pub fn disabled(mut a: Achievement) -> Achievement {
    a.enabled = false;
    a
}

/// 有効化済みの定義(レートは既定の `{"JPY":1.0}`)。
pub fn mk_defs(list: Vec<Achievement>) -> AchievementDefs {
    let mut d = AchievementDefs::default();
    d.enabled = true;
    d.achievements = list;
    d
}

// ---- メッセージの組み立て ----

pub fn chat(id: &str, platform: Platform, channel: &str, author_id: &str) -> ChatMessage {
    let mut m = msg(id);
    m.platform = platform;
    m.channel = channel.to_string();
    m.author.id = author_id.to_string();
    m
}

/// Twitch / "chan" / 著者 "u1" の通常コメント(`common::msg` と同じ既定)。
pub fn cm(id: &str) -> ChatMessage {
    msg(id)
}

pub fn cm_by(id: &str, author_id: &str) -> ChatMessage {
    let mut m = msg(id);
    m.author.id = author_id.to_string();
    m
}

pub fn with_amount(mut m: ChatMessage, kind: MessageKind, value: f64, currency: &str) -> ChatMessage {
    m.kind = kind;
    m.amount = Some(Amount { value, currency: currency.to_string(), raw_text: format!("{currency}{value}") });
    m
}

/// YouTube の SuperChat(`currency` は受信したままの記号・コード)。
pub fn super_chat(id: &str, author_id: &str, value: f64, currency: &str) -> ChatMessage {
    let m = chat(id, Platform::Youtube, "UCchan", author_id);
    with_amount(m, MessageKind::SuperChat, value, currency)
}

pub fn uncounted(mut m: ChatMessage) -> ChatMessage {
    m.route.uncounted = true;
    m
}

pub fn with_badge(mut m: ChatMessage, kind: &str) -> ChatMessage {
    m.author.badges.push(Badge { kind: kind.to_string(), label: kind.to_string(), image_url: None });
    m
}

// ---- LiveEvent ----

pub fn started(platform: Platform, identifier: &str, stream_id: &str) -> LiveEvent {
    LiveEvent::Started(LiveStream {
        platform,
        channel_identifier: identifier.to_string(),
        stream_id: stream_id.to_string(),
        title: None,
        url: format!("https://example.invalid/{stream_id}"),
        started_at_ms: None,
    })
}

pub fn ended(platform: Platform, identifier: &str) -> LiveEvent {
    LiveEvent::Ended { platform, channel_identifier: identifier.to_string() }
}

// ---- 結果の見方 ----

pub fn ids(v: &[AchievementUnlocked]) -> Vec<String> {
    let mut out: Vec<String> = v.iter().map(|u| u.achievement_id.clone()).collect();
    out.sort();
    out
}

pub fn to_json<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("シリアライズできる")
}

pub fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("{} を読めない: {e}", path.display()))
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&read(path)).expect("JSON として読める")
}

/// `dir` 直下で名前が `prefix` で始まるファイル(名前順)。
pub fn files_with_prefix(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with(prefix))
        .collect();
    v.sort();
    v
}

pub fn file_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

// ---- StoreIo の差し替え ----

/// 実 I/O(`FsIo`)に委ねつつ、書き込みの記録と失敗の注入ができる。
pub struct TestIo {
    writes: Mutex<Vec<(PathBuf, Vec<u8>)>>,
    attempts: AtomicUsize,
    fail_writes: AtomicUsize,
    fail_backup: AtomicBool,
}

impl TestIo {
    pub fn new() -> Arc<TestIo> {
        Arc::new(TestIo {
            writes: Mutex::new(vec![]),
            attempts: AtomicUsize::new(0),
            fail_writes: AtomicUsize::new(0),
            fail_backup: AtomicBool::new(false),
        })
    }
    /// これ以降の `write_atomic` を `n` 回だけ失敗させる。
    pub fn fail_next_writes(&self, n: usize) {
        self.fail_writes.store(n, Ordering::SeqCst);
    }
    pub fn fail_backups(&self, on: bool) {
        self.fail_backup.store(on, Ordering::SeqCst);
    }
    /// 成功した書き込み(パスと bytes)。
    pub fn written(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.writes.lock().unwrap().clone()
    }
    /// 失敗も含む `write_atomic` の呼び出し回数。
    pub fn write_attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
    /// 成功した書き込みのうち、`name` というファイル名への最後の bytes を文字列で。
    pub fn last_text_of(&self, name: &str) -> Option<String> {
        self.writes
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(p, _)| p.file_name().map(|n| n == name).unwrap_or(false))
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    }
}

impl StoreIo for TestIo {
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let left = self.fail_writes.load(Ordering::SeqCst);
        if left > 0 {
            self.fail_writes.store(left - 1, Ordering::SeqCst);
            return Err(io::Error::new(io::ErrorKind::Other, "injected write failure"));
        }
        FsIo.write_atomic(path, bytes)?;
        self.writes.lock().unwrap().push((path.to_path_buf(), bytes.to_vec()));
        Ok(())
    }
    fn create_backup(&self, src: &Path, dst: &Path) -> io::Result<()> {
        if self.fail_backup.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::Other, "injected backup failure"));
        }
        FsIo.create_backup(src, dst)
    }
}
