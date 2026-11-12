//! TTS(読み上げ)層 — 3バックエンドをアダプタ化する。
//!
//! - bouyomi  : 棒読みちゃん(TCP 127.0.0.1:50001 バイナリコマンド)
//! - voicevox : VOICEVOX 互換エンジン(HTTP /audio_query → /synthesis)
//! - webspeech: 実再生は UI 側(`speechSynthesis`)。Rust は読み上げ対象テキストを
//!   UI に渡すだけ。
//!
//! ルーティング: `TtsDispatcher::speak` は優先バックエンドを試し、失敗したら
//! Web Speech へフォールバックする。結果は [`SpeechOutput`] として返し、emit は
//! 呼び出し側(ワーカー)が [`TtsSink`] 経由で行う。読み上げ整形(名前/URL/絵文字/
//! 長文/テンプレート)もここで行う。

pub mod bouyomi;
pub mod params;
pub mod queue;
pub mod template;
pub mod voicevox;
pub mod wav;
pub mod worker;

/// max_length == 0(無制限)の場合に適用する安全上限。
///
/// ユーザーが finite な max_length(> 0)を指定した場合はその値を完全に尊重し、
/// この定数は使用しない。長文 SuperChat 連発などで巨大ペイロードが IPC に
/// 流れるのを防ぐバックストップとしてのみ機能する。
const TTS_UNLIMITED_HARD_CAP: usize = 500;
const TTS_NOTICE_THROTTLE: std::time::Duration = std::time::Duration::from_secs(30);

use regex::Regex;
use serde::Serialize;
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

use crate::config::{TtsBackendKind, TtsConfig, TtsDictEntry, TtsOptions};
use crate::listeners::TtsVoiceOverride;
use crate::model::{ChatMessage, Fragment, MessageKind};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSpeechPayload {
    pub text: String,
    pub rate: f32,
    pub pitch: f32,
    pub volume: f32,
    pub voice: String,
}

