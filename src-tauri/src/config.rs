//! アプリ設定の構造体と永続化。
//!
//! 保存先は Tauri の app config dir 配下 `config.json`。
//! JSON は camelCase。起動時にロードし、変更のたびに保存する。
//! 設定は「重い処理は Rust 側」原則のもと、Source/TTS/Bus/Moderation の
//! 全層から参照される単一の設定ソースとなる。

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

/// 設定上のプラットフォーム種別。`model::Platform` と1対1だが、
/// 設定ファイル独立のため別定義(将来 niconico 等の追加に備える)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelPlatform {
    Twitch,
    Youtube,
}

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

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            port: default_obs_port(),
            template: default_obs_template(),
            font_scale_pct: default_obs_font_scale_pct(),
            max_rows: default_obs_max_rows(),
            ttl_ms: default_obs_ttl_ms(),
            bg_opacity_pct: default_obs_bg_opacity_pct(),
            position: default_obs_position(),
            show_platform: true,
        }
    }
}

/// OBS 配信目標ゲージ(Goals overlay)設定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalsConfig {
    /// Goals overlay を有効にするか。false のときは全ゲージを非表示扱いにする。
    pub enabled: bool,
    /// アプリ本体内にも GoalsBar を常設表示するか。既定 false。
    #[serde(default)]
    pub show_in_app: bool,
    /// コメント数目標。0 は非表示。
    pub comments: u32,
    /// 視聴者数目標。0 は非表示。
    pub viewers: u32,
    /// 高評価数目標。0 は非表示。
    pub likes: u32,
}

/// コメント本文に反応してアプリ内エフェクトを表示するルール。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectRule {
    /// 大文字小文字を区別せずに本文へ単純 includes するキーワード。
    #[serde(default)]
    pub keyword: String,
    /// 表示する文字列。絵文字以外も許可する。
    #[serde(default)]
    pub emoji: String,
    /// 1回の一致で生成するパーティクル数。
    #[serde(default = "default_effect_count")]
    pub count: u32,
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
    /// 置換前のプレーン文字列。空なら適用時にスキップする。
    #[serde(default)]
    pub from: String,
    /// 置換後のプレーン文字列。
    #[serde(default)]
    pub to: String,
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
    /// URL を「URL省略」等に置換するか。
    #[serde(default = "default_true")]
    pub omit_url: bool,
    /// 絵文字/エモートを読み上げから除去するか。
    #[serde(default = "default_true")]
    pub strip_emoji: bool,
    /// 1メッセージあたりの最大読み上げ文字数(超過分はカット)。
    #[serde(default = "default_max_read_len")]
    pub max_length: usize,
    /// 読み上げ本文に適用するユーザー定義のプレーン文字列置換辞書。
    #[serde(default)]
    pub dictionary: Vec<TtsDictEntry>,
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
            voicevox_url: default_voicevox_url(),
            voicevox_speaker: default_voicevox_speaker(),
            web_speech_rate: default_web_speech_rate(),
            web_speech_pitch: default_web_speech_pitch(),
            web_speech_volume: default_web_speech_volume(),
            web_speech_voice: default_web_speech_voice(),
            read_name: true,
            omit_url: true,
            strip_emoji: true,
            max_length: default_max_read_len(),
            dictionary: Vec::new(),
        }
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
    /// 投げ銭を別タブで表示するか。既定 false。
    #[serde(default)]
    pub show_donation_panel: bool,
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
            show_donation_panel: false,
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

