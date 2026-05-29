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

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::TtsBackend;

const CMD_TALK: u16 = 0x0001;
const CHARCODE_UTF8: u8 = 0;

/// 接続〜送出のタイムアウト(stall 時に速やかに失敗し Web Speech へフォールバック)。
const SPEAK_TIMEOUT: Duration = Duration::from_secs(3);

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
            let mut stream = TcpStream::connect(&addr).await?;
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
        let connect = TcpStream::connect(self.addr());
        matches!(
            tokio::time::timeout(std::time::Duration::from_millis(500), connect).await,
            Ok(Ok(_))
        )
    }
}