impl WebSpeechPayload {
    fn new(text: &str, opt: &TtsOptions, voice: Option<&TtsVoiceOverride>) -> Self {
        let mut voice = voice.cloned().unwrap_or_default();
        voice.normalize();
        Self {
            text: text.to_string(),
            rate: voice.rate.unwrap_or(opt.web_speech_rate),
            pitch: voice.pitch.unwrap_or(opt.web_speech_pitch),
            volume: opt.web_speech_volume,
            voice: voice
                .web_speech_voice
                .unwrap_or_else(|| opt.web_speech_voice.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtsNoticePayload {
    pub level: &'static str,
    pub message: String,
}

#[derive(Clone, Serialize)]
pub struct TtsQueueItem {
    pub id: String,
    pub preview: String,
}

#[derive(Clone, Serialize)]
pub struct TtsQueueStatePayload {
    pub depth: u32,
    pub paused: bool,
    pub items: Vec<TtsQueueItem>,
}

impl TtsQueueItem {
    pub fn from_message(msg: &ChatMessage) -> Self {
        Self {
            id: msg.id.clone(),
            preview: truncate_preview(&message_body_preview(msg), 40),
        }
    }
}

pub fn tts_queue_state(paused: bool, items: &VecDeque<TtsQueueItem>) -> TtsQueueStatePayload {
    TtsQueueStatePayload {
        depth: u32::try_from(items.len()).unwrap_or(u32::MAX),
        paused,
        items: items.iter().cloned().collect(),
    }
}

pub fn emit_tts_queue_state(app: &AppHandle, payload: TtsQueueStatePayload) {
    if let Err(e) = app.emit("tts-queue-state", payload) {
        tracing::warn!("tts-queue-state emit 失敗: {e}");
    }
}

/// 読み上げ結果の送出先。本番は `AppHandle`、テストは記録実装を使う。
pub trait TtsSink: Send + Sync {
    fn audio(&self, wav_base64: String);
    fn web_speech(&self, p: WebSpeechPayload);
    fn cancel(&self);
    fn notice(&self, level: &'static str, message: String);
    fn queue_state(&self, p: TtsQueueStatePayload);
}

impl TtsSink for AppHandle {
    fn audio(&self, wav_base64: String) {
        if let Err(e) = self.emit("tts-audio", wav_base64) {
            tracing::warn!("tts-audio emit 失敗: {e}");
        }
    }

    fn web_speech(&self, p: WebSpeechPayload) {
        if let Err(e) = self.emit("tts-speak", p) {
            tracing::warn!("tts-speak emit 失敗: {e}");
        }
    }

    fn cancel(&self) {
        if let Err(e) = self.emit("tts-cancel", ()) {
            tracing::warn!("tts-cancel emit 失敗: {e}");
        }
    }

    fn notice(&self, level: &'static str, message: String) {
        if let Err(e) = self.emit("tts-notice", TtsNoticePayload { level, message }) {
            tracing::warn!("tts-notice emit 失敗: {e}");
        }
    }

    fn queue_state(&self, p: TtsQueueStatePayload) {
        emit_tts_queue_state(self, p);
    }
}

/// `TtsDispatcher::speak` の結果。emit は呼び出し側が行う。
#[derive(Debug, Clone, PartialEq)]
pub enum SpeechOutput {
    /// 読み上げ対象外、またはバックエンド無し。
    Nothing,
    /// 外部プロセス(棒読みちゃん)へ送信済み。UI への送出は不要。
    Sent,
    /// UI の Web Speech で再生させる。
    WebSpeech(WebSpeechPayload),
    /// UI で再生させる WAV(base64)。`duration` は WAV ヘッダから読めた時のみ。
    Audio {
        wav_base64: String,
        duration: Option<Duration>,
    },
}

/// 発話 1 件ごとの文脈。
#[derive(Debug, Clone, Copy, Default)]
pub struct SpeakContext {
    /// 取り出し後のキュー残件数(自動加速に使う)。
    pub pending: usize,
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
/// `AppHandle` を持たず emit もしない。通知は `take_notices` で取り出す。
pub struct TtsDispatcher {
    config: TtsConfig,
    dictionary: CompiledDictionary,
    bouyomi: bouyomi::BouyomiBackend,
    voicevox: voicevox::VoicevoxClient,
    notices: Vec<TtsNoticePayload>,
    last_bouyomi_notice: Option<Instant>,
    last_voicevox_notice: Option<Instant>,
}

impl TtsDispatcher {
    pub fn new(config: TtsConfig) -> Self {
        let bouyomi = Self::build_bouyomi(&config);
        let voicevox = Self::build_voicevox(&config);
        let dictionary = CompiledDictionary::new(&config.options.dictionary);
        TtsDispatcher {
            dictionary,
            config,
            bouyomi,
            voicevox,
            notices: Vec::new(),
            last_bouyomi_notice: None,
            last_voicevox_notice: None,
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
        if self.config.options.voicevox_url != config.options.voicevox_url {
            self.voicevox = Self::build_voicevox(&config);
        }
        if self.config.options.dictionary != config.options.dictionary {
            self.dictionary = CompiledDictionary::new(&config.options.dictionary);
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

    fn build_voicevox(config: &TtsConfig) -> voicevox::VoicevoxClient {
        voicevox::VoicevoxClient::new(&config.options.voicevox_url)
    }

    /// 溜まった通知(30 秒スロットル済み)を取り出す。
    pub fn take_notices(&mut self) -> Vec<TtsNoticePayload> {
        std::mem::take(&mut self.notices)
    }

    /// 1メッセージを読み上げる。整形 → 優先バックエンド → 失敗時フォールバック。
    /// emit はせず、UI へ送るべき物を返す。
    pub async fn speak(&mut self, msg: &ChatMessage, ctx: SpeakContext) -> SpeechOutput {
        if !should_speak(msg, &self.config.options) {
            return SpeechOutput::Nothing;
        }
        let text = self.format_for_speech(msg);
        if text.trim().is_empty() {
            return SpeechOutput::Nothing;
        }

        // TOCTOU と二重往復を避けるため、speak パスでは available() プローブを
        // 行わず直接合成を試み、Err なら Web Speech へフォールバックする。
        let voice = msg
            .listener
            .as_ref()
            .and_then(|info| info.tts_voice.as_ref());
        match self.config.backend {
            TtsBackendKind::None => SpeechOutput::Nothing,
            TtsBackendKind::WebSpeech => self.webspeech(&text, voice),
            TtsBackendKind::Bouyomi => {
                match self
                    .bouyomi
                    .speak_with_voice(text.clone(), voice.and_then(|v| v.bouyomi_voice))
                    .await
                {
                    Ok(()) => SpeechOutput::Sent,
                    Err(e) => {
                        tracing::warn!("bouyomi 読み上げ失敗→Web Speech へ: {e}");
                        self.push_backend_notice(
                            TtsBackendKind::Bouyomi,
                            format!(
                                "棒読みちゃんで読み上げできないため Web Speech に切り替えました: {e}"
                            ),
                        );
                        self.webspeech(&text, voice)
                    }
                }
            }
            TtsBackendKind::Voicevox => {
                let opt = &self.config.options;
                let factor =
                    params::auto_speed_factor(&opt.auto_speed, ctx.pending, text.chars().count());
                let resolved =
                    params::resolve_voice_params(&opt.voicevox_params, voice, &opt.auto_speed, factor);
                match self
                    .voicevox
                    .synthesize(
                        &text,
                        opt.voicevox_speaker,
                        voice.and_then(|v| v.voicevox_speaker),
                        &resolved,
                    )
                    .await
                {
                    Ok(wav) => SpeechOutput::Audio {
                        duration: wav::wav_duration(&wav),
                        wav_base64: voicevox::base64_encode::encode(&wav),
                    },
                    Err(e) => {
                        tracing::warn!("voicevox 読み上げ失敗→Web Speech へ: {e}");
                        self.push_backend_notice(
                            TtsBackendKind::Voicevox,
                            format!(
                                "VOICEVOXで読み上げできないため Web Speech に切り替えました: {e}"
                            ),
                        );
                        self.webspeech(&text, voice)
                    }
                }
            }
        }
    }

    fn push_backend_notice(&mut self, backend: TtsBackendKind, message: String) {
        let now = Instant::now();
        let last_notice = match backend {
            TtsBackendKind::Bouyomi => &mut self.last_bouyomi_notice,
            TtsBackendKind::Voicevox => &mut self.last_voicevox_notice,
            TtsBackendKind::None | TtsBackendKind::WebSpeech => return,
        };
        if last_notice.is_some_and(|last| now.duration_since(last) <= TTS_NOTICE_THROTTLE) {
            return;
        }
        *last_notice = Some(now);
        self.notices.push(TtsNoticePayload {
            level: "warn",
            message,
        });
    }

    /// Web Speech 用の「読み上げテキスト」(実再生は UI)。
    fn webspeech(&self, text: &str, voice: Option<&TtsVoiceOverride>) -> SpeechOutput {
        SpeechOutput::WebSpeech(WebSpeechPayload::new(text, &self.config.options, voice))
    }

    /// メッセージを読み上げ用テキストへ整形する。
    fn format_for_speech(&self, msg: &ChatMessage) -> String {
        Self::format_with_dictionary(&self.config.options, msg, &self.dictionary)
    }

    #[cfg(test)]
    fn format_for_speech_with_options(opt: &TtsOptions, msg: &ChatMessage) -> String {
        Self::format_with_dictionary(opt, msg, &CompiledDictionary::new(&opt.dictionary))
    }

    fn format_with_dictionary(
        opt: &TtsOptions,
        msg: &ChatMessage,
        dictionary: &CompiledDictionary,
    ) -> String {
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

        body = dictionary.apply(body);

        // 手動 / 抽選 / ウェルカムはテンプレートを通さず `{text}` 固定。
        let mut text = if is_manual_text(msg) {
            body
        } else {
            // 名前には辞書を適用せず、プレースホルダは一度だけ展開する。
            let name = if opt.read_name {
                format_name(msg, &opt.name_format)
            } else {
                String::new()
            };
            let paid_text = msg.amount.as_ref().map_or("", |a| a.raw_text.as_str());
            template::render_template(
                template::select_template(&opt.templates, msg.kind),
                &template::TemplateVars {
                    name: &name,
                    text: &body,
                    paid_text,
                },
            )
        };

        // 長文カット(char 単位)。展開後に行う。
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

/// `tts_speak_text` 経由の手動・抽選・ウェルカム読み上げか。
fn is_manual_text(msg: &ChatMessage) -> bool {
    msg.kind == MessageKind::System && msg.id.starts_with("tts-text-")
}

/// 辞書のコンパイルは生成時と辞書更新時のみ。不正なパターンも再試行しない。
struct CompiledDictionary(Vec<CompiledReplacement>);

enum CompiledReplacement {
    Literal { from: String, to: String },
    Pattern { pattern: Regex, to: String },
}

impl CompiledDictionary {
    fn new(entries: &[TtsDictEntry]) -> Self {
        Self(entries.iter().filter_map(|entry| {
            if entry.from.is_empty() {
                return None;
            }
            if entry.regex {
                match Regex::new(&entry.from) {
                    Ok(pattern) => Some(CompiledReplacement::Pattern { pattern, to: entry.to.clone() }),
                    Err(error) => {
                        tracing::warn!(pattern = %entry.from, %error, "TTS 辞書の不正な正規表現を無視");
                        None
                    }
                }
            } else {
                Some(CompiledReplacement::Literal { from: entry.from.clone(), to: entry.to.clone() })
            }
        }).collect())
    }

    fn apply(&self, mut body: String) -> String {
        for entry in &self.0 {
            body = match entry {
                CompiledReplacement::Literal { from, to } => body.replace(from, to),
                CompiledReplacement::Pattern { pattern, to } => {
                    pattern.replace_all(&body, to.as_str()).into_owned()
                }
            };
        }
        body
    }
}

/// コメントの読み上げ可否。手動テキストはユーザーミュートだけを尊重する。
pub fn should_speak(msg: &ChatMessage, opt: &TtsOptions) -> bool {
    if msg
        .listener
        .as_ref()
        .is_some_and(|listener| listener.tts_muted)
    {
        return false;
    }
    // tts_speak_text 経由の手動・抽選・ウェルカムはコメント用フィルタの対象外。
    // 明示的なユーザーミュートだけは上で尊重する。
    if is_manual_text(msg) {
        return true;
    }
    let enabled = match msg.kind {
        MessageKind::Normal => opt.read_kinds.normal,
        MessageKind::SuperChat => opt.read_kinds.super_chat,
        MessageKind::Membership => opt.read_kinds.membership,
        MessageKind::Bits => opt.read_kinds.bits,
        MessageKind::Gift => opt.read_kinds.gift,
        MessageKind::System => opt.read_kinds.system,
    };
    if !enabled
        || (opt.skip_commands
            && msg.fragments.iter().find_map(|fragment| match fragment {
                Fragment::Text { text } => text.chars().next(),
                Fragment::Emote { name, .. } => name.chars().next(),
            }) == Some('!'))
    {
        return false;
    }
    if opt.exclude_users.is_empty() {
        return true;
    }
    let name = msg.author.name.to_lowercase();
    let id = msg.author.id.to_lowercase();
    let nickname = msg.author.nickname.as_ref().map(|s| s.to_lowercase());
    !opt.exclude_users.iter().any(|pattern| {
        if pattern.trim().is_empty() {
            return false;
        }
        let pattern = pattern.to_lowercase();
        wildcard_matches(&pattern, &name)
            || wildcard_matches(&pattern, &id)
            || nickname
                .as_ref()
                .is_some_and(|nickname| wildcard_matches(&pattern, nickname))
    })
}

/// 全体一致の glob。* 以外はすべてリテラルとして扱う。
fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = value.strip_prefix(first) else {
        return false;
    };
    let mut parts = parts.peekable();
    if parts.peek().is_none() {
        return rest.is_empty();
    }
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return rest.ends_with(part);
        }
        let Some(index) = rest.find(part) else {
            return false;
        };
        rest = &rest[index + part.len()..];
    }
    true
}

fn format_name(msg: &ChatMessage, template: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(index) = rest.find('{') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        let (token, value) = if rest.starts_with("{name}") {
            (
                "{name}",
                msg.author.nickname.as_deref().unwrap_or(&msg.author.name),
            )
        } else if rest.starts_with("{rawName}") {
            ("{rawName}", msg.author.name.as_str())
        } else if rest.starts_with("{label}") {
            (
                "{label}",
                msg.listener
                    .as_ref()
                    .and_then(|listener| listener.label.as_deref())
                    .unwrap_or(""),
            )
        } else {
            out.push('{');
            rest = &rest[1..];
            continue;
        };
        out.push_str(value);
        rest = &rest[token.len()..];
    }
    out.push_str(rest);
    out
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

fn message_body_preview(msg: &ChatMessage) -> String {
    let mut body = String::new();
    for frag in &msg.fragments {
        match frag {
            Fragment::Text { text } => body.push_str(text),
            Fragment::Emote { name, .. } => body.push_str(name),
        }
    }
    if body.trim().is_empty() {
        msg.author.name.clone()
    } else {
        body
    }
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in s.chars().enumerate() {
        if idx >= max_chars {
            out.push_str("...");
            return out;
        }
        out.push(ch);
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
            listener: None,
            native_id: false,
            id: "msg-1".to_string(),
            platform: Platform::Youtube,
            channel: "channel".to_string(),
            author: Author {
                nickname: None,
                avatar_url: None,
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
            injected_test: false,
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
                    regex: false,
                    from: "fast".to_string(),
                    to: "slow".to_string(),
                },
                TtsDictEntry {
                    regex: false,
                    from: "slow comment".to_string(),
                    to: "読み上げ辞書".to_string(),
                },
                TtsDictEntry {
                    regex: false,
                    from: "".to_string(),
                    to: "ignored".to_string(),
                },
                TtsDictEntry {
                    regex: false,
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
    #[test]
    fn listener_voice_selects_webspeech_fields_independently_including_fallback() {
        let opt = TtsOptions {
            web_speech_voice: "global".into(),
            web_speech_rate: 1.1,
            web_speech_pitch: 0.8,
            web_speech_volume: 0.6,
            ..TtsOptions::default()
        };
        // Same builder is used for direct Web Speech and native backend failure fallback.
        for voice in [None, Some(TtsVoiceOverride::default())] {
            let payload = WebSpeechPayload::new("text", &opt, voice.as_ref());
            assert_eq!(
                (
                    payload.voice.as_str(),
                    payload.rate,
                    payload.pitch,
                    payload.volume
                ),
                ("global", 1.1, 0.8, 0.6)
            );
        }
        let mut voice = TtsVoiceOverride {
            web_speech_voice: Some("listener".into()),
            rate: Some(1.5),
            pitch: Some(1.2),
            ..TtsVoiceOverride::default()
        };
        let payload = WebSpeechPayload::new("text", &opt, Some(&voice));
        assert_eq!(
            (
                payload.voice.as_str(),
                payload.rate,
                payload.pitch,
                payload.volume
            ),
            ("listener", 1.5, 1.2, 0.6)
        );
        voice.web_speech_voice = None;
        voice.rate = None;
        let payload = WebSpeechPayload::new("text", &opt, Some(&voice));
        assert_eq!(
            (payload.voice.as_str(), payload.rate, payload.pitch),
            ("global", 1.1, 1.2)
        );
        voice.rate = Some(9.0);
        voice.pitch = Some(-9.0);
        let payload = WebSpeechPayload::new("text", &opt, Some(&voice));
        assert_eq!((payload.rate, payload.pitch), (2.0, 0.0));
    }

    fn listener(label: Option<&str>, tts_muted: bool) -> crate::model::ListenerInfo {
        crate::model::ListenerInfo {
            comment_count: 1,
            gift_count: 0,
            first_seen_ms: 0,
            prev_seen_ms: None,
            is_first_ever: true,
            label: label.map(str::to_string),
            tts_muted,
            tts_voice: None,
        }
    }

    #[test]
    fn tts_name_format_placeholders_and_missing_values() {
        let mut msg = message("表示名", "本文");
        let mut opt = TtsOptions {
            name_format: "{name}/{rawName}/{label}".into(),
            ..TtsOptions::default()
        };
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "表示名/表示名/ 本文"
        );
        msg.author.nickname = Some("ニックネーム".into());
        msg.listener = Some(listener(Some("常連"), false));
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "ニックネーム/表示名/常連 本文"
        );
        msg.listener = Some(listener(None, false));
        opt.name_format = "  {label}  ".into();
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "本文"
        );
        opt.name_format = "{name}".into();
        opt.read_name = false;
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "本文"
        );
        // 名前に含まれるプレースホルダ風文字列を再展開しない。
        opt.read_name = true;
        opt.name_format = "{name} {rawName}".into();
        msg.author.nickname = Some("{rawName}".into());
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "{rawName} 表示名 本文"
        );
    }

    #[test]
    fn tts_excluded_users_match_names_ids_and_nicknames() {
        let mut msg = message("Nightbot", "hello");
        msg.author.nickname = Some("Helper".into());
        for pattern in [
            "nightBOT", "*BOT", "NIGHT*", "*ght*", "author-1", "HELP*", "*",
        ] {
            let opt = TtsOptions {
                exclude_users: vec![pattern.into()],
                ..TtsOptions::default()
            };
            assert!(!should_speak(&msg, &opt), "{pattern}");
        }
        for pattern in [
            "bot", "Night", "*night", "bot*", "Night?ot", "Night.*", "other", "",
        ] {
            let opt = TtsOptions {
                exclude_users: vec![pattern.into()],
                ..TtsOptions::default()
            };
            assert!(should_speak(&msg, &opt), "{pattern}");
        }
        for (pattern, value, expected) in [
            ("a*a", "a", false),
            ("a**a", "aba", true),
            ("*ab*bc", "abbc", true),
            ("*ab*bc", "abc", false),
            ("*猫", "黒猫", true),
            ("", "", true),
        ] {
            assert_eq!(
                wildcard_matches(pattern, value),
                expected,
                "{pattern}: {value}"
            );
        }
    }

    #[test]
    fn tts_blank_exclusion_patterns_are_ignored() {
        let mut msg = message("", "hello");
        msg.author.id.clear();
        for value in ["", " ", "\t", "　"] {
            msg.author.name = value.into();
            msg.author.nickname = Some(value.into());
            let mut opt = TtsOptions {
                exclude_users: vec!["".into(), " ".into(), "\t\n".into(), "　".into()],
                ..TtsOptions::default()
            };
            assert!(should_speak(&msg, &opt), "blank author: {value:?}");
            // 空行が混在していても有効なパターンは適用する。
            opt.exclude_users.push("*".into());
            assert!(!should_speak(&msg, &opt));
        }
    }

    #[test]
    fn tts_manual_text_bypasses_comment_filters_but_respects_mute() {
        let mut msg = message("", "!抽選結果・ウェルカム・手動テキスト");
        msg.id = "tts-text-123456".into();
        msg.channel = "system".into();
        msg.kind = MessageKind::System;
        msg.author.id.clear();
        let mut opt = TtsOptions::default();
        opt.read_kinds.system = false;
        assert!(should_speak(&msg, &opt));
        opt.read_kinds.system = true;
        opt.skip_commands = true;
        assert!(should_speak(&msg, &opt));
        for pattern in ["*", "", " "] {
            opt.skip_commands = false;
            opt.exclude_users = vec![pattern.into()];
            assert!(should_speak(&msg, &opt), "{pattern:?}");
        }
        opt.read_kinds.system = false;
        opt.skip_commands = true;
        opt.exclude_users = vec!["*".into(), "".into(), " ".into()];
        assert!(should_speak(&msg, &opt));
        msg.listener = Some(listener(None, true));
        assert!(!should_speak(&msg, &opt));
        msg.listener = Some(listener(None, false));
        assert!(should_speak(&msg, &opt));

        // 通常の System 通知や通常コメントには引き続きフィルタを適用する。
        msg.id = "system-notice".into();
        assert!(!should_speak(&msg, &opt));
        msg.id = "tts-text-123456".into();
        msg.kind = MessageKind::Normal;
        assert!(!should_speak(&msg, &opt));
    }

    #[test]
    fn tts_manual_bypass_does_not_disable_individual_system_filters() {
        let mut manual = message("", "!welcome");
        manual.author.id.clear();
        manual.id = "tts-text-123".into();
        manual.kind = MessageKind::System;
        let mut notice = manual.clone();
        notice.id = "platform-notice".into();

        let mut kind_filter = TtsOptions::default();
        kind_filter.read_kinds.system = false;
        let user_filter = TtsOptions {
            exclude_users: vec!["*".into()],
            ..TtsOptions::default()
        };
        let command_filter = TtsOptions {
            skip_commands: true,
            ..TtsOptions::default()
        };
        for opt in [kind_filter, user_filter, command_filter] {
            assert!(should_speak(&manual, &opt));
            assert!(!should_speak(&notice, &opt));
            // フィルタを回避しても本文は通常どおり整形する。
            assert_eq!(
                TtsDispatcher::format_for_speech_with_options(&opt, &manual),
                "!welcome"
            );
        }
        // System 種別だけでは手動読み上げとして扱わない。
        notice.fragments = vec![Fragment::text("welcome")];
        assert!(should_speak(&notice, &TtsOptions::default()));
    }

    #[test]
    fn tts_empty_dictionary_sources_never_insert_text() {
        for regex in [false, true] {
            let cache = CompiledDictionary::new(&[TtsDictEntry {
                from: String::new(),
                to: "unexpected".into(),
                regex,
            }]);
            assert!(cache.0.is_empty());
            for body in ["", "本文", "a b"] {
                assert_eq!(cache.apply(body.into()), body);
            }
        }
    }

    #[test]
    fn tts_each_message_kind_can_be_disabled_independently() {
        let kinds = [
            MessageKind::Normal,
            MessageKind::SuperChat,
            MessageKind::Membership,
            MessageKind::Bits,
            MessageKind::Gift,
            MessageKind::System,
        ];
        for (index, kind) in kinds.iter().enumerate() {
            let mut opt = TtsOptions::default();
            let mut msg = message("name", "hello");
            msg.kind = *kind;
            assert!(should_speak(&msg, &opt));
            match index {
                0 => opt.read_kinds.normal = false,
                1 => opt.read_kinds.super_chat = false,
                2 => opt.read_kinds.membership = false,
                3 => opt.read_kinds.bits = false,
                4 => opt.read_kinds.gift = false,
                _ => opt.read_kinds.system = false,
            }
            for candidate in kinds {
                msg.kind = candidate;
                assert_eq!(should_speak(&msg, &opt), candidate != *kind);
            }
        }
    }

    #[test]
    fn tts_command_filter_and_listener_mute() {
        let mut opt = TtsOptions::default();
        assert!(should_speak(&message("name", "!help"), &opt));
        opt.skip_commands = true;
        assert!(!should_speak(&message("name", "!help"), &opt));
        for body in ["hello!", " !help", "", "！help"] {
            assert!(should_speak(&message("!author", body), &opt));
        }
        let mut msg = message("name", "!help");
        msg.fragments.insert(
            0,
            Fragment::Text {
                text: String::new(),
            },
        );
        assert!(!should_speak(&msg, &opt));
        msg = message("name", "hello");
        msg.listener = Some(listener(None, true));
        assert!(!should_speak(&msg, &opt));
        msg.listener = Some(listener(None, false));
        assert!(should_speak(&msg, &opt));
    }

    #[test]
    fn tts_compiled_dictionary_captures_invalid_patterns_and_literals() {
        let opt = TtsOptions {
            name_format: "{name}/{rawName}/{label}".into(),
            dictionary: vec![
                TtsDictEntry {
                    from: r"item-(\d+)".into(),
                    to: "番号$1".into(),
                    regex: true,
                },
                TtsDictEntry {
                    from: "[".into(),
                    to: "bad".into(),
                    regex: true,
                },
                TtsDictEntry {
                    from: ".".into(),
                    to: "$1".into(),
                    regex: false,
                },
                TtsDictEntry {
                    from: "".into(),
                    to: "ignored".into(),
                    regex: false,
                },
                TtsDictEntry {
                    from: "".into(),
                    to: "ignored-regex".into(),
                    regex: true,
                },
            ],
            ..TtsOptions::default()
        };
        let cache = CompiledDictionary::new(&opt.dictionary);
        assert_eq!(cache.0.len(), 2);
        let mut msg = message("item-1", "item-12 item-34.[");
        msg.author.nickname = Some("item-2".into());
        msg.listener = Some(listener(Some("item-3"), false));
        for _ in 0..2 {
            assert_eq!(
                TtsDispatcher::format_with_dictionary(&opt, &msg, &cache),
                "item-2/item-1/item-3 番号12 番号34$1["
            );
        }
    }

    #[test]
    fn tts_legacy_config_preserves_speech_and_defaults() {
        let cfg: crate::config::AppConfig = serde_json::from_str(
            r#"{
            "tts": {"options": {"dictionary": [{"from":"fast", "to":"ファスト"}]}}
        }"#,
        )
        .unwrap();
        let opt = &cfg.tts.options;
        assert_eq!(opt.name_format, "{name}");
        assert!(opt.exclude_users.is_empty());
        assert_eq!(opt.read_kinds, crate::config::TtsReadKinds::default());
        assert!(!opt.skip_commands);
        assert!(!opt.dictionary[0].regex);
        let msg = message("fast", "fast https://example.com");
        assert!(should_speak(&msg, opt));
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(opt, &msg),
            "fast ファスト URL省略"
        );
    }

    fn superchat(author_name: &str, body: &str, raw_text: &str) -> ChatMessage {
        let mut msg = message(author_name, body);
        msg.kind = MessageKind::SuperChat;
        msg.amount = Some(crate::model::Amount {
            value: 500.0,
            currency: "JPY".into(),
            raw_text: raw_text.into(),
        });
        msg
    }

    #[test]
    fn superchat_template_reads_paid_text() {
        let mut opt = TtsOptions::default();
        opt.templates.super_chat = Some("{name}さんから{paidText}、{text}".into());
        let msg = superchat("太郎", "ありがとう", "¥500");
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "太郎さんから¥500、ありがとう"
        );
        // 金額が無ければ空で展開する。通常コメントは all を使う。
        let mut no_amount = msg.clone();
        no_amount.amount = None;
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &no_amount),
            "太郎さんから、ありがとう"
        );
        opt.templates.all = "{text}({paidText})".into();
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &message("太郎", "やあ")),
            "やあ()"
        );
    }

    #[tokio::test]
    async fn empty_kind_template_is_not_spoken() {
        let mut config = TtsConfig {
            backend: TtsBackendKind::WebSpeech,
            ..TtsConfig::default()
        };
        config.options.templates.super_chat = Some(String::new());
        let mut dispatcher = TtsDispatcher::new(config);
        let msg = superchat("太郎", "ありがとう", "¥500");
        assert_eq!(
            dispatcher.speak(&msg, SpeakContext::default()).await,
            SpeechOutput::Nothing
        );
        // 他の種別は引き続き all テンプレートで読む。
        match dispatcher
            .speak(&message("太郎", "こんにちは"), SpeakContext::default())
            .await
        {
            SpeechOutput::WebSpeech(p) => assert_eq!(p.text, "太郎 こんにちは"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn manual_text_ignores_templates() {
        let mut msg = message("名前", "手動テキスト");
        msg.id = "tts-text-1".into();
        msg.kind = MessageKind::System;
        let mut opt = TtsOptions::default();
        opt.templates.all = "全体 {name} {text}".into();
        opt.templates.system = Some(String::new());
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "手動テキスト"
        );
        // 手動以外の System はテンプレートを通す。
        msg.id = "system-notice".into();
        assert_eq!(TtsDispatcher::format_for_speech_with_options(&opt, &msg), "");
        opt.templates.system = None;
        assert_eq!(
            TtsDispatcher::format_for_speech_with_options(&opt, &msg),
            "全体 名前 手動テキスト"
        );
    }

    #[tokio::test]
    async fn voicevox_failure_returns_webspeech_output_and_notice() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut config = TtsConfig {
            backend: TtsBackendKind::Voicevox,
            ..TtsConfig::default()
        };
        config.options.voicevox_url = format!("http://127.0.0.1:{port}");
        config.options.web_speech_voice = "global".into();
        let mut dispatcher = TtsDispatcher::new(config);
        let msg = message("太郎", "こんにちは");
        match dispatcher.speak(&msg, SpeakContext::default()).await {
            SpeechOutput::WebSpeech(p) => {
                assert_eq!(p.text, "太郎 こんにちは");
                assert_eq!(p.voice, "global");
            }
            other => panic!("unexpected {other:?}"),
        }
        let notices = dispatcher.take_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].level, "warn");
        assert!(notices[0].message.contains("VOICEVOX"));
        assert!(dispatcher.take_notices().is_empty());
        // 30 秒以内の再失敗は通知しない(フォールバックはする)。
        assert!(matches!(
            dispatcher.speak(&msg, SpeakContext::default()).await,
            SpeechOutput::WebSpeech(_)
        ));
        assert!(dispatcher.take_notices().is_empty());
    }

    #[tokio::test]
    async fn listener_rate_reaches_fake_engine() {
        let fake = voicevox::test_support::spawn_fake_engine().await;
        let mut config = TtsConfig {
            backend: TtsBackendKind::Voicevox,
            ..TtsConfig::default()
        };
        config.options.voicevox_url = fake.url.clone();
        config.options.voicevox_speaker = 3;
        let mut dispatcher = TtsDispatcher::new(config);
        let mut msg = message("太郎", "こんにちは");
        let mut info = listener(None, false);
        info.tts_voice = Some(TtsVoiceOverride {
            voicevox_speaker: Some(8),
            rate: Some(1.5),
            pitch: Some(1.5),
            ..TtsVoiceOverride::default()
        });
        msg.listener = Some(info);
        match dispatcher.speak(&msg, SpeakContext::default()).await {
            SpeechOutput::Audio { wav_base64, .. } => {
                assert_eq!(wav_base64, voicevox::base64_encode::encode(b"RIFFfake"))
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(dispatcher.take_notices().is_empty());
        let g = fake.state.lock().unwrap();
        assert_eq!(
            g.queries,
            vec![("太郎 こんにちは".to_string(), "8".to_string())]
        );
        let (speaker, body) = &g.syntheses[0];
        assert_eq!(speaker, "8");
        assert!((body["speedScale"].as_f64().unwrap() - 1.5).abs() < 1e-6);
        assert!((body["pitchScale"].as_f64().unwrap() - 0.075).abs() < 1e-6);
        // 未設定のパラメータはエンジン値のまま。
        assert_eq!(body["intonationScale"], serde_json::json!(1.0));
    }
}
