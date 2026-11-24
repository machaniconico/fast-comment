//! アプリ設定の構造体と永続化。
//!
//! 保存先は Tauri の app config dir 配下 `config.json`。
//! JSON は camelCase。起動時にロードし、変更のたびに保存する。
//! 設定は「重い処理は Rust 側」原則のもと、Source/TTS/Bus/Moderation の
//! 全層から参照される単一の設定ソースとなる。

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChannelTtsVoice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voicevox_speaker: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bouyomi_voice: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_speech_voice: Option<String>,
}

impl ChannelTtsVoice {
    pub fn is_empty(&self) -> bool {
        self.voicevox_speaker.is_none()
            && self.bouyomi_voice.is_none()
            && self.web_speech_voice.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ChannelOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tts_voice: Option<ChannelTtsVoice>,
    /// 効果音。`false` だけを保存する(`true` は上書き無しと同じ)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_enabled: Option<bool>,
}

impl ChannelOverrides {
    pub fn is_empty(&self) -> bool {
        self.tts_enabled.is_none()
            && self.sound_enabled.is_none()
            && self.tts_voice.as_ref().is_none_or(ChannelTtsVoice::is_empty)
    }

    pub fn normalize(&mut self) {
        if self.tts_enabled == Some(true) {
            self.tts_enabled = None;
        }
        if self.sound_enabled == Some(true) {
            self.sound_enabled = None;
        }
        if let Some(voice) = &mut self.tts_voice {
            voice.web_speech_voice = voice
                .web_speech_voice
                .as_deref()
                .map(|name| name.trim().chars().take(MAX_WEB_SPEECH_VOICE_CHARS).collect::<String>())
                .filter(|name| !name.chars().all(|c| c.is_whitespace() || is_invisible_format(c)));
            if voice.is_empty() {
                self.tts_voice = None;
            }
        }
    }
}

const MAX_WEB_SPEECH_VOICE_CHARS: usize = 200;

/// ゼロ幅・書式制御文字(Unicode Cf の主なもの)。これだけの声名は空とみなす。
fn is_invisible_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
    )
}

fn deserialize_channel_overrides<'de, D>(deserializer: D) -> Result<ChannelOverrides, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OverridesVisitor;

    impl<'de> serde::de::Visitor<'de> for OverridesVisitor {
        type Value = ChannelOverrides;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a channel overrides object")
        }

        fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            ChannelOverrides::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }

        /// `"overrides": null` は上書き無しとして読む(手編集で書かれうる)。
        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(ChannelOverrides::default())
        }
    }

    deserializer.deserialize_any(OverridesVisitor)
}

/// 監視対象チャンネル1件。
///
/// Twitch はチャンネル名(`#` 抜き)、YouTube は videoId か配信URL を `identifier` に入れる。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelConfig {
    /// 接続元プラットフォーム。
    pub platform: ChannelPlatform,
    /// Twitch: チャンネル名 / YouTube: videoId or 配信URL。
    pub identifier: String,
    /// この行を有効にするか(false なら接続しない)。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 「自分の配信」の印(X 自動告知の対象)。旧 config.json には無いので既定は false。
    #[serde(default)]
    pub own_stream: bool,
    #[serde(
        default,
        skip_serializing_if = "ChannelOverrides::is_empty",
        deserialize_with = "deserialize_channel_overrides"
    )]
    pub overrides: ChannelOverrides,
}

/// 設定上のプラットフォーム種別。`model::Platform` と1対1だが、
/// 設定ファイル独立のため別定義(将来 niconico 等の追加に備える)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelPlatform {
    Twitch,
    Youtube,
    /// X (Twitter) ライブ配信。identifier は broadcast URL または broadcast ID。
    X,
    /// ニコニコ生放送。identifier は番組URL または lv 番組ID。
    Niconico,
    /// ツイキャス。identifier は配信者 ID(screen_id)または URL。
    Twitcasting,
}

impl ChannelPlatform {
    fn key_prefix(self) -> &'static str {
        match self {
            Self::Twitch => "twitch",
            Self::Youtube => "youtube",
            Self::X => "x",
            Self::Niconico => "niconico",
            Self::Twitcasting => "twitcasting",
        }
    }
}

pub fn channel_key(ch: &ChannelConfig) -> String {
    format!("{}:{}", ch.platform.key_prefix(), ch.identifier)
}

fn channel_matches_key(ch: &ChannelConfig, key: &str) -> bool {
    key.split_once(':') == Some((ch.platform.key_prefix(), ch.identifier.as_str()))
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChannelEditError {
    NotFound,
    Duplicate,
}

impl std::fmt::Display for ChannelEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("チャンネルが見つかりません"),
            Self::Duplicate => f.write_str("チャンネルは既に存在します"),
        }
    }
}

pub fn set_channel_overrides_in(
    cfg: &mut AppConfig,
    key: &str,
    mut overrides: ChannelOverrides,
) -> Result<(), ChannelEditError> {
    let row = cfg
        .channels
        .iter_mut()
        .find(|ch| channel_matches_key(ch, key))
        .ok_or(ChannelEditError::NotFound)?;
    overrides.normalize();
    row.overrides = overrides;
    Ok(())
}

pub fn replace_channel_in(
    cfg: &mut AppConfig,
    old_key: &str,
    new: ChannelConfig,
) -> Result<(), ChannelEditError> {
    // 1 件指定の置換は旧行が無ければ NotFound(受け入れ契約)。複数指定の IPC 経路だけ寛容に扱う。
    if !cfg.channels.iter().any(|ch| channel_matches_key(ch, old_key)) {
        return Err(ChannelEditError::NotFound);
    }
    replace_channels_in(cfg, &[old_key.to_string()], new)
}

/// 旧 key に一致する行をすべて外し(同じ key の行が複数あれば全部)、新しい行を一番前の旧行の位置に入れる
/// (1 回の保存で済ませるため)。新しい行の上書きが空ならその旧行のものを引き継ぐ。
/// もう無い旧 key は無視する(別画面で先に消えていても追加は止めない)。旧行が 1 つも無ければ
/// 引き継ぐ上書きは無く、通常の追加と同じく末尾に足す。
pub fn replace_channels_in(
    cfg: &mut AppConfig,
    old_keys: &[String],
    mut new: ChannelConfig,
) -> Result<(), ChannelEditError> {
    let removed = |ch: &ChannelConfig| old_keys.iter().any(|key| channel_matches_key(ch, key));
    if cfg
        .channels
        .iter()
        .any(|ch| !removed(ch) && ch.platform == new.platform && ch.identifier == new.identifier)
    {
        return Err(ChannelEditError::Duplicate);
    }
    new.overrides.normalize();
    let Some(first) = cfg.channels.iter().position(removed) else {
        cfg.channels.push(new);
        return Ok(());
    };
    if new.overrides.is_empty() {
        new.overrides = cfg.channels[first].overrides.clone();
    }
    let mut i = 0;
    cfg.channels.retain(|ch| {
        let keep = i == first || !removed(ch);
        i += 1;
        keep
    });
    cfg.channels[first] = new;
    Ok(())
}

/// 元ファイルを読み切ってから、空いている連番の `.broken-*` を排他的に作成する。
/// 書き込みや同期に失敗したら、ファイルを閉じてから作成した退避先を削除する。
fn preserve_broken_config(path: &Path, now_ms: u128) -> std::io::Result<PathBuf> {
    let bytes = std::fs::read(path)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for n in 0u64.. {
        let suffix = if n == 0 { String::new() } else { format!("-{n}") };
        let backup = path.with_file_name(format!("{name}.broken-{now_ms}{suffix}"));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&backup) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        let result = file.write_all(&bytes).and_then(|()| file.sync_all());
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(&backup);
        }
        return result.map(|()| backup);
    }
    Err(ErrorKind::AlreadyExists.into())
}

/// Windows の共有違反など一時的な読込失敗を壊れた扱いにしないための再試行間隔。
const READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// 読込に失敗したら `delay` 待って 1 回だけ読み直す(解析失敗は対象外)。
fn read_with_retry<T>(
    path: &Path,
    delay: std::time::Duration,
    mut read: impl FnMut(&Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    read(path).or_else(|_| {
        std::thread::sleep(delay);
        read(path)
    })
}

fn read_config_text(
    path: &Path,
    delay: std::time::Duration,
    mut exists: impl FnMut(&Path) -> std::io::Result<bool>,
    mut read: impl FnMut(&Path) -> std::io::Result<String>,
) -> std::io::Result<Option<String>> {
    let mut found = false;
    read_with_retry(path, delay, |path| {
        // 存在確認済みなら、読込に失敗しても初回起動扱いにはしない。
        if !found {
            found = exists(path)?;
        }
        if found {
            read(path).map(Some)
        } else {
            Ok(None)
        }
    })
}

/// 残す `config.json.broken-*` の数。
const MAX_BROKEN_BACKUPS: usize = 5;

/// `<name>.broken-<ms>` / `<name>.broken-<ms>-<n>` の (ms, n)。それ以外の名前は None(消さない)。
fn broken_backup_order(file_name: &str, name: &str) -> Option<(u128, u64)> {
    let rest = file_name.strip_prefix(name)?.strip_prefix(".broken-")?;
    let (ms, n) = match rest.split_once('-') {
        Some((ms, n)) if !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()) => {
            (ms, n.parse().ok()?)
        }
        Some(_) => return None,
        None => (rest, 0),
    };
    if (ms.len() > 1 && ms.starts_with('0')) || !ms.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((ms.parse().ok()?, n))
}

/// 新しい退避を作れた後に呼ぶ。新しい順に `MAX_BROKEN_BACKUPS` 件(作ったばかりの物を含む)だけ残す。
fn prune_broken_backups(path: &Path, keep: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let name = name.to_string_lossy();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut backups: Vec<((u128, u64), PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let order = broken_backup_order(&e.file_name().to_string_lossy(), &name)?;
            Some((order, e.path()))
        })
        .filter(|(_, p)| p != keep)
        .collect();
    backups.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in backups.into_iter().skip(MAX_BROKEN_BACKUPS - 1) {
        if let Err(e) = std::fs::remove_file(&old) {
            tracing::warn!("古い設定退避の削除失敗: {}", e.kind());
        }
    }
}

/// 起動時ロードの結果。利用者への通知に使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLoadOutcome {
    /// 読めた(または初回起動)。
    Loaded,
    /// 読めなかったので控えを残し既定値で起動した。
    BackedUp,
    /// 読めず控えも作れなかったので保存を止めた。
    SavesBlocked,
}

impl ConfigLoadOutcome {
    /// 利用者に出す固定文。パスや内容は含めない。
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Loaded => None,
            Self::BackedUp => Some("設定ファイルが読めなかったため控えを残して初期設定で起動しました"),
            Self::SavesBlocked => Some("設定ファイルを読めず控えも作れなかったため、保存を止めています"),
        }
    }
}

/// 退避できなかった設定ディレクトリ。元の config.json を既定値で上書きしないよう保存を止める。
static SAVE_BLOCKED_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

pub const CONFIG_SAVE_BLOCKED_ERROR: &str =
    "設定ファイルを読めず退避もできなかったため、上書きしないよう保存を止めています。config.json を確認してアプリを再起動してください";

/// 保存停止中の `save` が返すエラー。表示は固定文。
#[derive(Debug)]
pub struct ConfigSaveBlocked;

impl std::fmt::Display for ConfigSaveBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(CONFIG_SAVE_BLOCKED_ERROR)
    }
}

impl std::error::Error for ConfigSaveBlocked {}

fn save_blocked(dir: &Path) -> bool {
    SAVE_BLOCKED_DIRS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|d| d == dir)
}

const MAX_OBS_ROWS: u16 = 1000;

/// OBS overlay サーバ設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsConfig {
    /// axum サーバの待受ポート。既定 11180。
    #[serde(default = "default_obs_port")]
    pub port: u16,
    /// OBS オーバーレイで使うテンプレート名。既定 "default"。
    /// SPEC §10 に従い config を唯一の永続化先とする(localStorage 非使用)。
    #[serde(default = "default_obs_template")]
    pub template: String,
    /// OBS オーバーレイのフォント倍率(%)。既定 100。
    #[serde(default = "default_obs_font_scale_pct")]
    pub font_scale_pct: u16,
    /// OBS弾幕オーバーレイの文字サイズ(px)。既定 30。
    #[serde(default = "default_obs_danmaku_font_size")]
    pub danmaku_font_size: u16,
    /// OBS オーバーレイの最大表示行数。既定 8。
    #[serde(default = "default_obs_max_rows")]
    pub max_rows: u16,
    /// OBS オーバーレイの表示時間(ms)。既定 12000。
    #[serde(default = "default_obs_ttl_ms")]
    pub ttl_ms: u32,
    /// OBS オーバーレイ全体背景の不透明度(%)。既定 0。
    #[serde(default = "default_obs_bg_opacity_pct")]
    pub bg_opacity_pct: u16,
    /// OBS オーバーレイの表示位置("top" / "bottom")。既定 "bottom"。
    #[serde(default = "default_obs_position")]
    pub position: String,
    /// プラットフォーム表示を出すか。既定 true。
    #[serde(default = "default_true")]
    pub show_platform: bool,
}

impl ObsConfig {
    pub fn normalize(&mut self) {
        self.max_rows = self.max_rows.clamp(1, MAX_OBS_ROWS);
        self.font_scale_pct = self.font_scale_pct.clamp(50, 200);
        self.danmaku_font_size = self.danmaku_font_size.clamp(12, 96);
        self.bg_opacity_pct = self.bg_opacity_pct.clamp(0, 100);
        self.ttl_ms = self.ttl_ms.clamp(500, 600_000);
        if self.position != "top" && self.position != "bottom" {
            self.position = "bottom".to_string();
        }
    }
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            port: default_obs_port(),
            template: default_obs_template(),
            font_scale_pct: default_obs_font_scale_pct(),
            danmaku_font_size: default_obs_danmaku_font_size(),
            max_rows: default_obs_max_rows(),
            ttl_ms: default_obs_ttl_ms(),
            bg_opacity_pct: default_obs_bg_opacity_pct(),
            position: default_obs_position(),
            show_platform: true,
        }
    }
}

/// OBS 配信目標ゲージ(Goals overlay)設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalsConfig {
    /// Goals overlay を有効にするか。false のときは全ゲージを非表示扱いにする。
    pub enabled: bool,
    /// アプリ本体内にも GoalsBar を常設表示するか。既定 false。
    #[serde(default)]
    pub show_in_app: bool,
    /// OBS上の配置("horizontal" / "vertical" / "grid")。
    #[serde(default = "default_goals_layout")]
    pub layout: String,
    /// OBS上の外観("glass" / "solid" / "minimal")。
    #[serde(default = "default_goals_skin")]
    pub skin: String,
    /// 各ゲージの表示可否。None は旧設定との互換用で、目標値が1以上なら表示。
    #[serde(default)]
    pub show_comments: Option<bool>,
    #[serde(default)]
    pub show_viewers: Option<bool>,
    #[serde(default)]
    pub show_likes: Option<bool>,
    #[serde(default)]
    pub show_reactions: Option<bool>,
    /// コメント数目標。0 は 0% として表示。
    pub comments: u32,
    /// 視聴者数目標。0 は 0% として表示。
    pub viewers: u32,
    /// 高評価数目標。0 は 0% として表示。
    pub likes: u32,
    /// YouTube 絵文字リアクション数目標。0 は 0% として表示。
    #[serde(default)]
    pub reactions: u32,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        GoalsConfig {
            enabled: false,
            show_in_app: false,
            layout: default_goals_layout(),
            skin: default_goals_skin(),
            show_comments: Some(true),
            show_viewers: Some(true),
            show_likes: Some(true),
            show_reactions: Some(true),
            comments: 0,
            viewers: 0,
            likes: 0,
            reactions: 0,
        }
    }
}