/// アプリ全体設定のルート。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub obs: ObsConfig,
    #[serde(default)]
    pub goals: GoalsConfig,
    #[serde(default)]
    pub effects: EffectsConfig,
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
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            channels: Vec::new(),
            obs: ObsConfig::default(),
            goals: GoalsConfig::default(),
            effects: EffectsConfig::default(),
            tts: TtsConfig::default(),
            moderation: ModerationConfig::default(),
            ui: UiConfig::default(),
            participation: ParticipationConfig::default(),
            youtube_overrides: YoutubeOverrides::default(),
        }
    }
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
        if !path.exists() {
            return Ok(AppConfig::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("設定ファイル読込失敗 {}: {e}", path.display()))?;
        let cfg: AppConfig = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("設定ファイル解析失敗 {}: {e}", path.display()))?;
        Ok(cfg)
    }

    /// `config.json` へ保存する(ディレクトリは必要なら作成)。
    ///
    /// 同一ディレクトリの一時ファイルへ書き込み → fsync → rename で原子的に置換する。
    /// rename は NTFS/POSIX いずれでも同一ディレクトリ内であれば原子的。
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn serde_roundtrip_keeps_camel_case_keys_and_values() {
        // camelCase の永続化キーと、各フィールド値の serde 往復を確認する。
        let mut paths = HashMap::new();
        paths.insert(
            "continuationPath".to_string(),
            "contents.twoColumnWatchNextResults.conversationBar".to_string(),
        );

        let cfg = AppConfig {
            channels: vec![ChannelConfig {
                platform: ChannelPlatform::Youtube,
                identifier: "https://www.youtube.com/watch?v=abc123".to_string(),
                enabled: false,
            }],
            obs: ObsConfig {
                port: 12000,
                template: "compact".to_string(),
                font_scale_pct: 125,
                max_rows: 12,
                ttl_ms: 9000,
                bg_opacity_pct: 35,
                position: "top".to_string(),
                show_platform: false,
            },
            goals: GoalsConfig {
                enabled: true,
                show_in_app: true,
                comments: 100,
                viewers: 50,
                likes: 25,
            },
            effects: EffectsConfig {
                enabled: true,
                rules: vec![EffectRule {
                    keyword: "party".to_string(),
                    emoji: "🎉".to_string(),
                    count: 24,
                }],
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
                    voicevox_url: "http://127.0.0.1:50022".to_string(),
                    voicevox_speaker: 3,
                    web_speech_rate: 1.4,
                    web_speech_pitch: 0.8,
                    web_speech_volume: 0.6,
                    web_speech_voice: "Test Voice".to_string(),
                    read_name: false,
                    omit_url: false,
                    strip_emoji: false,
                    max_length: 280,
                    dictionary: vec![
                        TtsDictEntry {
                            from: "fast".to_string(),
                            to: "ファスト".to_string(),
                        },
                        TtsDictEntry {
                            from: "comment".to_string(),
                            to: "コメント".to_string(),
                        },
                    ],
                },
            },
            moderation: ModerationConfig {
                ng_words: vec!["spam".to_string()],
                ng_users: vec!["bot".to_string()],
                highlights: vec!["important".to_string()],
            },
            ui: UiConfig {
                max_buffer: 1234,
                show_donation_panel: true,
                notify_sound: true,
                notify_volume: 0.8,
            },
            participation: ParticipationConfig {
                enabled: true,
                keyword: "join".to_string(),
                max: 32,
            },
            youtube_overrides: YoutubeOverrides {
                api_key: Some("test-api-key".to_string()),
                client_version: Some("1.20240501.00.00".to_string()),
                paths,
            },
        };

        let text = serde_json::to_string(&cfg).expect("serialize AppConfig");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse serialized JSON");

        assert_eq!(json["obs"]["template"].as_str(), Some("compact"));
        assert_eq!(json["obs"]["fontScalePct"].as_u64(), Some(125));
        assert_eq!(json["obs"]["maxRows"].as_u64(), Some(12));
        assert_eq!(json["obs"]["ttlMs"].as_u64(), Some(9000));
        assert_eq!(json["obs"]["bgOpacityPct"].as_u64(), Some(35));
        assert_eq!(json["obs"]["position"].as_str(), Some("top"));
        assert_eq!(json["obs"]["showPlatform"].as_bool(), Some(false));
        assert_eq!(json["goals"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["goals"]["showInApp"].as_bool(), Some(true));
        assert_eq!(json["goals"]["comments"].as_u64(), Some(100));
        assert_eq!(json["goals"]["viewers"].as_u64(), Some(50));
        assert_eq!(json["goals"]["likes"].as_u64(), Some(25));
        assert_eq!(json["effects"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["effects"]["rules"][0]["keyword"].as_str(), Some("party"));
        assert_eq!(json["effects"]["rules"][0]["emoji"].as_str(), Some("🎉"));
        assert_eq!(json["effects"]["rules"][0]["count"].as_u64(), Some(24));
        assert_eq!(json["ui"]["maxBuffer"].as_u64(), Some(1234));
        assert_eq!(json["ui"]["showDonationPanel"].as_bool(), Some(true));
        assert_eq!(json["ui"]["notifySound"].as_bool(), Some(true));
        // f32→JSON→f64 はビット表現が変わるので近似比較する。
        assert!((json["ui"]["notifyVolume"].as_f64().expect("notifyVolume") - 0.8).abs() < 1e-6);
        assert_eq!(json["participation"]["enabled"].as_bool(), Some(true));
        assert_eq!(json["participation"]["keyword"].as_str(), Some("join"));
        assert_eq!(json["participation"]["max"].as_u64(), Some(32));
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
        assert_eq!(cfg.effects, EffectsConfig::default());
        assert!(!cfg.effects.enabled);
        assert!(cfg.effects.rules.is_empty());
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
        assert!(!cfg.ui.show_donation_panel);
        // 通知設定は旧 config(キー欠落)でも default に劣化する(後方互換)。
        assert!(!cfg.ui.notify_sound);
        assert_eq!(cfg.ui.notify_volume, default_notify_volume());
        assert_eq!(cfg.participation, ParticipationConfig::default());
        assert!(!cfg.participation.enabled);
        assert_eq!(cfg.participation.keyword, default_participation_keyword());
        assert_eq!(cfg.participation.max, 0);
        assert_eq!(cfg.youtube_overrides, YoutubeOverrides::default());
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
        assert_eq!(cfg.effects, EffectsConfig::default());
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
        assert!(!cfg.ui.show_donation_panel);
        assert_eq!(cfg.participation, ParticipationConfig::default());
        assert_eq!(cfg.youtube_overrides, YoutubeOverrides::default());
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
                    from: "FC".to_string(),
                    to: "ファストコメント".to_string(),
                },
                TtsDictEntry {
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
        assert_eq!(legacy_goals.comments, 10);
        assert_eq!(legacy_goals.viewers, 20);
        assert_eq!(legacy_goals.likes, 30);

        let cfg = GoalsConfig {
            enabled: true,
            show_in_app: true,
            comments: 100,
            viewers: 50,
            likes: 25,
        };
        let text = serde_json::to_string(&cfg).expect("serialize goals config");
        let json: serde_json::Value = serde_json::from_str(&text).expect("parse goals json");
        assert_eq!(json["showInApp"].as_bool(), Some(true));

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
}
