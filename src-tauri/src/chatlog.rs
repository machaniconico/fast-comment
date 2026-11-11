//! Bounded, nonblocking ingress; all serialization and disk work belongs to the writer.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use chrono::{Local, NaiveDateTime, TimeZone};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};

use crate::{config::LogConfig, model::ChatMessage};

const CAPACITY: usize = 4096;
const ROTATE_BYTES: u64 = 50 * 1024 * 1024;

enum Command {
    Message(ChatMessage, bool),
    Configure(LogConfig, oneshot::Sender<Result<(), String>>),
    Shutdown(oneshot::Sender<Result<(), String>>),
}

pub struct ChatLog {
    tx: mpsc::Sender<Command>,
    dropped: AtomicU64,
}

impl ChatLog {
    pub fn start(config_dir: &Path, config: LogConfig) -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);
        let dir = log_dir(config_dir);
        tauri::async_runtime::spawn(run(dir, config, rx, ROTATE_BYTES));
        Self {
            tx,
            dropped: AtomicU64::new(0),
        }
    }

    /// Never waits for disk, serialization, or channel capacity.
    pub fn try_send(&self, msg: ChatMessage, hidden: bool) {
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.tx.try_send(Command::Message(msg, hidden))
        {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Rate-limit warnings during sustained overload.
            if dropped == 1 || dropped % 1024 == 0 {
                tracing::warn!(dropped, "コメントログキュー満杯: drop");
            }
        }
    }

    pub async fn configure(&self, config: LogConfig) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Configure(config, tx))
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }

    pub async fn shutdown(&self) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Command::Shutdown(tx))
            .await
            .map_err(|e| e.to_string())?;
        rx.await.map_err(|e| e.to_string())?
    }
}

pub fn log_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("comments")
}

struct Writer {
    dir: PathBuf,
    stem: String,
    sequence: u32,
    file: Option<BufWriter<File>>,
    bytes: u64,
    limit: u64,
}

impl Writer {
    fn new(dir: PathBuf, limit: u64) -> Self {
        Self {
            dir,
            stem: String::new(),
            sequence: 0,
            file: None,
            bytes: 0,
            limit,
        }
    }

    async fn open(&mut self, new_session: bool) -> io::Result<()> {
        self.close().await?;
        fs::create_dir_all(&self.dir).await?;
        if new_session {
            self.stem = format!("comments_{}", Local::now().format("%Y%m%d-%H%M%S"));
            self.sequence = 0;
        }
        loop {
            self.sequence += 1;
            let suffix = if self.sequence == 1 {
                String::new()
            } else {
                format!("_{}", self.sequence)
            };
            let path = self.dir.join(format!("{}{suffix}.jsonl", self.stem));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)
                .await
            {
                Ok(file) => {
                    self.file = Some(BufWriter::new(file));
                    self.bytes = 0;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush().await?;
        }
        Ok(())
    }

    async fn close(&mut self) -> io::Result<()> {
        self.flush().await?;
        if let Some(file) = self.file.take() {
            file.get_ref().sync_all().await?;
        }
        Ok(())
    }

    async fn write(&mut self, mut msg: ChatMessage) -> io::Result<()> {
        msg.raw = None;
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        if self.bytes > 0 && self.bytes.saturating_add(line.len() as u64) > self.limit {
            self.open(false).await?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("comment log is closed"))?;
        file.write_all(&line).await?;
        self.bytes += line.len() as u64;
        Ok(())
    }
}

/// Only our exact filename grammar is eligible. Prefer mtime; the local filename
/// timestamp also lets imported/old sessions expire after their mtime changes.
async fn prune(dir: &Path, days: u32) -> io::Result<()> {
    if days == 0 {
        return Ok(());
    }
    let Some(cutoff) = SystemTime::now().checked_sub(Duration::from_secs(u64::from(days) * 86400))
    else {
        return Ok(());
    };
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name
            .strip_prefix("comments_")
            .and_then(|s| s.strip_suffix(".jsonl"))
        else {
            continue;
        };
        let (stamp, suffix) = stem.split_once('_').unwrap_or((stem, ""));
        if !suffix.is_empty() && !suffix.parse::<u32>().is_ok_and(|n| n >= 2) {
            continue;
        }
        if stem.ends_with('_') {
            continue;
        }
        let Ok(date) = NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S") else {
            continue;
        };
        let metadata = entry.metadata().await?;
        if !metadata.is_file() || entry.file_type().await?.is_symlink() {
            continue;
        }
        let named_time = Local
            .from_local_datetime(&date)
            .latest()
            .map(SystemTime::from);
        if metadata.modified().is_ok_and(|time| time < cutoff)
            || named_time.is_some_and(|time| time < cutoff)
        {
            if let Err(e) = fs::remove_file(entry.path()).await {
                tracing::warn!("コメントログ削除失敗: {e}");
            }
        }
    }
    Ok(())
}