/// OBS タイマー/カウントダウン overlay 設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerConfig {
    /// Timer overlay を有効にするか。既定 false。
    #[serde(default)]
    pub enabled: bool,
    /// 開始時の既定時間(秒)。既定 300 秒。
    #[serde(default = "default_timer_duration_sec")]
    pub default_duration_sec: u32,
    /// 表示モード("countdown" / "elapsed")。既定 "countdown"。
    #[serde(default = "default_timer_mode")]
    pub mode: String,
}

impl Default for TimerConfig {
    fn default() -> Self {
        TimerConfig {
            enabled: false,
            default_duration_sec: default_timer_duration_sec(),
            mode: default_timer_mode(),
        }
    }
}

/// キーワードエフェクトの表示先。未知の値は既存の挙動(アプリ内のみ)に倒す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EffectTarget {
    Obs,
    Both,
    #[default]
    #[serde(other)]
    App,
}

impl EffectTarget {
    pub fn includes_app(self) -> bool {
        matches!(self, EffectTarget::App | EffectTarget::Both)
    }

    pub fn includes_obs(self) -> bool {
        matches!(self, EffectTarget::Obs | EffectTarget::Both)
    }
}

/// OBS テンプレート `effects` が持つアニメーションのプリセット名。
pub const EFFECT_ANIMATIONS: [&str; 6] = ["rain", "burst", "float", "bounce", "spin", "big"];
pub const EFFECT_MIN_COUNT: u32 = 1;
pub const EFFECT_MAX_COUNT: u32 = 50;
pub const EFFECT_MIN_DURATION_MS: u32 = 500;
pub const EFFECT_MAX_DURATION_MS: u32 = 15_000;
pub const EFFECT_MAX_COOLDOWN_MS: u32 = 60_000;
const EFFECT_MAX_EMOJI_CHARS: usize = 16;

/// コメント本文に反応してエフェクトを表示するルール(アプリ内・OBS)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "EffectRuleRaw")]
pub struct EffectRule {
    /// 規則ごとの有効/無効。既定 true。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 大文字小文字を区別せずに本文(テキスト部分のみ)へ部分一致するキーワード。
    #[serde(default)]
    pub keyword: String,
    /// 表示する文字列。絵文字以外の短い文字も許可する。
    #[serde(default)]
    pub emoji: String,
    /// 1回の一致で生成するパーティクル数(1〜50)。
    #[serde(default = "default_effect_count")]
    pub count: u32,
    /// 表示先。既定 `app`(従来の挙動)。
    #[serde(default)]
    pub target: EffectTarget,
    /// OBS 用のプリセット名。未知の値は既定へ丸める。
    #[serde(default = "default_effect_animation")]
    pub animation: String,
    /// OBS での表示時間(500〜15000ms)。
    #[serde(default = "default_effect_duration_ms")]
    pub duration_ms: u32,
    /// 規則ごとの最短発火間隔(0〜60000ms)。
    #[serde(default = "default_effect_cooldown_ms")]
    pub cooldown_ms: u32,
}

/// 読み込み用。旧 config(`enabled` 無し)の `count=0` は「無効」を意味していたので、
/// 無効の規則として count=1 で読む。キーワードは旧アプリと同じく trim する。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EffectRuleRaw {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    keyword: String,
    #[serde(default)]
    emoji: String,
    #[serde(default = "default_effect_count")]
    count: u32,
    #[serde(default)]
    target: EffectTarget,
    #[serde(default = "default_effect_animation")]
    animation: String,
    #[serde(default = "default_effect_duration_ms")]
    duration_ms: u32,
    #[serde(default = "default_effect_cooldown_ms")]
    cooldown_ms: u32,
}

impl From<EffectRuleRaw> for EffectRule {
    fn from(raw: EffectRuleRaw) -> Self {
        EffectRule {
            enabled: raw.enabled.unwrap_or(raw.count != 0),
            keyword: raw.keyword.trim().to_string(),
            emoji: raw.emoji,
            count: raw.count.max(EFFECT_MIN_COUNT),
            target: raw.target,
            animation: raw.animation,
            duration_ms: raw.duration_ms,
            cooldown_ms: raw.cooldown_ms,
        }
    }
}

impl Default for EffectRule {
    fn default() -> Self {
        EffectRule {
            enabled: true,
            keyword: String::new(),
            emoji: String::new(),
            count: default_effect_count(),
            target: EffectTarget::default(),
            animation: default_effect_animation(),
            duration_ms: default_effect_duration_ms(),
            cooldown_ms: default_effect_cooldown_ms(),
        }
    }
}

impl EffectRule {
    /// 範囲外値の丸め。
    pub fn normalize(&mut self) {
        let trimmed = self.keyword.trim();
        if trimmed.len() != self.keyword.len() {
            self.keyword = trimmed.to_string();
        }
        self.count = self.count.clamp(EFFECT_MIN_COUNT, EFFECT_MAX_COUNT);
        self.duration_ms = self
            .duration_ms
            .clamp(EFFECT_MIN_DURATION_MS, EFFECT_MAX_DURATION_MS);
        self.cooldown_ms = self.cooldown_ms.min(EFFECT_MAX_COOLDOWN_MS);
        if !EFFECT_ANIMATIONS.contains(&self.animation.as_str()) {
            self.animation = default_effect_animation();
        }
        if self.emoji.chars().count() > EFFECT_MAX_EMOJI_CHARS {
            self.emoji = self.emoji.chars().take(EFFECT_MAX_EMOJI_CHARS).collect();
        }
    }
}

/// コメント連動エフェクト設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EffectsConfig {
    /// アプリ内コメント連動エフェクトを有効にするか。既定 false。
    #[serde(default)]
    pub enabled: bool,
    /// キーワード一致ルール。
    #[serde(default)]
    pub rules: Vec<EffectRule>,
}

impl EffectsConfig {
    pub fn normalize(&mut self) {
        for rule in &mut self.rules {
            rule.normalize();
        }
    }
}

/// セッション初コメント視聴者への歓迎演出設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WelcomeConfig {
    /// 初見歓迎演出を有効にするか。既定 false。
    #[serde(default)]
    pub enabled: bool,
    /// 挨拶テンプレート。`{name}` を視聴者名に置換する。
    #[serde(default = "default_welcome_greeting")]
    pub greeting: String,
    /// 挨拶を TTS で読み上げるか。既定 false。
    #[serde(default)]
    pub tts: bool,
    /// パーティクルに使う文字列。既定 "👋"。
    #[serde(default = "default_welcome_emoji")]
    pub emoji: String,
    /// 生成するパーティクル数。既定 8。
    #[serde(default = "default_welcome_count")]
    pub count: u32,
}

impl Default for WelcomeConfig {
    fn default() -> Self {
        WelcomeConfig {
            enabled: false,
            greeting: default_welcome_greeting(),
            tts: false,
            emoji: default_welcome_emoji(),
            count: default_welcome_count(),
        }
    }
}

/// TTS(読み上げ)設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsConfig {
    /// 優先バックエンド。`available()==false` なら Web Speech へフォールバック。
    #[serde(default)]
    pub backend: TtsBackendKind,
    /// バックエンド別の細かな調整値。
    #[serde(default)]
    pub options: TtsOptions,
}

impl Default for TtsConfig {
    fn default() -> Self {
        TtsConfig {
            backend: TtsBackendKind::default(),
            options: TtsOptions::default(),
        }
    }
}

/// 読み上げバックエンドの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TtsBackendKind {
    /// 棒読みちゃん(TCP 127.0.0.1:50001)。
    Bouyomi,
    /// VOICEVOX(HTTP 127.0.0.1:50021)。
    Voicevox,
    /// ブラウザ `speechSynthesis`(UI 側で再生)。既定。
    #[default]
    WebSpeech,
    /// 読み上げ無効。
    None,
}

/// 読み上げ前に本文へ適用するユーザー定義置換の1件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsDictEntry {
    /// 置換前の文字列または正規表現。平文の空文字列はスキップする。
    #[serde(default)]
    pub from: String,
    /// 置換後の文字列。regex=true の場合はキャプチャ参照を展開する。
    #[serde(default)]
    pub to: String,
    /// 正規表現置換を使う(置換先では $1 等のキャプチャ参照が可能)。
    #[serde(default)]
    pub regex: bool,
}

/// 読み上げ対象のメッセージ種別。欠落した項目も従来どおり読む。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsReadKinds {
    pub normal: bool,
    pub super_chat: bool,
    pub membership: bool,
    pub bits: bool,
    pub gift: bool,
    pub system: bool,
}

impl Default for TtsReadKinds {
    fn default() -> Self {
        Self {
            normal: true,
            super_chat: true,
            membership: true,
            bits: true,
            gift: true,
            system: true,
        }
    }
}

fn default_tts_name_format() -> String {
    "{name}".to_string()
}

/// 読み上げの整形/エンジン調整パラメータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TtsOptions {
    /// 棒読み: 接続先ホスト。
    #[serde(default = "default_bouyomi_host")]
    pub bouyomi_host: String,
    /// 棒読み: 接続先ポート。既定 50001。
    #[serde(default = "default_bouyomi_port")]
    pub bouyomi_port: u16,
    /// 棒読み: 速度(-1=デフォルト)。
    #[serde(default = "default_minus_one")]
    pub bouyomi_speed: i16,
    /// 棒読み: 音量(-1=デフォルト)。
    #[serde(default = "default_minus_one")]
    pub bouyomi_volume: i16,
    /// 棒読み: 音程(-1=デフォルト)。
    #[serde(default = "default_minus_one")]
    pub bouyomi_tone: i16,
    /// 棒読み: 声質(0=デフォルト)。
    #[serde(default)]
    pub bouyomi_voice: i16,
    /// 棒読み: 自動起動する実行ファイルパス(空なら手動起動)。
    #[serde(default)]
    pub bouyomi_path: String,
    /// 棒読み: 自動起動時に UAC 昇格を要求するか。既定 false。
    #[serde(default)]
    pub bouyomi_launch_elevated: bool,

    /// VOICEVOX: ベースURL。
    #[serde(default = "default_voicevox_url")]
    pub voicevox_url: String,
    /// VOICEVOX: 話者(speaker)ID。
    #[serde(default = "default_voicevox_speaker")]
    pub voicevox_speaker: u32,

    /// Web Speech: 読み上げ速度。
    #[serde(default = "default_web_speech_rate")]
    pub web_speech_rate: f32,
    /// Web Speech: 音程。
    #[serde(default = "default_web_speech_pitch")]
    pub web_speech_pitch: f32,
    /// Web Speech: 音量。
    #[serde(default = "default_web_speech_volume")]
    pub web_speech_volume: f32,
    /// Web Speech: 音声名(空ならブラウザ既定)。
    #[serde(default = "default_web_speech_voice")]
    pub web_speech_voice: String,

    /// 名前を読み上げるか。
    #[serde(default = "default_true")]
    pub read_name: bool,
    /// 名前部の書式: {name} / {rawName} / {label}。
    #[serde(default = "default_tts_name_format")]
    pub name_format: String,
    /// 表示名・ID・ニックネームの全体一致。* のみワイルドカード。
    #[serde(default)]
    pub exclude_users: Vec<String>,
    #[serde(default)]
    pub read_kinds: TtsReadKinds,
    /// ! で始まる本文を読み上げない。
    #[serde(default)]
    pub skip_commands: bool,
    /// URL を「URL省略」等に置換するか。
    #[serde(default = "default_true")]
    pub omit_url: bool,
    /// 絵文字/エモートを読み上げから除去するか。
    #[serde(default = "default_true")]
    pub strip_emoji: bool,
    /// 1メッセージあたりの最大読み上げ文字数(超過分はカット)。
    #[serde(default = "default_max_read_len")]
    pub max_length: usize,
    /// 読み上げ本文に適用するユーザー定義の文字列/正規表現置換辞書。
    #[serde(default)]
    pub dictionary: Vec<TtsDictEntry>,

    /// VOICEVOX 互換エンジンの種別(プリセット URL の切替に使う)。
    #[serde(default)]
    pub voicevox_engine: VoicevoxEngineKind,
    /// VOICEVOX 音声パラメータの全体上書き(None は上書きしない)。
    #[serde(default)]
    pub voicevox_params: VoicevoxParams,
    /// キュー長・文字数に応じた自動加速。
    #[serde(default)]
    pub auto_speed: TtsAutoSpeed,
    /// キューの上限(件数・待機秒)。
    #[serde(default)]
    pub queue: TtsQueueLimits,
    /// 読み上げテンプレート。
    #[serde(default)]
    pub templates: TtsTemplates,
}

impl Default for TtsOptions {
    fn default() -> Self {
        TtsOptions {
            bouyomi_host: default_bouyomi_host(),
            bouyomi_port: default_bouyomi_port(),
            bouyomi_speed: -1,
            bouyomi_volume: -1,
            bouyomi_tone: -1,
            bouyomi_voice: 0,
            bouyomi_path: String::new(),
            bouyomi_launch_elevated: false,
            voicevox_url: default_voicevox_url(),
            voicevox_speaker: default_voicevox_speaker(),
            web_speech_rate: default_web_speech_rate(),
            web_speech_pitch: default_web_speech_pitch(),
            web_speech_volume: default_web_speech_volume(),
            web_speech_voice: default_web_speech_voice(),
            read_name: true,
            name_format: default_tts_name_format(),
            exclude_users: Vec::new(),
            read_kinds: TtsReadKinds::default(),
            skip_commands: false,
            omit_url: true,
            strip_emoji: true,
            max_length: default_max_read_len(),
            dictionary: Vec::new(),
            voicevox_engine: VoicevoxEngineKind::default(),
            voicevox_params: VoicevoxParams::default(),
            auto_speed: TtsAutoSpeed::default(),
            queue: TtsQueueLimits::default(),
            templates: TtsTemplates::default(),
        }
    }
}

/// VOICEVOX 互換エンジンの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoicevoxEngineKind {
    #[default]
    Voicevox,
    AivisSpeech,
}

impl VoicevoxEngineKind {
    fn as_str(self) -> &'static str {
        match self {
            VoicevoxEngineKind::Voicevox => "voicevox",
            VoicevoxEngineKind::AivisSpeech => "aivisSpeech",
        }
    }
}

impl Serialize for VoicevoxEngineKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// 未知の文字列・型は Voicevox に落とす(load 失敗を防ぐ)。
impl<'de> Deserialize<'de> for VoicevoxEngineKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(deserializer)?;
        Ok(match v.as_str() {
            Some("aivisSpeech") => VoicevoxEngineKind::AivisSpeech,
            _ => VoicevoxEngineKind::Voicevox,
        })
    }
}

/// VOICEVOX 音声パラメータ。None は AudioQuery の値を上書きしない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct VoicevoxParams {
    pub speed_scale: Option<f32>,
    pub pitch_scale: Option<f32>,
    pub intonation_scale: Option<f32>,
    pub volume_scale: Option<f32>,
    pub pre_phoneme_length: Option<f32>,
    pub post_phoneme_length: Option<f32>,
}

/// 自動加速設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsAutoSpeed {
    pub enabled: bool,
    /// この件数を超えた待ちから加速する。
    pub queue_threshold: u32,
    /// 閾値超過 1 件あたりの加算。
    pub per_queued: f32,
    /// この文字数を超えると加速(0 で文字数項なし)。
    pub length_threshold: u32,
    /// 閾値超過 10 文字あたりの加算。
    pub per_ten_chars: f32,
    pub max_speed: f32,
}

