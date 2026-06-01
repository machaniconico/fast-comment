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

/// max_length == 0(無制限)の場合に適用する安全上限。
///
/// ユーザーが finite な max_length(> 0)を指定した場合はその値を完全に尊重し、
/// この定数は使用しない。長文 SuperChat 連発などで巨大ペイロードが IPC に
/// 流れるのを防ぐバックストップとしてのみ機能する。
const TTS_UNLIMITED_HARD_CAP: usize = 500;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::config::{TtsBackendKind, TtsConfig, TtsOptions};
use crate::model::{ChatMessage, Fragment};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WebSpeechPayload {
    text: String,
    rate: f32,
    pitch: f32,
    volume: f32,
    voice: String,
}

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
        let opt = &self.config.options;
        let payload = WebSpeechPayload {
            text: text.to_string(),
            rate: opt.web_speech_rate,
            pitch: opt.web_speech_pitch,
            volume: opt.web_speech_volume,
            voice: opt.web_speech_voice.clone(),
        };

        if let Err(e) = self.app.emit("tts-speak", payload) {
            tracing::warn!("tts-speak emit 失敗: {e}");
        }
    }

    /// メッセージを読み上げ用テキストへ整形する。
    fn format_for_speech(&self, msg: &ChatMessage) -> String {
        Self::format_for_speech_with_options(&self.config.options, msg)
    }

    fn format_for_speech_with_options(opt: &TtsOptions, msg: &ChatMessage) -> String {
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

        for entry in &opt.dictionary {
            if !entry.from.is_empty() {
                body = body.replace(&entry.from, &entry.to);
            }
        }

        // 名前の前置。
        let mut text = if opt.read_name && !msg.author.name.is_empty() {
            format!("{} {}", msg.author.name, body)
        } else {
            body
        };

        // 長文カット(char 単位)。
        // finite 指定(> 0)はユーザー意図として完全に尊重する。
        // 0(無制限)の場合も TTS_UNLIMITED_HARD_CAP を上限として適用し、
        // 巨大 IPC ペイロードを防ぐバックストップとする。
        let effective_max = if opt.max_length > 0 {
            opt.max_length
        } else {
            TTS_UNLIMITED_HARD_CAP
        };
        let chars: Vec<char> = text.chars().collect();
        if chars.len() > effective_max {
            text = chars[..effective_max].iter().collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TtsDictEntry, TtsOptions};
    use crate::model::{Author, MessageKind, Platform, Roles};

    fn message(author_name: &str, body: &str) -> ChatMessage {
        ChatMessage {
            id: "msg-1".to_string(),
            platform: Platform::Youtube,
            channel: "channel".to_string(),
            author: Author {
                id: "author-1".to_string(),
                name: author_name.to_string(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::Text {
                text: body.to_string(),
            }],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms: 0,
            raw: None,
            skip_tts: false,
        }
    }

    #[test]
    fn format_for_speech_applies_dictionary_to_body_before_name_and_truncation() {
        let mut options = TtsOptions {
            read_name: true,
            omit_url: true,
            strip_emoji: true,
            max_length: 10,
            dictionary: vec![
                TtsDictEntry {
                    from: "fast".to_string(),
                    to: "slow".to_string(),
                },
                TtsDictEntry {
                    from: "slow comment".to_string(),
                    to: "読み上げ辞書".to_string(),
                },
                TtsDictEntry {
                    from: "".to_string(),
                    to: "ignored".to_string(),
                },
                TtsDictEntry {
                    from: "URL省略".to_string(),
                    to: "リンク".to_string(),
                },
            ],
            ..TtsOptions::default()
        };

        let msg = message("fast", "fast comment https://example.com trailing");
        let text = TtsDispatcher::format_for_speech_with_options(&options, &msg);

        assert_eq!(text, "fast 読み上げ辞");

        options.read_name = false;
        options.max_length = 0;
        let text = TtsDispatcher::format_for_speech_with_options(&options, &msg);
        assert_eq!(text, "読み上げ辞書 リンク trailing");
    }
}
