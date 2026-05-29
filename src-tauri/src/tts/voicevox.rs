//! VOICEVOX TTS バックエンド。
//!
//! 既定 `http://127.0.0.1:50021`。手順:
//!  1. `POST /audio_query?text=...&speaker=N` で音声合成クエリ(JSON)取得。
//!  2. `POST /synthesis?speaker=N`(body=クエリJSON)で wav バイトを取得。
//!
//! Cargo に rodio 等の再生クレートが無いため、合成した wav は base64 にして
//! UI へ `tts-audio` イベントで送り、UI(WebAudio)で再生させる。
//! (「重い合成は Rust、再生は薄く UI」方針。)

use std::time::Duration;

use self::base64_encode::encode as b64encode;
use reqwest::Client;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use super::TtsBackend;

/// 合成リクエスト全体のタイムアウト(エンジン stall 時に永久待ちを防ぐ)。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// VOICEVOX エンジンへの設定。
pub struct VoicevoxBackend {
    base_url: String,
    speaker: u32,
    app: AppHandle,
    http: Client,
}

impl VoicevoxBackend {
    pub fn new(base_url: String, speaker: u32, app: AppHandle) -> Self {
        VoicevoxBackend {
            base_url: base_url.trim_end_matches('/').to_string(),
            speaker,
            app,
            http: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }
}

impl TtsBackend for VoicevoxBackend {
    async fn speak(&self, text: String) -> anyhow::Result<()> {
        let speaker = self.speaker.to_string();

        // 1) audio_query
        let query_url = format!("{}/audio_query", self.base_url);
        let query: Value = self
            .http
            .post(&query_url)
            .query(&[("text", text.as_str()), ("speaker", speaker.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // 2) synthesis(wav バイト)
        let synth_url = format!("{}/synthesis", self.base_url);
        let wav = self
            .http
            .post(&synth_url)
            .query(&[("speaker", speaker.as_str())])
            .header("Content-Type", "application/json")
            .json(&query)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;

        // 3) base64 にして UI へ。UI は data URI で再生する。
        let b64 = b64encode(&wav);
        if let Err(e) = self.app.emit("tts-audio", b64) {
            tracing::warn!("tts-audio emit 失敗: {e}");
        }
        Ok(())
    }

    async fn available(&self) -> bool {
        // /version を叩けるかで判定。
        let url = format!("{}/version", self.base_url);
        matches!(
            tokio::time::timeout(
                std::time::Duration::from_millis(500),
                self.http.get(&url).send()
            )
            .await,
            Ok(Ok(resp)) if resp.status().is_success()
        )
    }
}

/// 依存追加を避けるための最小 base64 エンコーダ(標準アルファベット)。
mod base64_encode {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    /// バイト列を base64 文字列へ変換する。
    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;

            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    }
}