impl Default for TtsAutoSpeed {
    fn default() -> Self {
        TtsAutoSpeed {
            enabled: false,
            queue_threshold: 2,
            per_queued: 0.1,
            length_threshold: 40,
            per_ten_chars: 0.05,
            max_speed: 1.6,
        }
    }
}

/// 読み上げキューの上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsQueueLimits {
    /// 待機件数の上限(1..=500)。超過時は古い順に捨てる。
    pub max_pending: u32,
    /// 待機秒数の上限。0 は無制限。
    pub max_wait_sec: u32,
}

impl Default for TtsQueueLimits {
    fn default() -> Self {
        TtsQueueLimits {
            max_pending: 64,
            max_wait_sec: 0,
        }
    }
}

/// 読み上げテンプレート。種別別は None なら `all` を使い、`Some("")` は読まない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsTemplates {
    pub all: String,
    pub normal: Option<String>,
    pub super_chat: Option<String>,
    pub membership: Option<String>,
    pub bits: Option<String>,
    pub gift: Option<String>,
    pub system: Option<String>,
}

impl Default for TtsTemplates {
    fn default() -> Self {
        TtsTemplates {
            all: "{name} {text}".to_string(),
            normal: None,
            super_chat: None,
            membership: None,
            bits: None,
            gift: None,
            system: None,
        }
    }
}

fn clamp_opt(v: &mut Option<f32>, lo: f32, hi: f32) {
    if let Some(x) = v {
        *x = if x.is_finite() { x.clamp(lo, hi) } else { lo.max(0.0).min(hi) };
        if !x.is_finite() {
            *v = None;
        }
    }
}

fn clamp_f32(v: &mut f32, lo: f32, hi: f32, default: f32) {
    *v = if v.is_finite() { v.clamp(lo, hi) } else { default };
}

impl TtsOptions {
    /// 範囲外の値をクランプする(`AppConfig::load` と `update_config` から呼ぶ)。
    pub fn normalize(&mut self) {
        let p = &mut self.voicevox_params;
        clamp_opt(&mut p.speed_scale, 0.5, 2.0);
        clamp_opt(&mut p.pitch_scale, -0.15, 0.15);
        clamp_opt(&mut p.intonation_scale, 0.0, 2.0);
        clamp_opt(&mut p.volume_scale, 0.0, 2.0);
        clamp_opt(&mut p.pre_phoneme_length, 0.0, 1.5);
        clamp_opt(&mut p.post_phoneme_length, 0.0, 1.5);

        let d = TtsAutoSpeed::default();
        let a = &mut self.auto_speed;
        a.queue_threshold = a.queue_threshold.min(500);
        a.length_threshold = a.length_threshold.min(1000);
        clamp_f32(&mut a.per_queued, 0.0, 1.0, d.per_queued);
        clamp_f32(&mut a.per_ten_chars, 0.0, 1.0, d.per_ten_chars);
        clamp_f32(&mut a.max_speed, 0.5, 2.0, d.max_speed);

        self.queue.max_pending = self.queue.max_pending.clamp(1, 500);
        self.queue.max_wait_sec = self.queue.max_wait_sec.min(86_400);
    }
}

/// モデレーション設定(MVP=ローカル処理のみ)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModerationConfig {
    /// NGワード(正規表現)。マッチしたメッセージは隠す/グレー化。
    #[serde(default)]
    pub ng_words: Vec<String>,
    /// NGユーザー(著者名/IDに対する正規表現)。
    #[serde(default)]
    pub ng_users: Vec<String>,
    /// ハイライトルール(正規表現)。マッチで flag 付与。
    #[serde(default)]
    pub highlights: Vec<String>,
}

/// UI 表示設定。
// `Eq` は付けない: notify_volume が f32 で Eq 非実装(E0277)。PartialEq のみ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfig {
    /// リングバッファの保持上限件数。既定 2000。
    #[serde(default = "default_max_buffer")]
    pub max_buffer: usize,
    /// YouTubeメンバーの通常コメントで投稿者名を緑色にするか。既定 true。
    #[serde(default = "default_true")]
    pub youtube_member_name_green: bool,
    /// Twitchの通常コメントを本家チャット風に表示するか。既定 true。
    #[serde(default = "default_true")]
    pub twitch_native_style: bool,
    /// 投げ銭を別タブで表示するか。既定 false。
    #[serde(default)]
    pub show_donation_panel: bool,
    /// 初見コメントを別タブで表示するか。既定 false。
    #[serde(default)]
    pub show_first_comment_panel: bool,
    /// ハイライト一致コメント到着時に効果音で通知するか。既定 false。
    #[serde(default)]
    pub notify_sound: bool,
    /// 通知音の音量(0.0〜1.0)。既定 0.5。
    #[serde(default = "default_notify_volume")]
    pub notify_volume: f32,
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            max_buffer: default_max_buffer(),
            youtube_member_name_green: true,
            twitch_native_style: true,
            show_donation_panel: false,
            show_first_comment_panel: false,
            notify_sound: false,
            notify_volume: default_notify_volume(),
        }
    }
}

/// 参加型配信の参加管理設定。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct ParticipationConfig {
    /// 参加管理を有効にするか。既定 false。
    pub enabled: bool,
    /// 参加登録に使うキーワード。既定 "参加"。
    pub keyword: String,
    /// 最大参加者数。0 は無制限。
    pub max: u32,
}

impl Default for ParticipationConfig {
    fn default() -> Self {
        ParticipationConfig {
            enabled: false,
            keyword: default_participation_keyword(),
            max: 0,
        }
    }
}

fn twitcasting_overrides_is_default(o: &crate::sources::twitcasting::TwitcastingOverrides) -> bool {
    *o == Default::default()
}

/// YouTube InnerTube の仕様変更を再ビルド無しで吸収するための上書き設定。
///
/// いずれも `None`/空のときは parser/innertube 側の既定挙動を使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeOverrides {
    /// INNERTUBE_API_KEY を直接指定(初期HTML抽出をスキップ)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// clientVersion を直接指定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// 抽出パス等の上書き(キー→パターン)。空なら既定。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub paths: std::collections::HashMap<String, String>,
}

/// X (Twitter) ライブ配信の仕様変更を再ビルド無しで吸収するための上書き設定。
///
/// いずれも `None`/空のときは sources/x.rs 側の既定挙動を使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct XOverrides {
    /// X Web クライアントの公開 Bearer トークンを差し替える。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// API エンドポイント URL の上書き(キー→URL)。空なら既定。
    /// キー: guestActivateUrl / broadcastShowUrl / liveChatUrl / userQueryUrl /
    /// userByScreenNameUrl / avatarContentUrl
    /// (sources/x.rs・x_live.rs が実際に読むキー。liveChatUrl が本文ストリーム、
    /// userQueryUrl が cookie 併用時のユーザー名解決、userByScreenNameUrl が
    /// アカウントモードの handle→user id(GraphQL、queryId 込み URL)、
    /// avatarContentUrl が配信中判定。最も壊れやすい liveChatUrl はここで差替可)。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub endpoints: std::collections::HashMap<String, String>,
    /// アカウントモードのライブ検知ポーリング間隔(秒)。既定 30、下限 15。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_poll_secs: Option<u64>,
}

/// ニコニコ生放送の仕様変更を再ビルド無しで吸収するための上書き設定。
///
/// いずれも空のときは sources/niconico.rs 側の既定挙動を使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct NiconicoOverrides {
    /// URL の上書き(キー→URL)。空なら既定。
    /// キー: watchPageBaseUrl (既定 https://live.nicovideo.jp/watch/)。
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub endpoints: std::collections::HashMap<String, String>,
}

/// チャット送信・外部API利用に使う認証情報。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsConfig {
    /// Twitch 投稿用 OAuth token。`oauth:` prefix は送信時に正規化する。
    #[serde(default)]
    pub twitch_oauth: String,
    /// Twitch 投稿用ユーザー名。
    #[serde(default)]
    pub twitch_username: String,
    /// Twitch 同接取得用の App Access Token を client_credentials で自動取得するアプリのクライアントID。
    /// チャット送信用の `twitch_oauth` とは別の認証情報。
    #[serde(default)]
    pub twitch_client_id: String,
    /// Twitch 同接取得用の App Access Token を client_credentials で自動取得するアプリのクライアントシークレット。
    /// チャット送信用の `twitch_oauth` とは別の認証情報。
    #[serde(default)]
    pub twitch_client_secret: String,
    /// YouTube Data API v3 の公式 streamList で使う API キー。
    /// 空なら従来の無認証 InnerTube ポーリングへフォールバックする。
    #[serde(default)]
    pub youtube_api_key: String,
    /// YouTube投稿用の「デスクトップアプリ」OAuthクライアントID。
    /// refresh token自体はOSの資格情報ストアへ保存し、config.jsonには入れない。
    #[serde(default)]
    pub youtube_oauth_client_id: String,
    /// X ライブチャットのユーザー名解決に使うログイン cookie(auth_token)。
    /// live-chat の NDJSON に username が無く、ゲストで解決する API も無いため、
    /// 設定時のみ GraphQL liveAtomsUserQuery で userId→表示名を引く。
    /// 空なら「ユーザー<ID下4桁>」の匿名表示のまま動く(チャット受信は止まらない)。
    #[serde(default)]
    pub x_auth_token: String,
    /// X の csrf トークン(ct0 cookie)。x_auth_token とペアで設定する。
    #[serde(default)]
    pub x_csrf_token: String,
    /// X 自動告知(API モード)用の OAuth 2.0 Client ID。秘密値ではないが他の Client ID と同じ場所に置く。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_post_client_id: Option<String>,
    /// 公開 ClientID の上書き。トークンは資格情報ストアにのみ保存する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub twitcasting_client_id: Option<String>,
}

pub(crate) const MIN_OAUTH_CALLBACK_PORT: u16 = 1024;

/// X 自動告知の設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct XPostConfig {
    pub enabled: bool,
    /// "intent" | "api"
    pub mode: String,
    /// "button" | "auto"
    pub intent_open: String,
    pub combine: bool,
    pub template: String,
    pub delay_secs: u32,
    pub stale_minutes: u32,
    pub cooldown_minutes: u32,
    pub daily_limit: u32,
    pub callback_port: u16,
}

pub const DEFAULT_X_POST_TEMPLATE: &str = "{title}\n{url}\n{date} {time}";

impl Default for XPostConfig {
    fn default() -> Self {
        XPostConfig {
            enabled: false,
            mode: "intent".to_string(),
            intent_open: "button".to_string(),
            combine: false,
            template: DEFAULT_X_POST_TEMPLATE.to_string(),
            delay_secs: 60,
            stale_minutes: 15,
            cooldown_minutes: 60,
            daily_limit: 5,
            callback_port: 47821,
        }
    }
}

/// ツイキャスのログイン設定。資格情報ではないので `credentials` には置かない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TwitcastingConfig {
    /// ログイン待受(ループバック)のポート。X の `callbackPort` と同じく 0 はログイン時に拒否する。
    pub callback_port: u16,
    /// 取得経路。チャンネル Source の起動時に読む(稼働中の Source は作り直さない)。
    #[serde(default)]
    pub mode: TwitcastingMode,
}

/// ツイキャスの取得経路。未知の値・型違い・null は `Auto` に寛容に読む。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TwitcastingMode {
    #[default]
    Auto,
    Unofficial,
    Official,
}

impl<'de> Deserialize<'de> for TwitcastingMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some("unofficial") => TwitcastingMode::Unofficial,
            Some("official") => TwitcastingMode::Official,
            _ => TwitcastingMode::Auto,
        })
    }
}

impl Default for TwitcastingConfig {
    fn default() -> Self {
        TwitcastingConfig {
            callback_port: crate::sources::twitcasting_auth::CALLBACK_PORT,
            mode: TwitcastingMode::Auto,
        }
    }
}

impl TwitcastingConfig {
    /// 1024 未満(0 を含む)の待受ポートは既定に戻す(UI の検証と同じ下限)。
    pub fn normalize(&mut self) {
        if self.callback_port < MIN_OAUTH_CALLBACK_PORT {
            self.callback_port = TwitcastingConfig::default().callback_port;
        }
    }
}

impl XPostConfig {
    /// 範囲外の値を許容範囲へ丸める。
    pub fn normalize(&mut self) {
        // 1024 未満(0 を含む)のポートは既定に戻す。1024 以上の既存設定は変えない。
        if self.callback_port < MIN_OAUTH_CALLBACK_PORT {
            self.callback_port = XPostConfig::default().callback_port;
        }
        self.delay_secs = self.delay_secs.min(600);
        self.stale_minutes = self.stale_minutes.min(180);
        self.cooldown_minutes = self.cooldown_minutes.clamp(10, 1440);
        self.daily_limit = self.daily_limit.clamp(1, 20);
        if self.mode != "intent" && self.mode != "api" {
            self.mode = "intent".to_string();
        }
        if self.intent_open != "button" && self.intent_open != "auto" {
            self.intent_open = "button".to_string();
        }
    }
}

/// コメントログの自動保存設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LogConfig {
    pub enabled: bool,
    pub retention_days: u32,
    pub include_hidden: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { enabled: true, retention_days: 30, include_hidden: false }
    }
}

impl LogConfig {
    pub fn normalize(&mut self) {
        self.retention_days = self.retention_days.min(3650);
    }
}

/// アプリ全体設定のルート。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub obs: ObsConfig,
    #[serde(default)]
    pub goals: GoalsConfig,
    #[serde(default)]
    pub timer: TimerConfig,
    #[serde(default)]
    pub effects: EffectsConfig,
    #[serde(default)]
    pub welcome: WelcomeConfig,
    #[serde(default)]
    pub tts: TtsConfig,
    #[serde(default)]
    pub moderation: ModerationConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub participation: ParticipationConfig,
    #[serde(default)]
    pub youtube_overrides: YoutubeOverrides,
    #[serde(default)]
    pub x_overrides: XOverrides,
    #[serde(default)]
    pub niconico_overrides: NiconicoOverrides,
    /// ツイキャス非公式経路の上書き。空なら保存時に出力しない(旧 config と同形に保つ)。
    #[serde(default, skip_serializing_if = "twitcasting_overrides_is_default")]
    pub twitcasting_overrides: crate::sources::twitcasting::TwitcastingOverrides,
    #[serde(default)]
    pub credentials: CredentialsConfig,
    #[serde(default)]
    pub x_post: XPostConfig,
    #[serde(default)]
    pub twitcasting: TwitcastingConfig,
    /// 種別ごとの効果音。読み込みは寛容(壊れた項目は既定値)。
    #[serde(default)]
    pub sound_effects: crate::sound_effects::SoundEffectsConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            log: LogConfig::default(),
            channels: Vec::new(),
            obs: ObsConfig::default(),
            goals: GoalsConfig::default(),
            timer: TimerConfig::default(),
            effects: EffectsConfig::default(),
            welcome: WelcomeConfig::default(),
            tts: TtsConfig::default(),
            moderation: ModerationConfig::default(),
            ui: UiConfig::default(),
            participation: ParticipationConfig::default(),
            youtube_overrides: YoutubeOverrides::default(),
            x_overrides: XOverrides::default(),
            niconico_overrides: NiconicoOverrides::default(),
            twitcasting_overrides: Default::default(),
            credentials: CredentialsConfig::default(),
            x_post: XPostConfig::default(),
            twitcasting: TwitcastingConfig::default(),
            sound_effects: Default::default(),
        }
    }
}

