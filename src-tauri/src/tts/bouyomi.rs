//! 棒読みちゃん(BouyomiChan)TTS バックエンド。
//!
//! `TCP 127.0.0.1:50001` へ独自バイナリコマンドを送る。
//! コマンド 0x0001(Talk)のレイアウト(すべてリトルエンディアン):
//!
//! | offset | size | 内容                              |
//! |--------|------|-----------------------------------|
//! | 0      | 2    | コマンド = 0x0001                  |
//! | 2      | 2    | 速度 speed (i16, -1=既定)          |
//! | 4      | 2    | 音程 tone  (i16, -1=既定)          |
//! | 6      | 2    | 音量 volume(i16, -1=既定)          |
//! | 8      | 2    | 声質 voice (i16, 0=既定)           |
//! | 10     | 1    | 文字コード(0 = UTF-8)             |
//! | 11     | 4    | 本文バイト長(i32)                 |
//! | 15     | n    | 本文(UTF-8)                       |

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream as TokioTcpStream;

use super::TtsBackend;

const CMD_TALK: u16 = 0x0001;
const CHARCODE_UTF8: u8 = 0;

/// 接続〜送出のタイムアウト(stall 時に速やかに失敗し Web Speech へフォールバック)。
const SPEAK_TIMEOUT: Duration = Duration::from_secs(3);
const LAUNCH_CHECK_TIMEOUT: Duration = Duration::from_millis(300);

/// 起動試行のクールダウン。棒読みちゃんは spawn 後 TCP を listen し始めるまで
/// 数秒かかるため、その間に再度 ensure_launched が呼ばれても二重起動しないよう、
/// 直近の起動試行からこの時間内は再 spawn を抑止する。
const LAUNCH_COOLDOWN: Duration = Duration::from_secs(15);

/// 最後に起動を試みた時刻(プロセス全体で共有)。重複起動防止に用いる。
static LAST_LAUNCH_ATTEMPT: Mutex<Option<Instant>> = Mutex::new(None);

/// 棒読みちゃんが未起動なら、指定された exe を起動する。
pub fn ensure_launched(path: String, host: String, port: u16) {
    let path = path.trim().to_string();
    if path.is_empty() {
        tracing::info!("棒読みちゃん: パス未設定のため自動起動しません");
        return;
    }

    let addr_text = format!("{}:{}", host.trim(), port);
    let addr = addr_text
        .parse::<SocketAddr>()
        .ok()
        .or_else(|| match addr_text.to_socket_addrs() {
            Ok(mut addrs) => addrs.next(),
            Err(e) => {
                tracing::warn!("棒読みちゃん接続先の解決に失敗: {addr_text}: {e}");
                None
            }
        });

    if let Some(addr) = addr {
        if TcpStream::connect_timeout(&addr, LAUNCH_CHECK_TIMEOUT).is_ok() {
            tracing::info!("棒読みちゃん: 既に {addr} で応答があるため自動起動しません");
            return;
        }
    }
    tracing::info!("棒読みちゃん: 未応答のため自動起動を試みます: {path}");

    // 重複起動防止: TCP 未応答でも、直近に起動を試みた直後なら棒読みちゃんがまだ
    // 起動処理中(listen 開始前)であり得る。クールダウン内なら再 spawn しない。
    {
        let mut last = LAST_LAUNCH_ATTEMPT.lock().unwrap();
        if let Some(t) = *last {
            if t.elapsed() < LAUNCH_COOLDOWN {
                tracing::info!("棒読みちゃん: 直近に起動を試みたばかり(起動処理中とみなし)、再起動をスキップします");
                return;
            }
        }
        *last = Some(Instant::now());
    }

    let mut cmd = Command::new(&path);
    // BouyomiChan は自身のフォルダを基準に設定/辞書を読むため、作業ディレクトリを
    // exe のあるフォルダに合わせる(別 cwd から起動すると初期化に失敗し即終了することがある)。
    if let Some(dir) = std::path::Path::new(&path).parent() {
        if !dir.as_os_str().is_empty() {
            cmd.current_dir(dir);
        }
    }
    match cmd.spawn() {
        Ok(_) => tracing::info!("棒読みちゃんを自動起動しました: {path}"),
        Err(e) => {
            tracing::warn!("棒読みちゃんの自動起動に失敗: {path}: {e}");
            // 起動に失敗した場合はクールダウンを解除し、次回(パス修正後など)に
            // 即座に再試行できるようにする。失敗試行で 15 秒ブロックしない。
            *LAST_LAUNCH_ATTEMPT.lock().unwrap() = None;
        }
    }
}

/// 棒読みちゃんへの送信設定。
pub struct BouyomiBackend {
    host: String,
    port: u16,
    speed: i16,
    tone: i16,
    volume: i16,
    voice: i16,
}

impl BouyomiBackend {
    pub fn new(host: String, port: u16, speed: i16, tone: i16, volume: i16, voice: i16) -> Self {
        BouyomiBackend {
            host,
            port,
            speed,
            tone,
            volume,
            voice,
        }
    }

    fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Talk コマンドのバイト列を組み立てる。
    fn build_packet(&self, text: &str) -> Vec<u8> {
        let body = text.as_bytes();
        let mut buf = Vec::with_capacity(15 + body.len());
        buf.extend_from_slice(&CMD_TALK.to_le_bytes());
        buf.extend_from_slice(&self.speed.to_le_bytes());
        buf.extend_from_slice(&self.tone.to_le_bytes());
        buf.extend_from_slice(&self.volume.to_le_bytes());
        buf.extend_from_slice(&self.voice.to_le_bytes());
        buf.push(CHARCODE_UTF8);
        buf.extend_from_slice(&(body.len() as i32).to_le_bytes());
        buf.extend_from_slice(body);
        buf
    }
}

impl TtsBackend for BouyomiBackend {
    async fn speak(&self, text: String) -> anyhow::Result<()> {
        let packet = self.build_packet(&text);
        let addr = self.addr();
        tokio::time::timeout(SPEAK_TIMEOUT, async {
            let mut stream = TokioTcpStream::connect(&addr).await?;
            stream.write_all(&packet).await?;
            stream.flush().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|_| anyhow::anyhow!("棒読みちゃん送出がタイムアウト({}秒)", SPEAK_TIMEOUT.as_secs()))??;
        Ok(())
    }

    async fn available(&self) -> bool {
        // ポートへ接続できるかで簡易判定(短いタイムアウト)。
        let connect = TokioTcpStream::connect(self.addr());
        matches!(
            tokio::time::timeout(std::time::Duration::from_millis(500), connect).await,
            Ok(Ok(_))
        )
    }
}
