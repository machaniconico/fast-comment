//! TTS(読み上げ)層 — 3バックエンドをアダプタ化する。
//!
//! - bouyomi  : 棒読みちゃん(TCP 127.0.0.1:50001 バイナリコマンド)
//! - voicevox : VOICEVOX(HTTP /audio_query → /synthesis)
//! - webspeech: 実再生は UI 側(`speechSynthesis`)。Rust は読み上げ対象テキストを
//!   イベントで UI に渡すだけ。
//!
//! ルーティング: 読み上げ時は優先バックエンドへ直接 `speak()` し、失敗したら
//! Web Speech へフォールバックする。読み上げ整形(名前/URL/絵文字/長文)もここで行う。

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
    ///
    /// speak パスでは TOCTOU と二重往復を避けるため呼ばない。設定 UI からの
    /// 疎通テスト用途として残す。
    #[allow(dead_code)]
    async fn available(&self) -> bool;
}

/// 設定に基づき読み上げを振り分けるディスパッチャ。
///
/// `AppHandle` は Web Speech フォールバック時に UI へイベントを送るために持つ。
pub struct TtsDispatcher {
    config: TtsConfig,
    app: AppHandle,
    bouyomi: bouyomi::BouyomiBackend,
    voicevox: voicevox::VoicevoxBackend,
}

impl TtsDispatcher {
    pub fn new(config: TtsConfig, app: AppHandle) -> Self {
        let bouyomi = Self::build_bouyomi(&config);
        let voicevox = Self::build_voicevox(&config, &app);
        TtsDispatcher {
            config,
            app,
            bouyomi,
            voicevox,
        }
    }

    /// 設定を差し替える(設定更新コマンドから呼ぶ)。
    pub fn update_config(&mut self, config: TtsConfig) {
        if self.config == config {
            return;
        }

        if !Self::same_bouyomi_config(&self.config, &config) {
            self.bouyomi = Self::build_bouyomi(&config);
        }
        if !Self::same_voicevox_config(&self.config, &config) {
            self.voicevox = Self::build_voicevox(&config, &self.app);
        }
        self.config = config;
    }

    fn same_bouyomi_config(a: &TtsConfig, b: &TtsConfig) -> bool {
        let a = &a.options;
        let b = &b.options;
        a.bouyomi_host == b.bouyomi_host
            && a.bouyomi_port == b.bouyomi_port
            && a.bouyomi_speed == b.bouyomi_speed
            && a.bouyomi_tone == b.bouyomi_tone
            && a.bouyomi_volume == b.bouyomi_volume
            && a.bouyomi_voice == b.bouyomi_voice
    }

    fn same_voicevox_config(a: &TtsConfig, b: &TtsConfig) -> bool {
        let a = &a.options;
        let b = &b.options;
        a.voicevox_url == b.voicevox_url && a.voicevox_speaker == b.voicevox_speaker
    }

    fn build_bouyomi(config: &TtsConfig) -> bouyomi::BouyomiBackend {
        let opt = &config.options;
        bouyomi::BouyomiBackend::new(
            opt.bouyomi_host.clone(),
            opt.bouyomi_port,
            opt.bouyomi_speed,
            opt.bouyomi_tone,
            opt.bouyomi_volume,
            opt.bouyomi_voice,
        )
    }

    fn build_voicevox(config: &TtsConfig, app: &AppHandle) -> voicevox::VoicevoxBackend {
        let opt = &config.options;
        voicevox::VoicevoxBackend::new(
            opt.voicevox_url.clone(),
            opt.voicevox_speaker,
            app.clone(),
        )
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
                if let Err(e) = self.bouyomi.speak(text.clone()).await {
                    tracing::warn!("bouyomi 読み上げ失敗→Web Speech へ: {e}");
                    self.emit_webspeech(&text);
                }
            }
            TtsBackendKind::Voicevox => {
                if let Err(e) = self.voicevox.speak(text.clone()).await {
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