fn config_error_path(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;

    // 既定の設定に存在するフィールド名だけを採用し、配列やマップの要素には進まない。
    let defaults = serde_json::to_value(AppConfig::default()).unwrap_or_default();
    let mut schema = &defaults;
    let mut names = Vec::new();
    for segment in path {
        let Segment::Map { key } = segment else {
            break;
        };
        if let Some(next) = schema.get(key) {
            names.push(key.as_str());
            schema = next;
            continue;
        }
        // 既定値の JSON で省略される固定フィールド。任意のマップキーは表示しない。
        if matches!(
            (names.as_slice(), key.as_str()),
            (["youtubeOverrides"], "apiKey" | "clientVersion" | "paths")
                | (["xOverrides"], "bearerToken" | "endpoints" | "livePollSecs")
                | (["niconicoOverrides"], "endpoints")
                | (["credentials"], "xPostClientId" | "twitcastingClientId")
        ) {
            names.push(key.as_str());
        }
        break;
    }
    if names.is_empty() {
        return ".".to_owned();
    }
    names.join(".")
}

impl AppConfig {
    /// 指定ディレクトリ配下の `config.json` パスを返す。
    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    /// `config.json` をロードする。存在しなければ既定値を返す。
    ///
    /// パース失敗時はエラーを返す(壊れた設定で黙って初期化しないため)。
    pub fn load(dir: &Path) -> anyhow::Result<AppConfig> {
        let path = Self::path_in(dir);
        let Some(text) = read_config_text(&path, READ_RETRY_DELAY, Path::try_exists, |p| {
            std::fs::read_to_string(p)
        })
        .map_err(|e| anyhow::anyhow!("設定ファイル読込失敗 {}: {e}", path.display()))?
        else {
            return Ok(AppConfig::default());
        };
        // serde の文面は不正な値をそのまま含む(トークン等が紛れうる)ので、項目と位置だけ出す。
        let mut deserializer = serde_json::Deserializer::from_str(&text);
        let mut cfg: AppConfig =
            serde_path_to_error::deserialize(&mut deserializer)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "設定ファイル解析失敗 {}: 項目 {} ({} 行 {} 列)",
                        path.display(),
                        config_error_path(e.path()),
                        e.inner().line(),
                        e.inner().column()
                    )
                })?;
        deserializer.end().map_err(|e| {
            anyhow::anyhow!(
                "設定ファイル解析失敗 {}: JSON の末尾が不正です ({} 行 {} 列)",
                path.display(),
                e.line(),
                e.column()
            )
        })?;
        cfg.normalize_all();
        Ok(cfg)
    }

    /// 起動時のロード。失敗したら元ファイルを `config.json.broken-<unix-ms>` へ複製してから既定値で続行する
    /// (次の保存で上書きされ、チャンネルや資格情報を失わないため)。既存の退避ファイルは上書きしない。
    /// 戻り値は退避したか・保存を止めたか(起動側で利用者に知らせる)。
    pub fn load_or_preserve(dir: &Path) -> (AppConfig, ConfigLoadOutcome) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        Self::load_or_preserve_at(dir, now_ms)
    }

    fn load_or_preserve_at(dir: &Path, now_ms: u128) -> (AppConfig, ConfigLoadOutcome) {
        let e = match Self::load(dir) {
            Ok(cfg) => return (cfg, ConfigLoadOutcome::Loaded),
            Err(e) => e,
        };
        let path = Self::path_in(dir);
        match preserve_broken_config(&path, now_ms) {
            Ok(backup) => {
                prune_broken_backups(&path, &backup);
                tracing::warn!(
                    "設定ロード失敗・{} に退避して既定値で続行: {e}",
                    backup.file_name().unwrap_or_default().to_string_lossy()
                );
                (AppConfig::default(), ConfigLoadOutcome::BackedUp)
            }
            Err(err) => {
                tracing::warn!("設定ロード失敗・退避できず保存を止めて既定値で続行: {e} (退避: {})", err.kind());
                SAVE_BLOCKED_DIRS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(dir.to_path_buf());
                (AppConfig::default(), ConfigLoadOutcome::SavesBlocked)
            }
        }
    }

    /// IPC の入力値はエラー文に含めない。
    pub(crate) fn from_config_update(value: serde_json::Value) -> Result<Self, String> {
        serde_path_to_error::deserialize(value).map_err(|e| {
            format!("設定の形式が不正です (項目 {})", config_error_path(e.path()))
        })
    }

    /// 範囲外値の丸め。ロード時と更新(保存前)の双方から呼ぶ。
    pub fn normalize_all(&mut self) {
        self.obs.normalize();
        self.log.normalize();
        self.tts.options.normalize();
        self.x_post.normalize();
        self.twitcasting.normalize();
        self.sound_effects.normalize();
        self.effects.normalize();
        for channel in &mut self.channels {
            channel.overrides.normalize();
        }
    }

    pub fn channel_overrides(&self, key: &str) -> Option<&ChannelOverrides> {
        self.channels
            .iter()
            .find(|ch| channel_matches_key(ch, key))
            .map(|ch| &ch.overrides)
    }

    /// `config.json` へ保存する(ディレクトリは必要なら作成)。
    ///
    /// 同一ディレクトリの一時ファイルへ書き込み → fsync → rename で原子的に置換する。
    /// rename は NTFS/POSIX いずれでも同一ディレクトリ内であれば原子的。
    ///
    /// クレート内では `commit_config` だけが呼ぶ(revision を進めない保存を作らない。lib.rs の
    /// `every_config_write_goes_through_commit_config` で検査)。受け入れテストが使うので公開している。
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        if save_blocked(dir) {
            return Err(anyhow::Error::new(ConfigSaveBlocked));
        }
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("設定ディレクトリ作成失敗 {}: {e}", dir.display()))?;
        let path = Self::path_in(dir);
        let tmp_path = dir.join("config.json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        {
            let mut f = File::create(&tmp_path)
                .map_err(|e| anyhow::anyhow!("設定一時ファイル作成失敗 {}: {e}", tmp_path.display()))?;
            f.write_all(text.as_bytes())
                .map_err(|e| anyhow::anyhow!("設定一時ファイル書込失敗 {}: {e}", tmp_path.display()))?;
            f.sync_all()
                .map_err(|e| anyhow::anyhow!("設定一時ファイル同期失敗 {}: {e}", tmp_path.display()))?;
        }
        std::fs::rename(&tmp_path, &path)
            .map_err(|e| anyhow::anyhow!("設定ファイル置換失敗 {} -> {}: {e}", tmp_path.display(), path.display()))?;
        Ok(())
    }
}

/// 全体保存の基準 revision が古い時の固定文言。
pub const CONFIG_CHANGED_ERROR: &str = "設定が別の操作で変更されました。読み直してから保存してください";

#[derive(Debug)]
pub enum ConfigCommitError {
    /// `base_revision` が現在の revision と違う。保存していない。
    Stale,
    Save(anyhow::Error),
}

impl std::fmt::Display for ConfigCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale => f.write_str(CONFIG_CHANGED_ERROR),
            Self::Save(e) => e.fmt(f),
        }
    }
}

/// 設定をディスクへ保存し、成功した時だけメモリ・watch・revision を進める。
///
/// 設定を書くすべての経路がここを通る。
/// 呼び出し側は `current`(= 設定のロック)を保持していること。`base_revision` が `Some` で
/// 現在値と違えば何も書かずに `Stale` を返す。
pub fn commit_config(
    dir: &Path,
    revision: &AtomicU64,
    tx: &tokio::sync::watch::Sender<AppConfig>,
    current: &mut AppConfig,
    next: AppConfig,
    base_revision: Option<u64>,
) -> Result<u64, ConfigCommitError> {
    if base_revision.is_some_and(|base| base != revision.load(Ordering::SeqCst)) {
        return Err(ConfigCommitError::Stale);
    }
    next.save(dir).map_err(ConfigCommitError::Save)?;
    *current = next;
    tx.send_replace(current.clone());
    Ok(revision.fetch_add(1, Ordering::SeqCst) + 1)
}

#[derive(Debug)]
pub enum ChannelOverridesCommitError {
    Stale,
    NotFound,
    Save(anyhow::Error),
}

impl std::fmt::Display for ChannelOverridesCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stale => f.write_str(CONFIG_CHANGED_ERROR),
            Self::NotFound => std::fmt::Display::fmt(&ChannelEditError::NotFound, f),
            Self::Save(e) if e.is::<ConfigSaveBlocked>() => f.write_str(CONFIG_SAVE_BLOCKED_ERROR),
            Self::Save(_) => f.write_str("枠の設定を保存できませんでした"),
        }
    }
}

pub fn commit_channel_overrides(
    dir: &Path,
    revision: &AtomicU64,
    tx: &tokio::sync::watch::Sender<AppConfig>,
    current: &mut AppConfig,
    key: &str,
    overrides: ChannelOverrides,
    base_revision: u64,
) -> Result<u64, ChannelOverridesCommitError> {
    if base_revision != revision.load(Ordering::SeqCst) {
        return Err(ChannelOverridesCommitError::Stale);
    }
    let mut next = current.clone();
    set_channel_overrides_in(&mut next, key, overrides)
        .map_err(|_| ChannelOverridesCommitError::NotFound)?;
    commit_config(dir, revision, tx, current, next, Some(base_revision)).map_err(|err| match err {
        ConfigCommitError::Stale => ChannelOverridesCommitError::Stale,
        ConfigCommitError::Save(err) => ChannelOverridesCommitError::Save(err),
    })
}

// ---- serde default ヘルパ ----

fn default_true() -> bool {
    true
}
fn default_minus_one() -> i16 {
    -1
}
fn default_obs_port() -> u16 {
    11180
}
fn default_obs_template() -> String {
    "default".to_string()
}
fn default_obs_font_scale_pct() -> u16 {
    100
}
fn default_obs_danmaku_font_size() -> u16 {
    30
}
fn default_obs_max_rows() -> u16 {
    8
}
fn default_obs_ttl_ms() -> u32 {
    12000
}
fn default_obs_bg_opacity_pct() -> u16 {
    0
}
fn default_obs_position() -> String {
    "bottom".to_string()
}
fn default_goals_layout() -> String {
    "horizontal".to_string()
}
fn default_goals_skin() -> String {
    "glass".to_string()
}
fn default_timer_duration_sec() -> u32 {
    300
}
fn default_timer_mode() -> String {
    "countdown".to_string()
}
fn default_bouyomi_host() -> String {
    "127.0.0.1".to_string()
}
fn default_bouyomi_port() -> u16 {
    50001
}
fn default_voicevox_url() -> String {
    "http://127.0.0.1:50021".to_string()
}
fn default_voicevox_speaker() -> u32 {
    1
}
fn default_web_speech_rate() -> f32 {
    1.0
}
fn default_web_speech_pitch() -> f32 {
    1.0
}
fn default_web_speech_volume() -> f32 {
    1.0
}
fn default_web_speech_voice() -> String {
    String::new()
}
fn default_notify_volume() -> f32 {
    0.5
}