async fn run(dir: PathBuf, mut config: LogConfig, mut rx: mpsc::Receiver<Command>, limit: u64) {
    let mut writer = Writer::new(dir, limit);
    if let Err(e) = prune(&writer.dir, config.retention_days).await {
        tracing::warn!("ログ保持期限処理失敗: {e}");
    }
    if config.enabled {
        if let Err(e) = writer.open(true).await {
            tracing::error!("コメントログ作成失敗: {e}");
        }
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            command = rx.recv() => match command {
                Some(Command::Message(msg, hidden)) => {
                    if config.enabled && (!hidden || config.include_hidden) {
                        if writer.file.is_none() {
                            if let Err(e) = writer.open(true).await {
                                tracing::error!("コメントログ作成失敗: {e}");
                                continue;
                            }
                        }
                        if let Err(e) = writer.write(msg).await {
                            tracing::error!("コメントログ書込失敗: {e}");
                            // A failed write may leave a partial JSON line. Never append
                            // the next message to that line; retry in a fresh file.
                            if let Err(e) = writer.close().await {
                                tracing::error!("コメントログ close 失敗: {e}");
                            }
                            writer.file = None;
                        }
                    }
                }
                Some(Command::Configure(next, reply)) => {
                    let result = if next.enabled && (!config.enabled || writer.file.is_none()) {
                        writer.open(true).await
                    } else if !next.enabled { writer.close().await } else { Ok(()) };
                    config = next;
                    let _ = reply.send(result.map_err(|e| e.to_string()));
                }
                Some(Command::Shutdown(reply)) => {
                    // FIFO command: every previously queued message has been processed.
                    let _ = reply.send(writer.close().await.map_err(|e| e.to_string()));
                    return;
                }
                None => break,
            },
            _ = tick.tick() => {
                if let Err(e) = writer.flush().await { tracing::error!("コメントログ flush 失敗: {e}"); }
            }
        }
    }
    if let Err(e) = writer.close().await {
        tracing::error!("コメントログ終了失敗: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("fast-comment-chatlog-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn message(id: &str) -> ChatMessage {
        serde_json::from_value(serde_json::json!({
            "id": id, "platform": "twitch", "channel": "channel",
            "author": {"id": "user", "name": "Alice"},
            "fragments": [{"type":"text", "text":"hello\nworld"}],
            "kind": "normal", "timestampMs": 1,
            "raw": {"secret": "never save this"}
        }))
        .unwrap()
    }

    async fn lines(dir: &Path) -> Vec<serde_json::Value> {
        let mut entries = fs::read_dir(dir).await.unwrap();
        let mut result = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let text = fs::read_to_string(entry.path()).await.unwrap();
            for line in text.lines() {
                result.push(serde_json::from_str(line).unwrap());
            }
        }
        result
    }

    #[tokio::test]
    async fn chatlog_jsonl_and_rotation() {
        let dir = TestDir::new();
        let mut writer = Writer::new(dir.0.clone(), 1);
        writer.open(true).await.unwrap();
        let stem = writer.stem.clone();
        writer.write(message("one")).await.unwrap();
        writer.write(message("two")).await.unwrap();
        writer.write(message("three")).await.unwrap();
        writer.close().await.unwrap();
        for suffix in ["", "_2", "_3"] {
            let text = fs::read_to_string(dir.0.join(format!("{stem}{suffix}.jsonl")))
                .await
                .unwrap();
            assert_eq!(text.lines().count(), 1);
            let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
            assert!(value.get("raw").is_none());
            assert_eq!(value["fragments"][0]["text"], "hello\nworld");
            let decoded: ChatMessage = serde_json::from_value(value).unwrap();
            assert!(decoded.raw.is_none());
        }
    }

    #[tokio::test]
    async fn chatlog_retention_by_name_and_mtime() {
        let dir = TestDir::new();
        let today = Local::now().format("%Y%m%d-%H%M%S");
        let recent = format!("comments_{today}.jsonl");
        let old_mtime = format!("comments_{today}_2.jsonl");
        let names = [
            "comments_20000101-000000.jsonl",
            "comments_20000101-000000_2.jsonl",
            &recent,
            &old_mtime,
            "unrelated.jsonl",
            "comments_invalid.jsonl",
            "comments_20000101-000000_no.jsonl",
        ];
        for name in names {
            fs::write(dir.0.join(name), "").await.unwrap();
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.0.join(&old_mtime))
            .unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(60 * 86400))
            .unwrap();
        drop(file);
        prune(&dir.0, 0).await.unwrap();
        assert!(dir.0.join(names[0]).exists());
        prune(&dir.0, 30).await.unwrap();
        for name in &names[..2] {
            assert!(!dir.0.join(name).exists());
        }
        assert!(!dir.0.join(&old_mtime).exists());
        for name in [recent.as_str(), names[4], names[5], names[6]] {
            assert!(dir.0.join(name).exists());
        }
    }

    #[test]
    fn chatlog_full_queue_drops_without_panicking() {
        let (tx, mut rx) = mpsc::channel(CAPACITY);
        let log = ChatLog {
            tx,
            dropped: AtomicU64::new(0),
        };
        for _ in 0..CAPACITY + 10 {
            log.try_send(message("one"), false);
        }
        assert_eq!(log.dropped.load(Ordering::Relaxed), 10);
        assert_eq!(rx.len(), CAPACITY);
        rx.close();
        drop(rx);
        log.try_send(message("closed"), false);
    }

    #[tokio::test]
    async fn chatlog_toggle_hidden_and_shutdown_flush() {
        let dir = TestDir::new();
        let (tx, rx) = mpsc::channel(CAPACITY);
        let log = ChatLog {
            tx,
            dropped: AtomicU64::new(0),
        };
        let worker = tokio::spawn(run(dir.0.clone(), LogConfig::default(), rx, ROTATE_BYTES));
        log.try_send(message("visible"), false);
        log.try_send(message("hidden-excluded"), true);
        let mut cfg = LogConfig::default();
        cfg.enabled = false;
        log.configure(cfg.clone()).await.unwrap();
        // configure acknowledges only after flushing and closing the file.
        assert_eq!(lines(&dir.0).await.len(), 1);
        log.try_send(message("disabled"), false);
        cfg.enabled = true;
        cfg.include_hidden = true;
        log.configure(cfg).await.unwrap();
        log.try_send(message("hidden-included"), true);
        log.shutdown().await.unwrap();
        worker.await.unwrap();
        let values = lines(&dir.0).await;
        let mut ids: Vec<_> = values.iter().map(|v| v["id"].as_str().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, ["hidden-included", "visible"]);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn chatlog_periodic_flush() {
        let dir = TestDir::new();
        let (tx, rx) = mpsc::channel(CAPACITY);
        let log = ChatLog {
            tx,
            dropped: AtomicU64::new(0),
        };
        let worker = tokio::spawn(run(dir.0.clone(), LogConfig::default(), rx, ROTATE_BYTES));
        log.try_send(message("periodic"), false);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if lines(&dir.0).await.len() == 1 {
                    break;
                }
            }
        })
        .await
        .unwrap();
        log.shutdown().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn chatlog_disabled_start_and_sender_drop_flush() {
        let dir = TestDir::new();
        let path = dir.0.join("not-created-until-enabled");
        let (tx, rx) = mpsc::channel(CAPACITY);
        let log = ChatLog {
            tx,
            dropped: AtomicU64::new(0),
        };
        let config = LogConfig {
            enabled: false,
            ..LogConfig::default()
        };
        let worker = tokio::spawn(run(path.clone(), config.clone(), rx, ROTATE_BYTES));
        log.try_send(message("disabled"), false);
        log.configure(config).await.unwrap();
        assert!(!path.exists());
        log.configure(LogConfig::default()).await.unwrap();
        log.try_send(message("saved-on-disconnect"), false);
        drop(log);
        worker.await.unwrap();
        let saved = lines(&path).await;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0]["id"], "saved-on-disconnect");
    }

    #[tokio::test]
    async fn chatlog_new_session_never_overwrites_existing_file() {
        let dir = TestDir::new();
        let mut first = Writer::new(dir.0.clone(), ROTATE_BYTES);
        first.open(true).await.unwrap();
        first.write(message("existing")).await.unwrap();
        first.close().await.unwrap();

        // Force the same local timestamp, as when toggling within one second.
        let mut second = Writer::new(dir.0.clone(), ROTATE_BYTES);
        second.stem = first.stem.clone();
        second.open(false).await.unwrap();
        assert_eq!(second.sequence, 2);
        second.write(message("new-session")).await.unwrap();
        second.close().await.unwrap();
        let mut ids: Vec<_> = lines(&dir.0)
            .await
            .into_iter()
            .map(|v| v["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        assert_eq!(ids, ["existing", "new-session"]);
    }
}
