//! TTS(読み上げ)層 — 3バックエンドをアダプタ化する。
//!
//! - bouyomi  : 棒読みちゃん(TCP 127.0.0.1:50001 バイナリコマンド)
//! - voicevox : VOICEVOX(HTTP /audio_query → /synthesis)
//! - webspeech: 実再生は UI 側(`speechSynthesis`)。Rust は読み上げ対象テキストを
//!   イベントで UI に渡すだけ。
//!
//! ルーティング: 設定の優先バックエンドが `available()==false` なら Web Speech へ
//! フォールバックする。読み上げ整形(名前/URL/絵文字/長文)もここで行う。

pub mod bouyomi;
pub mod voicevox;

use tauri::{AppHandle, Emitter};

use crate::config::{TtsBackendKind, TtsConfig};
use crate::model::{ChatMessage, Fragment};

/// 読み上げバックエンドの共通インタフェース。
#[allow(async_fn_in_trait)]
pub trait TtsBackend {
    /// 整形済みテキストを読み上げる。
    async fn speak(&self, text: String) -> anyhow::Result<()>;
    /// バックエンドが現在利用可能か(接続可否の簡易判定)。
    async fn available(&self) -> bool;
}

/// 設定に基づき読み上げを振り分けるディスパッチャ。
///
/// `AppHandle` は Web Speech フォールバック時に UI へイベントを送るために持つ。
pub struct TtsDispatcher {
    config: TtsConfig,
    app: AppHandle,
}

impl TtsDispatcher {
    pub fn new(config: TtsConfig, app: AppHandle) -> Self {
        TtsDispatcher { config, app }
    }

    /// 設定を差し替える(設定更新コマンドから呼ぶ)。
    pub fn update_config(&mut self, config: TtsConfig) {
        self.config = config;
    }

    /// 1メッセージを読み上げる。整形 → 優先バックエンド → 失敗時フォールバック。
    pub async fn speak_message(&self, msg: &ChatMessage) {
        let text = self.format_for_speech(msg);
        if text.trim().is_empty() {
            return;
        }

        // TOCTOU と二重往復を避けるため、speak パスでは available() プローブを
        // 行わず直接 speak() を試み、Err なら Web Speech へフォールバックする。
        match self.config.backend {
            TtsBackendKind::None => {}
            TtsBackendKind::WebSpeech => self.emit_webspeech(&text),
            TtsBackendKind::Bouyomi => {
                let opt = &self.config.options;
                let backend = bouyomi::BouyomiBackend::new(
                    opt.bouyomi_host.clone(),
                    opt.bouyomi_port,
                    opt.bouyomi_speed,
                    opt.bouyomi_tone,
                    opt.bouyomi_volume,
                    opt.bouyomi_voice,
                );
                if let Err(e) = backend.speak(text.clone()).await {
                    tracing::warn!("bouyomi 読み上げ失敗→Web Speech へ: {e}");
                    self.emit_webspeech(&text);
                }
            }
            TtsBackendKind::Voicevox => {
                let opt = &self.config.options;
                let backend = voicevox::VoicevoxBackend::new(
                    opt.voicevox_url.clone(),
                    opt.voicevox_speaker,
                    self.app.clone(),
                );
                if let Err(e) = backend.speak(text.clone()).await {
                    tracing::warn!("voicevox 読み上げ失敗→Web Speech へ: {e}");
                    self.emit_webspeech(&text);
                }
            }
        }
    }

    /// Web Speech 用に「読み上げテキスト」を UI へ送る(実再生は UI)。
    fn emit_webspeech(&self, text: &str) {
        if let Err(e) = self.app.emit("tts-speak", text) {
            tracing::warn!("tts-speak emit 失敗: {e}");
        }
    }

    /// メッセージを読み上げ用テキストへ整形する。
    fn format_for_speech(&self, msg: &ChatMessage) -> String {
        let opt = &self.config.options;

        // 本文を組み立てる(絵文字除去オプション対応)。
        let mut body = String::new();
        for frag in &msg.fragments {
            match frag {
                Fragment::Text { text } => body.push_str(text),
                Fragment::Emote { name, .. } => {
                    if !opt.strip_emoji {
                        body.push_str(name);
                    }
                }
            }
        }

        // URL 省略。
        if opt.omit_url {
            body = omit_urls(&body);
        }

        // 名前の前置。
        let mut text = if opt.read_name && !msg.author.name.is_empty() {
            format!("{} {}", msg.author.name, body)
        } else {
            body
        };

        // 長文カット(char 単位)。
        if opt.max_length > 0 {
            let chars: Vec<char> = text.chars().collect();
            if chars.len() > opt.max_length {
                text = chars[..opt.max_length].iter().collect();
            }
        }

        text
    }
}

/// 本文中の URL を「URL省略」へ置換する(簡易: `http`/`https` で始まる連続非空白)。
fn omit_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for token in s.split_inclusive(char::is_whitespace) {
        let trimmed = token.trim_end();
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            out.push_str("URL省略");
            // 区切りの空白は維持。
            if token.len() != trimmed.len() {
                out.push_str(&token[trimmed.len()..]);
            }
        } else {
            out.push_str(token);
        }
    }
    out
}