fn default_max_buffer() -> usize {
    2000
}
fn default_max_read_len() -> usize {
    140
}
fn default_participation_keyword() -> String {
    "参加".to_string()
}
fn default_effect_count() -> u32 {
    12
}
fn default_effect_animation() -> String {
    "float".to_string()
}
fn default_effect_duration_ms() -> u32 {
    3000
}
fn default_effect_cooldown_ms() -> u32 {
    2000
}
fn default_welcome_greeting() -> String {
    "{name} さん、いらっしゃい！".to_string()
}
fn default_welcome_emoji() -> String {
    "👋".to_string()
}
fn default_welcome_count() -> u32 {
    8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_overrides_reject_positional_arrays() {
        for overrides in [serde_json::json!([]), serde_json::json!([false])] {
            let err = AppConfig::from_config_update(serde_json::json!({
                "channels": [{"platform": "x", "identifier": "live", "overrides": overrides}]
            }))
            .unwrap_err();
            assert!(err.contains("channels"), "{err}");
        }
    }
    #[test]
    fn legacy_config_without_own_stream_or_x_post_loads_with_defaults() {
        let legacy = r#"{"channels":[{"platform":"twitch","identifier":"abc"}],"credentials":{"twitchOauth":"x"}}"#;
        let cfg: AppConfig = serde_json::from_str(legacy).expect("legacy config");
        assert!(!cfg.channels[0].own_stream);
        assert_eq!(cfg.credentials.x_post_client_id, None);
        assert_eq!(cfg.x_post, XPostConfig::default());
        assert!(!cfg.x_post.enabled);
        assert_eq!(cfg.x_post.mode, "intent");
        assert_eq!(cfg.x_post.intent_open, "button");
        assert!(!cfg.x_post.combine);
        assert_eq!(cfg.x_post.template, "{title}\n{url}\n{date} {time}");
        assert_eq!(cfg.x_post.delay_secs, 60);
        assert_eq!(cfg.x_post.stale_minutes, 15);
        assert_eq!(cfg.x_post.cooldown_minutes, 60);
        assert_eq!(cfg.x_post.daily_limit, 5);
        assert_eq!(cfg.x_post.callback_port, 47821);
        assert_eq!(cfg.twitcasting, TwitcastingConfig::default());
        assert_eq!(cfg.twitcasting.callback_port, 39822);
        assert_eq!(cfg.twitcasting.mode, TwitcastingMode::Auto);
    }

    #[test]
    fn callback_ports_below_1024_fall_back_to_defaults() {
        for bad in [0u16, 80, 1023] {
            let mut cfg: AppConfig = serde_json::from_str(&format!(
                r#"{{"twitcasting":{{"callbackPort":{bad}}},"xPost":{{"callbackPort":{bad}}}}}"#
            ))
            .expect("ports");
            cfg.normalize_all();
            assert_eq!(cfg.twitcasting.callback_port, 39822, "{bad}");
            assert_eq!(cfg.x_post.callback_port, 47821, "{bad}");
        }
        // Existing values of 1024 and above are kept as they are.
        for kept in [1024u16, 40123, 47821, 65535] {
            let mut cfg: AppConfig = serde_json::from_str(&format!(
                r#"{{"twitcasting":{{"callbackPort":{kept}}},"xPost":{{"callbackPort":{kept}}}}}"#
            ))
            .expect("ports");
            cfg.normalize_all();
            assert_eq!(cfg.twitcasting.callback_port, kept);
            assert_eq!(cfg.x_post.callback_port, kept);
        }
    }

    #[test]
    fn load_error_names_the_field_and_position_but_not_the_value() {
        let dir = std::env::temp_dir().join(format!("fast-comment-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = "http://localhost/#access_token=TEST";
        std::fs::write(
            dir.join("config.json"),
            format!("{{\n  \"twitcasting\": {{\"callbackPort\": \"{secret}\"}}\n}}"),
        )
        .unwrap();
        let message = AppConfig::load(&dir).unwrap_err().to_string();
        assert!(!message.contains("access_token"), "{message}");
        assert!(!message.contains("TEST"), "{message}");
        assert!(message.contains("twitcasting.callbackPort"), "{message}");
        assert!(message.contains("2 行"), "{message}");
        // Syntax errors do not carry values either.
        std::fs::write(dir.join("config.json"), format!("{{\"credentials\": \"{secret}")).unwrap();
        let message = AppConfig::load(&dir).unwrap_err().to_string();
        assert!(!message.contains("access_token"), "{message}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_error_hides_override_map_keys() {
        let dir = std::env::temp_dir().join(format!("fast-comment-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            AppConfig::path_in(&dir),
            r#"{"xOverrides":{"endpoints":{"https://example.test/?token=TEST":42}}}"#,
        )
        .unwrap();
        let result = AppConfig::load(&dir);
        std::fs::remove_dir_all(dir).unwrap();
        let message = result.unwrap_err().to_string();
        assert!(message.contains("xOverrides.endpoints"));
        assert!(!message.contains("token=TEST"));
    }

    #[test]
    fn load_rejects_a_second_json_value() {
        let dir = std::env::temp_dir().join(format!("fast-comment-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(AppConfig::path_in(&dir), "{} {}").unwrap();
        let result = AppConfig::load(&dir);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn update_error_reports_callback_port_fields_without_input_values() {
        for field in ["twitcasting", "xPost"] {
            let result = AppConfig::from_config_update(serde_json::json!({
                (field): { "callbackPort": "https://example.test/#access_token=TEST" },
            }));
            assert_eq!(
                result.unwrap_err(),
                format!("設定の形式が不正です (項目 {field}.callbackPort)")
            );
        }
        assert_eq!(
            AppConfig::from_config_update(serde_json::json!({})).unwrap(),
            AppConfig::default()
        );
    }

    #[test]
    fn twitcasting_overrides_absent_in_old_config_and_omitted_when_empty() {
        let cfg: AppConfig = serde_json::from_str(r#"{"channels":[]}"#).expect("old config");
        assert_eq!(cfg.twitcasting_overrides, Default::default());
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert!(json.get("twitcasting_overrides").is_none());
        assert!(json.get("twitcastingOverrides").is_none());
        let cfg: AppConfig = serde_json::from_str(
            r#"{"twitcastingOverrides":{"endpoints":{"userPageBaseUrl":"https://x.test/"}}}"#,
        )
        .expect("with overrides");
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(json["twitcastingOverrides"]["endpoints"]["userPageBaseUrl"], "https://x.test/");
    }

    #[test]
    fn twitcasting_callback_port_reads_configured_value() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{"twitcasting":{"callbackPort":40123}}"#).expect("twitcasting");
        assert_eq!(cfg.twitcasting.callback_port, 40123);
        assert_eq!(cfg.twitcasting.mode, TwitcastingMode::Auto);
        let cfg: AppConfig = serde_json::from_str(r#"{"twitcasting":{}}"#).expect("empty twitcasting");
        assert_eq!(cfg.twitcasting.callback_port, 39822);
        assert_eq!(cfg.twitcasting.mode, TwitcastingMode::Auto);
        let cfg: AppConfig =
            serde_json::from_str(r#"{"twitcasting":{"callbackPort":40123,"mode":"official"}}"#)
                .expect("twitcasting mode");
        assert_eq!(cfg.twitcasting.mode, TwitcastingMode::Official);
    }

    #[test]
    fn x_post_partial_json_fills_defaults_and_normalize_clamps() {
        let mut cfg: AppConfig = serde_json::from_str(
            r#"{"xPost":{"enabled":true,"delaySecs":9999,"staleMinutes":999,"cooldownMinutes":1,"dailyLimit":0}}"#,
        )
        .expect("partial xPost");
        assert!(cfg.x_post.enabled);
        assert!(!cfg.x_post.combine);
        cfg.normalize_all();
        assert_eq!(cfg.x_post.delay_secs, 600);
        assert_eq!(cfg.x_post.stale_minutes, 180);
        assert_eq!(cfg.x_post.cooldown_minutes, 10);
        assert_eq!(cfg.x_post.daily_limit, 1);
        cfg.x_post.cooldown_minutes = 99999;
        cfg.x_post.daily_limit = 99;
        cfg.normalize_all();
        assert_eq!(cfg.x_post.cooldown_minutes, 1440);
        assert_eq!(cfg.x_post.daily_limit, 20);
    }

    use std::collections::HashMap;

    #[test]
    fn chatlog_legacy_defaults_and_normalization() {
        let dir = std::env::temp_dir().join(format!("fast-comment-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.json"), "{}").unwrap();
        let cfg = AppConfig::load(&dir).unwrap();
        assert!(cfg.log.enabled);
        assert_eq!(cfg.log.retention_days, 30);
        assert!(!cfg.log.include_hidden);
        std::fs::write(dir.join("config.json"), r#"{"log":{"retentionDays":4294967295}}"#).unwrap();
        assert_eq!(AppConfig::load(&dir).unwrap().log.retention_days, 3650);
        let mut log: LogConfig = serde_json::from_str(r#"{"retentionDays":0}"#).unwrap();
        log.normalize();
        assert_eq!(log.retention_days, 0);
        assert!(log.enabled);
        assert!(!log.include_hidden);
        let value = serde_json::to_value(log).unwrap();
        assert_eq!(value["retentionDays"], 0);
        assert_eq!(value["includeHidden"], false);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn serde_roundtrip_keeps_camel_case_keys_and_values() {
        // camelCase の永続化キーと、各フィールド値の serde 往復を確認する。
        let mut paths = HashMap::new();
        paths.insert(
            "continuationPath".to_string(),
            "contents.twoColumnWatchNextResults.conversationBar".to_string(),
        );

        let cfg = AppConfig {
            log: LogConfig::default(),
            channels: vec![ChannelConfig {
                platform: ChannelPlatform::Youtube,
                identifier: "https://www.youtube.com/watch?v=abc123".to_string(),
                enabled: false,
                own_stream: true,
                overrides: ChannelOverrides::default(),
            }],
            obs: ObsConfig {
                port: 12000,
                template: "compact".to_string(),
                font_scale_pct: 125,
                danmaku_font_size: 42,
                max_rows: 12,
                ttl_ms: 9000,
                bg_opacity_pct: 35,
                position: "top".to_string(),
                show_platform: false,
            },
            goals: GoalsConfig {
                enabled: true,
                show_in_app: true,
                layout: "grid".to_string(),
                skin: "solid".to_string(),
                show_comments: Some(true),
                show_viewers: Some(false),
                show_likes: Some(true),
                show_reactions: Some(false),
                comments: 100,
                viewers: 50,
                likes: 25,
                reactions: 0,
            },
            timer: TimerConfig {
                enabled: true,
                default_duration_sec: 900,
                mode: "elapsed".to_string(),
            },
            effects: EffectsConfig {
                enabled: true,
                rules: vec![EffectRule {
                    keyword: "party".to_string(),
                    emoji: "🎉".to_string(),
                    count: 24,
                    ..Default::default()
                }],
            },
            welcome: WelcomeConfig {
                enabled: true,
                greeting: "{name} さん、ようこそ！".to_string(),
                tts: true,
                emoji: "🌟".to_string(),
                count: 16,
            },
            tts: TtsConfig {
                backend: TtsBackendKind::Voicevox,
                options: TtsOptions {
                    bouyomi_host: "192.0.2.10".to_string(),
                    bouyomi_port: 50002,
                    bouyomi_speed: 90,
                    bouyomi_volume: 80,
                    bouyomi_tone: 70,
                    bouyomi_voice: 2,
                    bouyomi_path: r"C:\BouyomiChan\BouyomiChan.exe".to_string(),
                    bouyomi_launch_elevated: false,
                    voicevox_url: "http://127.0.0.1:50022".to_string(),
                    voicevox_speaker: 3,
                    web_speech_rate: 1.4,
                    web_speech_pitch: 0.8,
                    web_speech_volume: 0.6,
                    web_speech_voice: "Test Voice".to_string(),
                    read_name: false,
                    name_format: default_tts_name_format(),
                    exclude_users: Vec::new(),
                    read_kinds: TtsReadKinds::default(),
                    skip_commands: false,
                    omit_url: false,
                    strip_emoji: false,
                    max_length: 280,
                    dictionary: vec![
                        TtsDictEntry {
                            regex: false,
                            from: "fast".to_string(),
                            to: "ファスト".to_string(),
                        },
                        TtsDictEntry {
                            regex: false,
                            from: "comment".to_string(),
                            to: "コメント".to_string(),
                        },
                    ],
                    voicevox_engine: VoicevoxEngineKind::default(),
                    voicevox_params: VoicevoxParams::default(),
                    auto_speed: TtsAutoSpeed::default(),
                    queue: TtsQueueLimits::default(),
                    templates: TtsTemplates::default(),
                },
            },
            moderation: ModerationConfig {
                ng_words: vec!["spam".to_string()],
                ng_users: vec!["bot".to_string()],
                highlights: vec!["important".to_string()],
            },
            ui: UiConfig {
                max_buffer: 1234,
                youtube_member_name_green: false,
                twitch_native_style: false,
                show_donation_panel: true,
                show_first_comment_panel: true,
                notify_sound: true,
                notify_volume: 0.8,
            },
            participation: ParticipationConfig {
                enabled: true,
                keyword: "join".to_string(),
                max: 32,
            },
            credentials: CredentialsConfig {
                twitch_oauth: "oauth:test-token".to_string(),
                twitch_username: "FastCommentBot".to_string(),
                twitch_client_id: "twitch-client-id".to_string(),
                twitch_client_secret: "twitch-client-secret".to_string(),
                youtube_api_key: "youtube-data-api-key".to_string(),
                youtube_oauth_client_id: "desktop-client.apps.googleusercontent.com".to_string(),
                x_auth_token: "x-auth-cookie".to_string(),
                x_csrf_token: "x-ct0".to_string(),
                x_post_client_id: Some("x-post-client".to_string()),
                twitcasting_client_id: None,
            },
            youtube_overrides: YoutubeOverrides {
                api_key: Some("test-api-key".to_string()),
                client_version: Some("1.20240501.00.00".to_string()),
                paths,
            },
            x_overrides: XOverrides {
                bearer_token: Some("test-bearer".to_string()),
                endpoints: HashMap::new(),
                live_poll_secs: Some(45),
            },
            niconico_overrides: NiconicoOverrides::default(),
            twitcasting_overrides: Default::default(),
            x_post: XPostConfig::default(),
            twitcasting: TwitcastingConfig::default(),
            sound_effects: Default::default(),
        };

        let text = serde_json::to_string(&cfg).expect("serialize AppConfig");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse serialized JSON");

        assert_eq!(json["channels"][0]["ownStream"].as_bool(), Some(true));
        assert_eq!(json["credentials"]["xPostClientId"].as_str(), Some("x-post-client"));
        assert_eq!(json["xPost"]["callbackPort"].as_u64(), Some(47821));
        assert_eq!(json["twitcasting"]["callbackPort"].as_u64(), Some(39822));
        assert_eq!(json["twitcasting"]["mode"].as_str(), Some("auto"));
        assert_eq!(json["obs"]["template"].as_str(), Some("compact"));
        assert_eq!(json["obs"]["fontScalePct"].as_u64(), Some(125));
        assert_eq!(json["obs"]["danmakuFontSize"].as_u64(), Some(42));
        assert_eq!(json["obs"]["maxRows"].as_u64(), Some(12));
        assert_eq!(json["obs"]["ttlMs"].as_u64(), Some(9000));
        assert_eq!(json["obs"]["bgOpacityPct"].as_u64(), Some(35));
        assert_eq!(json["obs"]["position"].as_str(), Some("top"));
        assert_eq!(json["obs"]["showPlatform"].as_bool(), Some(false));
        assert_eq!(json["goals"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["goals"]["showInApp"].as_bool(), Some(true));
        assert_eq!(json["goals"]["layout"].as_str(), Some("grid"));
        assert_eq!(json["goals"]["skin"].as_str(), Some("solid"));
        assert_eq!(json["goals"]["showComments"].as_bool(), Some(true));
        assert_eq!(json["goals"]["showViewers"].as_bool(), Some(false));
        assert_eq!(json["goals"]["comments"].as_u64(), Some(100));
        assert_eq!(json["goals"]["viewers"].as_u64(), Some(50));
        assert_eq!(json["goals"]["likes"].as_u64(), Some(25));
        assert_eq!(json["goals"]["reactions"].as_u64(), Some(0));
        assert_eq!(json["timer"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["timer"]["defaultDurationSec"].as_u64(), Some(900));
        assert_eq!(json["timer"]["mode"].as_str(), Some("elapsed"));
        assert_eq!(json["effects"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["effects"]["rules"][0]["keyword"].as_str(), Some("party"));
        assert_eq!(json["effects"]["rules"][0]["emoji"].as_str(), Some("🎉"));
        assert_eq!(json["effects"]["rules"][0]["count"].as_u64(), Some(24));
        assert_eq!(json["welcome"]["enabled"].as_bool(), Some(true));
        assert_eq!(
            json["ui"]["youtubeMemberNameGreen"].as_bool(),
            Some(false)
        );
        assert_eq!(json["ui"]["twitchNativeStyle"].as_bool(), Some(false));
        assert_eq!(
            json["welcome"]["greeting"].as_str(),
            Some("{name} さん、ようこそ！")
        );
        assert_eq!(json["welcome"]["tts"].as_bool(), Some(true));
        assert_eq!(json["welcome"]["emoji"].as_str(), Some("🌟"));
        assert_eq!(json["welcome"]["count"].as_u64(), Some(16));
        assert_eq!(json["ui"]["maxBuffer"].as_u64(), Some(1234));
        assert_eq!(json["ui"]["showDonationPanel"].as_bool(), Some(true));
        assert_eq!(json["ui"]["showFirstCommentPanel"].as_bool(), Some(true));
        assert_eq!(json["ui"]["notifySound"].as_bool(), Some(true));
        // f32→JSON→f64 はビット表現が変わるので近似比較する。
        assert!((json["ui"]["notifyVolume"].as_f64().expect("notifyVolume") - 0.8).abs() < 1e-6);
        assert_eq!(json["participation"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["participation"]["keyword"].as_str(), Some("join"));
        assert_eq!(json["participation"]["max"].as_u64(), Some(32));
        assert_eq!(
            json["credentials"]["twitchOauth"].as_str(),
            Some("oauth:test-token")
        );
        assert_eq!(
            json["credentials"]["twitchUsername"].as_str(),
            Some("FastCommentBot")
        );
        assert_eq!(
            json["credentials"]["twitchClientId"].as_str(),
            Some("twitch-client-id")
        );
        assert_eq!(
            json["credentials"]["twitchClientSecret"].as_str(),
            Some("twitch-client-secret")
        );
        assert_eq!(
            json["credentials"]["youtubeApiKey"].as_str(),
            Some("youtube-data-api-key")
        );
        assert_eq!(
            json["credentials"]["youtubeOauthClientId"].as_str(),
            Some("desktop-client.apps.googleusercontent.com")
        );
        assert_eq!(
            json["tts"]["options"]["bouyomiHost"].as_str(),
            Some("192.0.2.10")
        );
        assert_eq!(json["tts"]["options"]["bouyomiPort"].as_u64(), Some(50002));
        assert_eq!(json["tts"]["options"]["bouyomiSpeed"].as_i64(), Some(90));
        assert_eq!(
            json["tts"]["options"]["bouyomiVolume"].as_i64(),
            Some(80)
        );
        assert_eq!(json["tts"]["options"]["bouyomiTone"].as_i64(), Some(70));
        assert_eq!(json["tts"]["options"]["bouyomiVoice"].as_i64(), Some(2));
        assert_eq!(
            json["tts"]["options"]["bouyomiPath"].as_str(),
            Some(r"C:\BouyomiChan\BouyomiChan.exe")
        );
        assert_eq!(
            json["tts"]["options"]["voicevoxUrl"].as_str(),
            Some("http://127.0.0.1:50022")
        );
        assert_eq!(
            json["tts"]["options"]["voicevoxSpeaker"].as_u64(),
            Some(3)
        );
        assert!(
            (json["tts"]["options"]["webSpeechRate"]
                .as_f64()
                .expect("webSpeechRate")
                - 1.4)
                .abs()
                < 1e-6
        );
        assert!(
            (json["tts"]["options"]["webSpeechPitch"]
                .as_f64()
                .expect("webSpeechPitch")
                - 0.8)
                .abs()
                < 1e-6
        );
        assert!(
            (json["tts"]["options"]["webSpeechVolume"]
                .as_f64()
                .expect("webSpeechVolume")
                - 0.6)
                .abs()
                < 1e-6
        );
        assert_eq!(
            json["tts"]["options"]["webSpeechVoice"].as_str(),
            Some("Test Voice")
        );
        assert_eq!(json["tts"]["options"]["readName"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["omitUrl"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["stripEmoji"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["maxLength"].as_u64(), Some(280));
        assert_eq!(
            json["tts"]["options"]["dictionary"][0]["from"].as_str(),
            Some("fast")
        );
        assert_eq!(
            json["tts"]["options"]["dictionary"][0]["to"].as_str(),
            Some("ファスト")
        );
        assert_eq!(
            json["tts"]["options"]["dictionary"][1]["from"].as_str(),
            Some("comment")
        );
        assert_eq!(
            json["tts"]["options"]["dictionary"][1]["to"].as_str(),
            Some("コメント")
        );
        assert_eq!(
            json["youtubeOverrides"]["apiKey"].as_str(),
            Some("test-api-key")
        );
        assert_eq!(
            json["youtubeOverrides"]["clientVersion"].as_str(),
            Some("1.20240501.00.00")
        );
        assert_eq!(
            json["youtubeOverrides"]["paths"]["continuationPath"].as_str(),
            Some("contents.twoColumnWatchNextResults.conversationBar")
        );
        assert_eq!(
            json["xOverrides"]["bearerToken"].as_str(),
            Some("test-bearer")
        );
        assert_eq!(json["xOverrides"]["livePollSecs"].as_u64(), Some(45));

        let decoded: AppConfig = serde_json::from_str(&text).expect("deserialize AppConfig");
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn default_helpers_are_reflected_in_default_config() {
        // private な default helper の値が Default 実装へ反映されることを確認する。
        let cfg = AppConfig::default();

        assert!(cfg.channels.is_empty());
        assert_eq!(cfg.obs.port, default_obs_port());
        assert_eq!(cfg.obs.port, 11180);
        assert_eq!(cfg.obs.template, default_obs_template());
        assert_eq!(cfg.obs.template, "default");
        assert_eq!(cfg.obs.font_scale_pct, default_obs_font_scale_pct());
        assert_eq!(cfg.obs.font_scale_pct, 100);
        assert_eq!(
            cfg.obs.danmaku_font_size,
            default_obs_danmaku_font_size()
        );
        assert_eq!(cfg.obs.danmaku_font_size, 30);
        assert_eq!(cfg.obs.max_rows, default_obs_max_rows());
        assert_eq!(cfg.obs.max_rows, 8);
        assert_eq!(cfg.obs.ttl_ms, default_obs_ttl_ms());
        assert_eq!(cfg.obs.ttl_ms, 12000);
        assert_eq!(cfg.obs.bg_opacity_pct, default_obs_bg_opacity_pct());
        assert_eq!(cfg.obs.bg_opacity_pct, 0);
        assert_eq!(cfg.obs.position, default_obs_position());
        assert_eq!(cfg.obs.position, "bottom");
        assert_eq!(cfg.obs.show_platform, default_true());
        assert_eq!(cfg.goals, GoalsConfig::default());
        assert!(!cfg.goals.enabled);
        assert!(!cfg.goals.show_in_app);
        assert_eq!(cfg.goals.comments, 0);
        assert_eq!(cfg.goals.viewers, 0);
        assert_eq!(cfg.goals.likes, 0);
        assert_eq!(cfg.goals.reactions, 0);
        assert_eq!(cfg.timer, TimerConfig::default());
        assert!(!cfg.timer.enabled);
        assert_eq!(cfg.timer.default_duration_sec, default_timer_duration_sec());
        assert_eq!(cfg.timer.default_duration_sec, 300);
        assert_eq!(cfg.timer.mode, default_timer_mode());
        assert_eq!(cfg.timer.mode, "countdown");
        assert_eq!(cfg.effects, EffectsConfig::default());
        assert!(!cfg.effects.enabled);
        assert!(cfg.effects.rules.is_empty());
        assert_eq!(cfg.welcome, WelcomeConfig::default());
        assert!(!cfg.welcome.enabled);
        assert_eq!(cfg.welcome.greeting, default_welcome_greeting());
        assert_eq!(cfg.welcome.greeting, "{name} さん、いらっしゃい！");
        assert!(!cfg.welcome.tts);
        assert_eq!(cfg.welcome.emoji, default_welcome_emoji());
        assert_eq!(cfg.welcome.emoji, "👋");
        assert_eq!(cfg.welcome.count, default_welcome_count());
        assert_eq!(cfg.welcome.count, 8);
        assert_eq!(cfg.tts.backend, TtsBackendKind::WebSpeech);
        assert_eq!(cfg.tts.options.bouyomi_host, default_bouyomi_host());
        assert_eq!(cfg.tts.options.bouyomi_host, "127.0.0.1");
        assert_eq!(cfg.tts.options.bouyomi_port, default_bouyomi_port());
        assert_eq!(cfg.tts.options.bouyomi_port, 50001);
        assert_eq!(cfg.tts.options.bouyomi_speed, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_volume, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_tone, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_voice, 0);
        assert_eq!(cfg.tts.options.bouyomi_path, "");
        assert_eq!(cfg.tts.options.voicevox_url, default_voicevox_url());
        assert_eq!(cfg.tts.options.voicevox_url, "http://127.0.0.1:50021");
        assert_eq!(cfg.tts.options.voicevox_speaker, default_voicevox_speaker());
        assert_eq!(cfg.tts.options.voicevox_speaker, 1);
        assert_eq!(cfg.tts.options.web_speech_rate, default_web_speech_rate());
        assert_eq!(cfg.tts.options.web_speech_rate, 1.0);
        assert_eq!(cfg.tts.options.web_speech_pitch, default_web_speech_pitch());
        assert_eq!(cfg.tts.options.web_speech_pitch, 1.0);
        assert_eq!(cfg.tts.options.web_speech_volume, default_web_speech_volume());
        assert_eq!(cfg.tts.options.web_speech_volume, 1.0);
        assert_eq!(cfg.tts.options.web_speech_voice, default_web_speech_voice());
        assert_eq!(cfg.tts.options.web_speech_voice, "");
        assert_eq!(cfg.tts.options.read_name, default_true());
        assert_eq!(cfg.tts.options.omit_url, default_true());
        assert_eq!(cfg.tts.options.strip_emoji, default_true());
        assert_eq!(cfg.tts.options.max_length, default_max_read_len());
        assert_eq!(cfg.tts.options.max_length, 140);
        assert!(cfg.tts.options.dictionary.is_empty());
        assert!(cfg.moderation.ng_words.is_empty());
        assert!(cfg.moderation.ng_users.is_empty());
        assert!(cfg.moderation.highlights.is_empty());
        assert_eq!(cfg.ui.max_buffer, default_max_buffer());
        assert_eq!(cfg.ui.max_buffer, 2000);
        assert!(cfg.ui.youtube_member_name_green);
        assert!(cfg.ui.twitch_native_style);
        assert!(!cfg.ui.show_donation_panel);
        assert!(!cfg.ui.show_first_comment_panel);
        // 通知設定は旧 config(キー欠落)でも default に劣化する(後方互換)。
        assert!(!cfg.ui.notify_sound);
        assert_eq!(cfg.ui.notify_volume, default_notify_volume());
        assert_eq!(cfg.participation, ParticipationConfig::default());
        assert!(!cfg.participation.enabled);
        assert_eq!(cfg.participation.keyword, default_participation_keyword());
        assert_eq!(cfg.participation.max, 0);
        assert_eq!(cfg.credentials, CredentialsConfig::default());
        assert_eq!(cfg.credentials.twitch_oauth, "");
        assert_eq!(cfg.credentials.twitch_username, "");
        assert_eq!(cfg.credentials.twitch_client_id, "");
        assert_eq!(cfg.credentials.twitch_client_secret, "");
        assert_eq!(cfg.credentials.youtube_api_key, "");
        assert_eq!(cfg.credentials.youtube_oauth_client_id, "");
        assert_eq!(cfg.youtube_overrides, YoutubeOverrides::default());
        assert_eq!(cfg.x_overrides, XOverrides::default());
    }

    #[test]
    fn partial_legacy_config_is_backfilled_by_serde_defaults() {
        // 古い/部分的な config.json でも serde default で後方互換の既定値を補完する。
        let text = r#"{
            "channels": [
                {
                    "platform": "twitch",
                    "identifier": "example_channel"
                }
            ],
            "obs": {
                "port": 12345
            },
            "tts": {
                "backend": "bouyomi",
                "options": {
                    "bouyomiPort": 50003,
                    "readName": false
                }
            },
            "ui": {
                "maxBuffer": 321
            }
        }"#;

        let cfg: AppConfig = serde_json::from_str(text).expect("partial legacy config");

        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.channels[0].platform, ChannelPlatform::Twitch);
        assert_eq!(cfg.channels[0].identifier, "example_channel");
        assert_eq!(cfg.channels[0].enabled, default_true());
        assert_eq!(cfg.obs.port, 12345);
        assert_eq!(cfg.obs.template, default_obs_template());
        assert_eq!(cfg.obs.font_scale_pct, default_obs_font_scale_pct());
        assert_eq!(cfg.obs.max_rows, default_obs_max_rows());
        assert_eq!(cfg.obs.ttl_ms, default_obs_ttl_ms());
        assert_eq!(cfg.obs.bg_opacity_pct, default_obs_bg_opacity_pct());
        assert_eq!(cfg.obs.position, default_obs_position());
        assert_eq!(cfg.obs.show_platform, default_true());
        assert_eq!(cfg.goals, GoalsConfig::default());
        assert_eq!(cfg.timer, TimerConfig::default());
        assert_eq!(cfg.effects, EffectsConfig::default());
        assert_eq!(cfg.welcome, WelcomeConfig::default());
        assert_eq!(cfg.tts.backend, TtsBackendKind::Bouyomi);
        assert_eq!(cfg.tts.options.bouyomi_host, default_bouyomi_host());
        assert_eq!(cfg.tts.options.bouyomi_port, 50003);
        assert_eq!(cfg.tts.options.bouyomi_speed, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_volume, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_tone, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_voice, 0);
        assert_eq!(cfg.tts.options.bouyomi_path, "");
        assert_eq!(cfg.tts.options.voicevox_url, default_voicevox_url());
        assert_eq!(cfg.tts.options.voicevox_speaker, default_voicevox_speaker());
        assert_eq!(cfg.tts.options.web_speech_rate, default_web_speech_rate());
        assert_eq!(cfg.tts.options.web_speech_pitch, default_web_speech_pitch());
        assert_eq!(cfg.tts.options.web_speech_volume, default_web_speech_volume());
        assert_eq!(cfg.tts.options.web_speech_voice, default_web_speech_voice());
        assert!(!cfg.tts.options.read_name);
        assert_eq!(cfg.tts.options.omit_url, default_true());
        assert_eq!(cfg.tts.options.strip_emoji, default_true());
        assert_eq!(cfg.tts.options.max_length, default_max_read_len());
        assert!(cfg.tts.options.dictionary.is_empty());
        assert_eq!(cfg.ui.max_buffer, 321);
        assert!(cfg.ui.youtube_member_name_green);
        assert!(cfg.ui.twitch_native_style);
        assert!(!cfg.ui.show_donation_panel);
        assert!(!cfg.ui.show_first_comment_panel);
        assert_eq!(cfg.participation, ParticipationConfig::default());
        assert_eq!(cfg.credentials, CredentialsConfig::default());
        assert_eq!(cfg.youtube_overrides, YoutubeOverrides::default());
        assert_eq!(cfg.x_overrides, XOverrides::default());
    }

    #[test]
    fn tts_dictionary_defaults_and_roundtrips() {
        let legacy: TtsOptions = serde_json::from_str(
            r#"{
                "readName": false
            }"#,
        )
        .expect("deserialize legacy tts options");
        assert!(legacy.dictionary.is_empty());

        let options = TtsOptions {
            dictionary: vec![
                TtsDictEntry {
                    regex: false,
                    from: "FC".to_string(),
                    to: "ファストコメント".to_string(),
                },
                TtsDictEntry {
                    regex: false,
                    from: "".to_string(),
                    to: "ignored".to_string(),
                },
            ],
            ..TtsOptions::default()
        };

        let text = serde_json::to_string(&options).expect("serialize tts options");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse tts options json");
        assert_eq!(json["dictionary"][0]["from"].as_str(), Some("FC"));
        assert_eq!(
            json["dictionary"][0]["to"].as_str(),
            Some("ファストコメント")
        );
        assert_eq!(json["dictionary"][1]["from"].as_str(), Some(""));
        assert_eq!(json["dictionary"][1]["to"].as_str(), Some("ignored"));

        let decoded: TtsOptions = serde_json::from_str(&text).expect("deserialize tts options");
        assert_eq!(decoded, options);
    }

    #[test]
    fn tts_extended_options_roundtrip_and_partial_kind_defaults() {
        let options: TtsOptions = serde_json::from_str(r#"{
            "nameFormat": "{name} ({rawName}) {label}",
            "excludeUsers": ["*bot", "User"],
            "readKinds": {"superChat": false, "system": false},
            "skipCommands": true,
            "dictionary": [{"from": "(hello)", "to": "$1!", "regex": true}]
        }"#).unwrap();
        assert_eq!(options.name_format, "{name} ({rawName}) {label}");
        assert_eq!(options.exclude_users, vec!["*bot", "User"]);
        assert!(!options.read_kinds.super_chat);
        assert!(!options.read_kinds.system);
        assert!(options.read_kinds.normal);
        assert!(options.read_kinds.membership);
        assert!(options.read_kinds.bits);
        assert!(options.read_kinds.gift);
        assert!(options.skip_commands);
        assert!(options.dictionary[0].regex);
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(json["readKinds"]["superChat"], false);
        assert_eq!(json["nameFormat"], options.name_format);
        assert_eq!(json["excludeUsers"][0], "*bot");
        assert_eq!(json["skipCommands"], true);
        assert_eq!(json["dictionary"][0]["regex"], true);
        assert_eq!(serde_json::from_value::<TtsOptions>(json).unwrap(), options);
    }

    #[test]
    fn tts_pack_legacy_config_gets_defaults() {
        let o: TtsOptions = serde_json::from_str(r#"{"readName": false}"#).unwrap();
        assert_eq!(o.voicevox_engine, VoicevoxEngineKind::Voicevox);
        assert_eq!(o.voicevox_params, VoicevoxParams::default());
        assert!(o.voicevox_params.speed_scale.is_none());
        assert!(!o.auto_speed.enabled);
        assert_eq!(o.queue.max_pending, 64);
        assert_eq!(o.queue.max_wait_sec, 0);
        assert_eq!(o.templates.all, "{name} {text}");
        assert!(o.templates.super_chat.is_none());
    }

    #[test]
    fn tts_pack_roundtrip_camel_case() {
        let mut o = TtsOptions::default();
        o.voicevox_engine = VoicevoxEngineKind::AivisSpeech;
        o.voicevox_params.speed_scale = Some(1.25);
        o.auto_speed.enabled = true;
        o.auto_speed.queue_threshold = 3;
        o.queue.max_pending = 10;
        o.templates.super_chat = Some("{name} {paidText}".to_string());
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["voicevoxEngine"], "aivisSpeech");
        assert_eq!(json["voicevoxParams"]["speedScale"], 1.25);
        assert_eq!(json["autoSpeed"]["queueThreshold"], 3);
        assert_eq!(json["autoSpeed"]["enabled"], true);
        assert_eq!(json["queue"]["maxPending"], 10);
        assert_eq!(json["templates"]["superChat"], "{name} {paidText}");
        assert_eq!(serde_json::from_value::<TtsOptions>(json).unwrap(), o);
    }

    #[test]
    fn tts_pack_unknown_engine_falls_back_to_voicevox() {
        for raw in [r#""coeiroink""#, "42", "null", r#""""#] {
            let o: TtsOptions =
                serde_json::from_str(&format!(r#"{{"voicevoxEngine": {raw}}}"#)).unwrap();
            assert_eq!(o.voicevox_engine, VoicevoxEngineKind::Voicevox, "{raw}");
        }
    }

    #[test]
    fn tts_pack_normalize_all_clamps_update_path() {
        // update_config は保存前に normalize_all を呼ぶ。ロードを経ずに範囲外値が丸まること。
        let mut c = AppConfig::default();
        c.tts.options.voicevox_params.speed_scale = Some(9.0);
        c.tts.options.queue.max_pending = 0;
        c.normalize_all();
        assert_eq!(c.tts.options.voicevox_params.speed_scale, Some(2.0));
        assert!(c.tts.options.queue.max_pending >= 1);
    }

    #[test]
    fn tts_pack_normalize_clamps_out_of_range() {
        let mut o: TtsOptions = serde_json::from_str(
            r#"{
                "voicevoxParams": {"speedScale": 9, "pitchScale": -5, "intonationScale": -1,
                    "volumeScale": 99, "prePhonemeLength": -2, "postPhonemeLength": 50},
                "autoSpeed": {"maxSpeed": 10, "perQueued": -1, "perTenChars": 5,
                    "queueThreshold": 100000, "lengthThreshold": 100000},
                "queue": {"maxPending": 0, "maxWaitSec": 4000000000}
            }"#,
        )
        .unwrap();
        o.normalize();
        assert_eq!(o.voicevox_params.speed_scale, Some(2.0));
        assert_eq!(o.voicevox_params.pitch_scale, Some(-0.15));
        assert_eq!(o.voicevox_params.intonation_scale, Some(0.0));
        assert_eq!(o.voicevox_params.volume_scale, Some(2.0));
        assert_eq!(o.voicevox_params.pre_phoneme_length, Some(0.0));
        assert_eq!(o.voicevox_params.post_phoneme_length, Some(1.5));
        assert_eq!(o.auto_speed.max_speed, 2.0);
        assert_eq!(o.auto_speed.per_queued, 0.0);
        assert_eq!(o.auto_speed.per_ten_chars, 1.0);
        assert_eq!(o.auto_speed.queue_threshold, 500);
        assert_eq!(o.auto_speed.length_threshold, 1000);
        assert_eq!(o.queue.max_pending, 1);
        assert_eq!(o.queue.max_wait_sec, 86_400);
        o.queue.max_pending = 9999;
        o.normalize();
        assert_eq!(o.queue.max_pending, 500);
        // 既定値は normalize で変わらない。
        let mut d = TtsOptions::default();
        d.normalize();
        assert_eq!(d, TtsOptions::default());
    }

    #[test]
    fn tts_pack_template_null_vs_empty() {
        let t: TtsTemplates =
            serde_json::from_str(r#"{"normal": null, "bits": "", "gift": "x"}"#).unwrap();
        assert_eq!(t.normal, None);
        assert_eq!(t.super_chat, None);
        assert_eq!(t.bits, Some(String::new()));
        assert_eq!(t.gift.as_deref(), Some("x"));
        assert_eq!(t.all, "{name} {text}");
        let json = serde_json::to_value(&t).unwrap();
        assert!(json["normal"].is_null());
        assert_eq!(json["bits"], "");
    }

    #[test]
    fn goals_show_in_app_defaults_false_and_roundtrips() {
        let legacy_goals: GoalsConfig = serde_json::from_str(
            r#"{
                "enabled": true,
                "comments": 10,
                "viewers": 20,
                "likes": 30
            }"#,
        )
        .expect("deserialize legacy goals config");
        assert!(legacy_goals.enabled);
        assert!(!legacy_goals.show_in_app);
        assert_eq!(legacy_goals.layout, "horizontal");
        assert_eq!(legacy_goals.skin, "glass");
        assert_eq!(legacy_goals.show_comments, None);
        assert_eq!(legacy_goals.show_viewers, None);
        assert_eq!(legacy_goals.show_likes, None);
        assert_eq!(legacy_goals.show_reactions, None);
        assert_eq!(legacy_goals.comments, 10);
        assert_eq!(legacy_goals.viewers, 20);
        assert_eq!(legacy_goals.likes, 30);
        assert_eq!(legacy_goals.reactions, 0);

        let cfg = GoalsConfig {
            enabled: true,
            show_in_app: true,
            layout: "vertical".to_string(),
            skin: "minimal".to_string(),
            show_comments: Some(false),
            show_viewers: Some(true),
            show_likes: Some(false),
            show_reactions: Some(true),
            comments: 100,
            viewers: 50,
            likes: 25,
            reactions: 12,
        };
        let text = serde_json::to_string(&cfg).expect("serialize goals config");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse goals json");
        assert_eq!(json["showInApp"].as_bool(), Some(true));
        assert_eq!(json["layout"].as_str(), Some("vertical"));
        assert_eq!(json["skin"].as_str(), Some("minimal"));
        assert_eq!(json["showComments"].as_bool(), Some(false));
        assert_eq!(json["reactions"].as_u64(), Some(12));

        let decoded: GoalsConfig = serde_json::from_str(&text).expect("deserialize goals config");
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn effects_config_defaults_and_roundtrips() {
        let legacy_effects: EffectsConfig = serde_json::from_str(
            r#"{
                "enabled": true,
                "rules": [
                    {
                        "keyword": "nice",
                        "emoji": "✨"
                    }
                ]
            }"#,
        )
        .expect("deserialize legacy effects config");
        assert!(legacy_effects.enabled);
        assert_eq!(legacy_effects.rules.len(), 1);
        assert_eq!(legacy_effects.rules[0].keyword, "nice");
        assert_eq!(legacy_effects.rules[0].emoji, "✨");
        assert_eq!(legacy_effects.rules[0].count, default_effect_count());

        let cfg = EffectsConfig {
            enabled: true,
            rules: vec![EffectRule {
                keyword: "party".to_string(),
                emoji: "🎉".to_string(),
                count: 24,
                ..Default::default()
            }],
        };
        let text = serde_json::to_string(&cfg).expect("serialize effects config");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse effects json");
        assert_eq!(json["enabled"].as_bool(), Some(true));
        assert_eq!(json["rules"][0]["keyword"].as_str(), Some("party"));
        assert_eq!(json["rules"][0]["emoji"].as_str(), Some("🎉"));
        assert_eq!(json["rules"][0]["count"].as_u64(), Some(24));

        let decoded: EffectsConfig =
            serde_json::from_str(&text).expect("deserialize effects config");
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn legacy_effect_rules_load_as_app_target_with_defaults() {
        let legacy: EffectsConfig = serde_json::from_str(
            r#"{"enabled":true,"rules":[{"keyword":"nice","emoji":"✨","count":7}]}"#,
        )
        .expect("legacy rule");
        let rule = &legacy.rules[0];
        assert_eq!(rule.target, EffectTarget::App);
        assert!(rule.target.includes_app() && !rule.target.includes_obs());
        assert_eq!(rule.count, 7);
        assert_eq!(rule.animation, "float");
        assert_eq!(rule.duration_ms, 3000);
        assert_eq!(rule.cooldown_ms, 2000);

        // AppConfig 全体でも、effects の新項目が無い旧 config.json が読める。
        let cfg: AppConfig = serde_json::from_str(
            r#"{"effects":{"enabled":true,"rules":[{"keyword":"a","emoji":"b"}]}}"#,
        )
        .expect("legacy config");
        assert_eq!(cfg.effects.rules[0].target, EffectTarget::App);
        assert_eq!(cfg.effects.rules[0].count, 12);
    }

    #[test]
    fn legacy_count_zero_rule_loads_disabled_with_count_one() {
        let legacy: EffectsConfig = serde_json::from_str(
            r#"{"enabled":true,"rules":[
                {"keyword":"off","emoji":"x","count":0},
                {"keyword":"on","emoji":"y","count":3},
                {"keyword":"new","emoji":"z"}]}"#,
        )
        .unwrap();
        assert!(!legacy.rules[0].enabled);
        assert_eq!(legacy.rules[0].count, 1);
        assert!(legacy.rules[1].enabled);
        assert!(legacy.rules[2].enabled);
        // enabled を明示した規則はそのまま。保存し直しても無効のまま。
        let text = serde_json::to_string(&legacy).unwrap();
        let again: EffectsConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(again, legacy);
        let explicit: EffectRule =
            serde_json::from_str(r#"{"keyword":"a","emoji":"b","count":0,"enabled":true}"#).unwrap();
        assert!(explicit.enabled);
        assert_eq!(explicit.count, 1);
    }

    #[test]
    fn effect_keywords_are_trimmed_on_load_and_normalize() {
        let legacy: EffectsConfig = serde_json::from_str(
            r#"{"enabled":true,"rules":[{"keyword":" 拍手 ","emoji":"x"}]}"#,
        )
        .unwrap();
        assert_eq!(legacy.rules[0].keyword, "拍手");
        let mut rule = EffectRule { keyword: "\t拍手\u{3000}".into(), ..Default::default() };
        rule.normalize();
        assert_eq!(rule.keyword, "拍手");
    }

    #[test]
    fn effect_target_parses_values_and_falls_back_to_app() {
        for (raw, want) in [
            ("app", EffectTarget::App),
            ("obs", EffectTarget::Obs),
            ("both", EffectTarget::Both),
            ("bogus", EffectTarget::App),
        ] {
            let rule: EffectRule =
                serde_json::from_str(&format!(r#"{{"keyword":"a","emoji":"b","target":"{raw}"}}"#))
                    .unwrap();
            assert_eq!(rule.target, want, "{raw}");
        }
        assert!(EffectTarget::Both.includes_app() && EffectTarget::Both.includes_obs());
        assert!(EffectTarget::Obs.includes_obs() && !EffectTarget::Obs.includes_app());
        let json = serde_json::to_value(EffectRule { target: EffectTarget::Obs, ..Default::default() })
            .unwrap();
        assert_eq!(json["target"], "obs");
        assert_eq!(json["durationMs"], 3000);
        assert_eq!(json["cooldownMs"], 2000);
    }

    #[test]
    fn effect_rule_normalize_rounds_out_of_range_values() {
        let mut rule = EffectRule {
            count: 0,
            duration_ms: 10,
            cooldown_ms: 999_999,
            animation: "zigzag".into(),
            emoji: "x".repeat(40),
            ..Default::default()
        };
        rule.normalize();
        assert_eq!((rule.count, rule.duration_ms, rule.cooldown_ms), (1, 500, 60_000));
        assert_eq!(rule.animation, "float");
        assert_eq!(rule.emoji.chars().count(), 16);

        let mut rule = EffectRule { count: 500, duration_ms: 1_000_000, ..Default::default() };
        rule.normalize();
        assert_eq!((rule.count, rule.duration_ms), (50, 15_000));

        // 範囲内・既知のプリセットは変えない。
        let mut rule = EffectRule {
            count: 20,
            duration_ms: 800,
            cooldown_ms: 0,
            animation: "burst".into(),
            ..Default::default()
        };
        let before = rule.clone();
        rule.normalize();
        assert_eq!(rule, before);
    }

    #[test]
    fn normalize_all_rounds_effect_rules() {
        let mut cfg = AppConfig::default();
        cfg.effects.rules.push(EffectRule { count: 900, ..Default::default() });
        cfg.normalize_all();
        assert_eq!(cfg.effects.rules[0].count, 50);
    }

    #[test]
    fn welcome_config_defaults_and_roundtrips() {
        let legacy_welcome: WelcomeConfig = serde_json::from_str(
            r#"{
                "enabled": true
            }"#,
        )
        .expect("deserialize legacy welcome config");
        assert!(legacy_welcome.enabled);
        assert_eq!(legacy_welcome.greeting, default_welcome_greeting());
        assert!(!legacy_welcome.tts);
        assert_eq!(legacy_welcome.emoji, default_welcome_emoji());
        assert_eq!(legacy_welcome.count, default_welcome_count());

        let cfg = WelcomeConfig {
            enabled: true,
            greeting: "こんにちは、{name}さん".to_string(),
            tts: true,
            emoji: "👏".to_string(),
            count: 18,
        };
        let text = serde_json::to_string(&cfg).expect("serialize welcome config");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse welcome json");
        assert_eq!(json["enabled"].as_bool(), Some(true));
        assert_eq!(json["greeting"].as_str(), Some("こんにちは、{name}さん"));
        assert_eq!(json["tts"].as_bool(), Some(true));
        assert_eq!(json["emoji"].as_str(), Some("👏"));
        assert_eq!(json["count"].as_u64(), Some(18));

        let decoded: WelcomeConfig =
            serde_json::from_str(&text).expect("deserialize welcome config");
        assert_eq!(decoded, cfg);
    }

    #[test]
    fn youtube_override_paths_accept_empty_and_preserve_values() {
        // paths が空でも壊れず、指定したキー名と文字列値は往復で保持される。
        let empty_text = r#"{
            "youtubeOverrides": {
                "paths": {}
            }
        }"#;
        let empty_cfg: AppConfig = serde_json::from_str(empty_text).expect("empty override paths");
        assert!(empty_cfg.youtube_overrides.paths.is_empty());

        let specified_text = r#"{
            "youtubeOverrides": {
                "paths": {
                    "apiKey": "ytInitialPlayerResponse.args.innertubeApiKey",
                    "continuation": "contents.liveChatRenderer.continuations.0"
                }
            }
        }"#;
        let specified_cfg: AppConfig =
            serde_json::from_str(specified_text).expect("specified override paths");
        assert_eq!(
            specified_cfg
                .youtube_overrides
                .paths
                .get("apiKey")
                .map(String::as_str),
            Some("ytInitialPlayerResponse.args.innertubeApiKey")
        );
        assert_eq!(
            specified_cfg
                .youtube_overrides
                .paths
                .get("continuation")
                .map(String::as_str),
            Some("contents.liveChatRenderer.continuations.0")
        );

        let text = serde_json::to_string(&specified_cfg).expect("serialize override paths");
        let decoded: AppConfig = serde_json::from_str(&text).expect("deserialize override paths");
        assert_eq!(decoded.youtube_overrides.paths, specified_cfg.youtube_overrides.paths);
    }
    fn row(platform: ChannelPlatform, identifier: &str, overrides: ChannelOverrides) -> ChannelConfig {
        ChannelConfig {
            platform,
            identifier: identifier.into(),
            enabled: true,
            own_stream: false,
            overrides,
        }
    }

    fn disabled_tts() -> ChannelOverrides {
        ChannelOverrides {
            tts_enabled: Some(false),
            ..Default::default()
        }
    }

    #[test]
    fn replace_channels_in_removes_all_old_rows_and_inherits_first_overrides() {
        let mut cfg = AppConfig::default();
        cfg.channels = vec![
            row(ChannelPlatform::Niconico, "lv1", disabled_tts()),
            row(ChannelPlatform::Twitch, "keep", ChannelOverrides::default()),
            row(ChannelPlatform::Niconico, "lv2", ChannelOverrides::default()),
        ];
        let keys = ["niconico:lv1".to_string(), "niconico:lv2".to_string()];
        replace_channels_in(&mut cfg, &keys, row(ChannelPlatform::Niconico, "lv3", ChannelOverrides::default()))
            .unwrap();
        let ids: Vec<_> = cfg.channels.iter().map(|c| c.identifier.as_str()).collect();
        assert_eq!(ids, ["lv3", "keep"]);
        assert_eq!(cfg.channels[0].overrides, disabled_tts());

        // もう無い旧 key は無視し、残っている旧行を置き換える。
        let missing = ["niconico:gone".to_string(), "niconico:lv3".to_string()];
        replace_channels_in(&mut cfg, &missing, row(ChannelPlatform::Niconico, "lv4", ChannelOverrides::default()))
            .unwrap();
        let ids: Vec<_> = cfg.channels.iter().map(|c| c.identifier.as_str()).collect();
        assert_eq!(ids, ["lv4", "keep"]);
        assert_eq!(cfg.channels[0].overrides, disabled_tts());
    }

    #[test]
    fn replace_channels_in_with_no_existing_old_key_appends_without_overrides() {
        let mut cfg = AppConfig::default();
        cfg.channels = vec![row(ChannelPlatform::Twitch, "keep", disabled_tts())];
        let gone = ["niconico:gone".to_string()];
        replace_channels_in(&mut cfg, &gone, row(ChannelPlatform::Niconico, "lv5", ChannelOverrides::default()))
            .unwrap();
        let ids: Vec<_> = cfg.channels.iter().map(|c| c.identifier.as_str()).collect();
        assert_eq!(ids, ["keep", "lv5"]);
        assert!(cfg.channels[1].overrides.is_empty());
        // 末尾追加でも既存行との重複は弾く。
        assert_eq!(
            replace_channels_in(&mut cfg, &gone, row(ChannelPlatform::Twitch, "keep", ChannelOverrides::default())),
            Err(ChannelEditError::Duplicate)
        );
        // 1 件指定の公開関数は従来どおり NotFound。
        assert_eq!(
            replace_channel_in(&mut cfg, "niconico:gone", row(ChannelPlatform::Niconico, "lv6", ChannelOverrides::default())),
            Err(ChannelEditError::NotFound)
        );
    }

    #[test]
    fn replace_channel_in_normalizes_new_overrides_before_carry_over() {
        let mut cfg = AppConfig::default();
        cfg.channels = vec![row(ChannelPlatform::Twitch, "old", disabled_tts())];
        // ttsEnabled: true と空白だけの声は正規化で空になるので、旧行の上書きを引き継ぐ。
        let new = row(
            ChannelPlatform::Twitch,
            "new",
            ChannelOverrides {
                tts_enabled: Some(true),
                tts_voice: Some(ChannelTtsVoice {
                    web_speech_voice: Some("  ".into()),
                    ..Default::default()
                }),
                sound_enabled: Some(true),
            },
        );
        replace_channel_in(&mut cfg, "twitch:old", new).unwrap();
        assert_eq!(cfg.channels[0].overrides, disabled_tts());
    }

    #[test]
    fn web_speech_voice_is_capped_and_invisible_only_names_are_empty() {
        let voice = |name: String| {
            let mut o = ChannelOverrides {
                tts_voice: Some(ChannelTtsVoice {
                    web_speech_voice: Some(name),
                    ..Default::default()
                }),
                ..Default::default()
            };
            o.normalize();
            o.tts_voice.and_then(|v| v.web_speech_voice)
        };
        for invisible in ["\u{200B}", "\u{FEFF}", " \u{2060}\u{200B} ", "\u{200D}\u{00AD}"] {
            assert_eq!(voice(invisible.to_string()), None, "{invisible:?}");
        }
        let long = format!("  {}  ", "あ".repeat(MAX_WEB_SPEECH_VOICE_CHARS + 50));
        assert_eq!(voice(long).unwrap().chars().count(), MAX_WEB_SPEECH_VOICE_CHARS);
        assert_eq!(voice(" \u{200B}Kyoko ".into()).as_deref(), Some("\u{200B}Kyoko"));
    }

    #[test]
    fn broken_config_is_preserved_before_default_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = AppConfig::path_in(dir.path());
        std::fs::write(&path, b"{ broken").unwrap();
        // 同じ ms の退避ファイルが既にあっても上書きしない。
        let existing = dir.path().join("config.json.broken-42");
        std::fs::write(&existing, b"older").unwrap();

        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 42), (AppConfig::default(), ConfigLoadOutcome::BackedUp));
        assert_eq!(std::fs::read(&existing).unwrap(), b"older");
        assert_eq!(std::fs::read(dir.path().join("config.json.broken-42-1")).unwrap(), b"{ broken");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ broken");

        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 43), (AppConfig::default(), ConfigLoadOutcome::BackedUp));
        assert_eq!(std::fs::read(dir.path().join("config.json.broken-43")).unwrap(), b"{ broken");
    }

    #[test]
    fn backup_preserves_all_bytes_and_leaves_existing_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = AppConfig::path_in(dir.path());
        let bytes = b"\xff\0broken config".repeat(4096);
        std::fs::write(&path, &bytes).unwrap();
        let old_backup = dir.path().join("config.json.broken-42");
        let old_tmp = dir.path().join("config.json.broken-42.tmp");
        std::fs::write(&old_backup, b"older backup").unwrap();
        std::fs::write(&old_tmp, b"older temporary file").unwrap();

        assert_eq!(
            AppConfig::load_or_preserve_at(dir.path(), 42),
            (AppConfig::default(), ConfigLoadOutcome::BackedUp)
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::read(dir.path().join("config.json.broken-42-1")).unwrap(),
            bytes
        );
        assert_eq!(std::fs::read(&old_backup).unwrap(), b"older backup");
        assert_eq!(std::fs::read(&old_tmp).unwrap(), b"older temporary file");
    }

    #[test]
    fn missing_or_valid_config_is_not_preserved() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 1), (AppConfig::default(), ConfigLoadOutcome::Loaded));
        AppConfig::default().save(dir.path()).unwrap();
        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 2).1, ConfigLoadOutcome::Loaded);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".broken-")), "{names:?}");
    }

    #[test]
    fn config_metadata_error_blocks_saves() {
        let dir = tempfile::tempdir().unwrap();
        let invalid_dir = dir.path().join("invalid\0name");
        assert_eq!(
            std::fs::metadata(AppConfig::path_in(&invalid_dir)).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert!(AppConfig::load(&invalid_dir).is_err());
        assert_eq!(
            AppConfig::load_or_preserve_at(&invalid_dir, 42),
            (AppConfig::default(), ConfigLoadOutcome::SavesBlocked)
        );
        let err = AppConfig::default().save(&invalid_dir).unwrap_err();
        assert_eq!(err.to_string(), CONFIG_SAVE_BLOCKED_ERROR);
    }

    #[test]
    fn only_newest_five_broken_backups_are_kept_after_a_new_backup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(AppConfig::path_in(dir.path()), b"{ broken").unwrap();
        for ms in [10, 20, 30, 40, 50, 60] {
            std::fs::write(dir.path().join(format!("config.json.broken-{ms}")), b"old").unwrap();
        }
        std::fs::write(dir.path().join("config.json.broken-60-1"), b"old").unwrap();
        // 退避名の形でない物は消さない。
        std::fs::write(dir.path().join("config.json.broken-1.tmp"), b"tmp").unwrap();
        std::fs::write(dir.path().join("other.json.broken-1"), b"other").unwrap();

        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 70).1, ConfigLoadOutcome::BackedUp);
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "config.json",
                "config.json.broken-1.tmp",
                "config.json.broken-40",
                "config.json.broken-50",
                "config.json.broken-60",
                "config.json.broken-60-1",
                "config.json.broken-70",
                "other.json.broken-1",
            ]
        );
    }

    #[test]
    fn pruning_ignores_noncanonical_names_and_nonregular_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = AppConfig::path_in(dir.path());
        std::fs::write(&path, b"{ broken").unwrap();
        let backups = [20, 30, 40, 50].map(|ms| {
            let backup = dir.path().join(format!("config.json.broken-{ms}"));
            std::fs::write(&backup, b"old").unwrap();
            backup
        });
        let unrelated = ["1-0", "01", "2-01", "+3", "4-+1", "5-"].map(|suffix| {
            let backup = dir.path().join(format!("config.json.broken-{suffix}"));
            std::fs::write(&backup, b"unrelated").unwrap();
            backup
        });
        let directory = dir.path().join("config.json.broken-90");
        std::fs::create_dir(&directory).unwrap();
        let link = dir.path().join("config.json.broken-100");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&path, &link).unwrap();
        #[cfg(windows)]
        if let Err(err) = std::os::windows::fs::symlink_file(&path, &link) {
            // Windows では開発者モードまたはシンボリックリンク作成権限が必要。
            assert_eq!(err.raw_os_error(), Some(1314), "{err}");
        }
        let link_existed = link.is_symlink();

        assert_eq!(
            AppConfig::load_or_preserve_at(dir.path(), 60).1,
            ConfigLoadOutcome::BackedUp
        );
        assert_eq!(
            std::fs::read(dir.path().join("config.json.broken-60")).unwrap(),
            b"{ broken"
        );
        for backup in backups {
            assert_eq!(std::fs::read(backup).unwrap(), b"old");
        }
        for backup in unrelated {
            assert_eq!(std::fs::read(backup).unwrap(), b"unrelated");
        }
        assert!(directory.is_dir());
        assert_eq!(link.is_symlink(), link_existed);
    }

    #[test]
    fn failed_backup_does_not_prune_old_backups() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(AppConfig::path_in(dir.path())).unwrap();
        for ms in 1..=7 {
            std::fs::write(dir.path().join(format!("config.json.broken-{ms}")), b"old").unwrap();
        }
        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 9).1, ConfigLoadOutcome::SavesBlocked);
        let count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".broken-"))
            .count();
        assert_eq!(count, 7);
    }

    #[test]
    fn read_error_is_retried_once_but_second_failure_is_returned() {
        let path = Path::new("config.json");
        let mut calls = 0;
        let ok = read_with_retry(path, std::time::Duration::ZERO, |_| {
            calls += 1;
            if calls == 1 {
                Err(std::io::Error::from(ErrorKind::PermissionDenied))
            } else {
                Ok("{}")
            }
        });
        assert_eq!(ok.unwrap(), "{}");
        assert_eq!(calls, 2);

        let mut calls = 0;
        let err = read_with_retry::<()>(path, std::time::Duration::ZERO, |_| {
            calls += 1;
            Err(std::io::Error::from(ErrorKind::PermissionDenied))
        });
        assert_eq!(err.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(calls, 2);
    }

    #[test]
    fn config_existence_error_is_retried_once() {
        let path = Path::new("config.json");
        let delay = std::time::Duration::ZERO;
        let mut checks = 0;
        let text = read_config_text(
            path,
            delay,
            |_| {
                checks += 1;
                if checks == 1 {
                    Err(ErrorKind::PermissionDenied.into())
                } else {
                    Ok(true)
                }
            },
            |_| Ok("{}".to_owned()),
        );
        assert_eq!(text.unwrap().as_deref(), Some("{}"));
        assert_eq!(checks, 2);

        let mut checks = 0;
        let err = read_config_text(
            path,
            delay,
            |_| {
                checks += 1;
                Err(ErrorKind::PermissionDenied.into())
            },
            |_| panic!("存在を確認できないファイルを読み込んだ"),
        );
        assert_eq!(err.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(checks, 2);
    }

    #[test]
    fn load_outcome_notices_are_fixed_text() {
        assert_eq!(ConfigLoadOutcome::Loaded.notice(), None);
        assert_eq!(
            ConfigLoadOutcome::BackedUp.notice(),
            Some("設定ファイルが読めなかったため控えを残して初期設定で起動しました")
        );
        assert_eq!(
            ConfigLoadOutcome::SavesBlocked.notice(),
            Some("設定ファイルを読めず控えも作れなかったため、保存を止めています")
        );
    }

    #[test]
    fn null_overrides_load_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            AppConfig::path_in(dir.path()),
            br#"{"channels":[{"platform":"twitch","identifier":"alice","overrides":null}]}"#,
        )
        .unwrap();
        let cfg = AppConfig::load(dir.path()).unwrap();
        assert!(cfg.channels[0].overrides.is_empty());
    }

    #[test]
    fn many_existing_backup_names_never_block_preservation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(AppConfig::path_in(dir.path()), b"{ broken").unwrap();
        std::fs::write(dir.path().join("config.json.broken-7"), b"old").unwrap();
        for n in 1..=100 {
            std::fs::write(dir.path().join(format!("config.json.broken-7-{n}")), b"old").unwrap();
        }
        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 7), (AppConfig::default(), ConfigLoadOutcome::BackedUp));
        assert_eq!(std::fs::read(dir.path().join("config.json.broken-7-101")).unwrap(), b"{ broken");
        assert_eq!(std::fs::read(dir.path().join("config.json.broken-7-100")).unwrap(), b"old");
        AppConfig::default().save(dir.path()).expect("退避できたので保存できる");
    }

    #[test]
    fn unreadable_config_leaves_no_partial_backup_and_blocks_saves() {
        let dir = tempfile::tempdir().unwrap();
        // config.json がディレクトリだと読めず、退避もできない。
        std::fs::create_dir(AppConfig::path_in(dir.path())).unwrap();
        assert_eq!(AppConfig::load_or_preserve_at(dir.path(), 9), (AppConfig::default(), ConfigLoadOutcome::SavesBlocked));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["config.json"]);

        let err = AppConfig::default().save(dir.path()).unwrap_err();
        assert_eq!(err.to_string(), CONFIG_SAVE_BLOCKED_ERROR);
        let revision = AtomicU64::new(0);
        let (tx, _rx) = tokio::sync::watch::channel(AppConfig::default());
        let mut current = AppConfig::default();
        current.channels.push(ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "a".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides::default(),
        });
        let mut next = current.clone();
        next.obs.max_rows = 3;
        let err = commit_config(dir.path(), &revision, &tx, &mut current, next, None).unwrap_err();
        assert_eq!(err.to_string(), CONFIG_SAVE_BLOCKED_ERROR);
        assert_eq!(revision.load(Ordering::SeqCst), 0);
        let err = commit_channel_overrides(
            dir.path(),
            &revision,
            &tx,
            &mut current,
            "twitch:a",
            ChannelOverrides { tts_enabled: Some(false), ..Default::default() },
            0,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), CONFIG_SAVE_BLOCKED_ERROR);
    }

    #[test]
    fn sound_enabled_override_stores_only_false() {
        let mut o = ChannelOverrides { sound_enabled: Some(true), ..Default::default() };
        o.normalize();
        assert!(o.is_empty());
        let mut off = ChannelOverrides { sound_enabled: Some(false), ..Default::default() };
        off.normalize();
        assert!(!off.is_empty());
        assert_eq!(serde_json::to_string(&off).unwrap(), r#"{"soundEnabled":false}"#);
        assert_eq!(serde_json::to_string(&ChannelOverrides::default()).unwrap(), "{}");
        let old: ChannelOverrides = serde_json::from_str(r#"{"ttsEnabled":false}"#).unwrap();
        assert_eq!(old.sound_enabled, None);
    }
}
