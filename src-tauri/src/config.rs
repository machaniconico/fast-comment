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
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            port: default_obs_port(),
            template: default_obs_template(),
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

    /// VOICEVOX: ベースURL。
    #[serde(default = "default_voicevox_url")]
    pub voicevox_url: String,
    /// VOICEVOX: 話者(speaker)ID。
    #[serde(default = "default_voicevox_speaker")]
    pub voicevox_speaker: u32,

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
            voicevox_url: default_voicevox_url(),
            voicevox_speaker: default_voicevox_speaker(),
            read_name: true,
            omit_url: true,
            strip_emoji: true,
            max_length: default_max_read_len(),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfig {
    /// リングバッファの保持上限件数。既定 2000。
    #[serde(default = "default_max_buffer")]
    pub max_buffer: usize,
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
            notify_sound: false,
            notify_volume: default_notify_volume(),
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
    pub tts: TtsConfig,
    #[serde(default)]
    pub moderation: ModerationConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub youtube_overrides: YoutubeOverrides,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            channels: Vec::new(),
            obs: ObsConfig::default(),
            tts: TtsConfig::default(),
            moderation: ModerationConfig::default(),
            ui: UiConfig::default(),
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
fn default_notify_volume() -> f32 {
    0.5
}

fn default_max_buffer() -> usize {
    2000
}
fn default_max_read_len() -> usize {
    140
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
                    voicevox_url: "http://127.0.0.1:50022".to_string(),
                    voicevox_speaker: 3,
                    read_name: false,
                    omit_url: false,
                    strip_emoji: false,
                    max_length: 280,
                },
            },
            moderation: ModerationConfig {
                ng_words: vec!["spam".to_string()],
                ng_users: vec!["bot".to_string()],
                highlights: vec!["important".to_string()],
            },
            ui: UiConfig {
                max_buffer: 1234,
                notify_sound: true,
                notify_volume: 0.8,
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
        assert_eq!(json["ui"]["maxBuffer"].as_u64(), Some(1234));
        assert_eq!(json["ui"]["notifySound"].as_bool(), Some(true));
        // f32→JSON→f64 はビット表現が変わるので近似比較する。
        assert!((json["ui"]["notifyVolume"].as_f64().expect("notifyVolume") - 0.8).abs() < 1e-6);
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
            json["tts"]["options"]["voicevoxUrl"].as_str(),
            Some("http://127.0.0.1:50022")
        );
        assert_eq!(
            json["tts"]["options"]["voicevoxSpeaker"].as_u64(),
            Some(3)
        );
        assert_eq!(json["tts"]["options"]["readName"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["omitUrl"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["stripEmoji"].as_bool(), Some(false));
        assert_eq!(json["tts"]["options"]["maxLength"].as_u64(), Some(280));
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
        assert_eq!(cfg.tts.backend, TtsBackendKind::WebSpeech);
        assert_eq!(cfg.tts.options.bouyomi_host, default_bouyomi_host());
        assert_eq!(cfg.tts.options.bouyomi_host, "127.0.0.1");
        assert_eq!(cfg.tts.options.bouyomi_port, default_bouyomi_port());
        assert_eq!(cfg.tts.options.bouyomi_port, 50001);
        assert_eq!(cfg.tts.options.bouyomi_speed, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_volume, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_tone, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_voice, 0);
        assert_eq!(cfg.tts.options.voicevox_url, default_voicevox_url());
        assert_eq!(cfg.tts.options.voicevox_url, "http://127.0.0.1:50021");
        assert_eq!(cfg.tts.options.voicevox_speaker, default_voicevox_speaker());
        assert_eq!(cfg.tts.options.voicevox_speaker, 1);
        assert_eq!(cfg.tts.options.read_name, default_true());
        assert_eq!(cfg.tts.options.omit_url, default_true());
        assert_eq!(cfg.tts.options.strip_emoji, default_true());
        assert_eq!(cfg.tts.options.max_length, default_max_read_len());
        assert_eq!(cfg.tts.options.max_length, 140);
        assert!(cfg.moderation.ng_words.is_empty());
        assert!(cfg.moderation.ng_users.is_empty());
        assert!(cfg.moderation.highlights.is_empty());
        assert_eq!(cfg.ui.max_buffer, default_max_buffer());
        assert_eq!(cfg.ui.max_buffer, 2000);
        // 通知設定は旧 config(キー欠落)でも default に劣化する(後方互換)。
        assert!(!cfg.ui.notify_sound);
        assert_eq!(cfg.ui.notify_volume, default_notify_volume());
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
        assert_eq!(cfg.tts.backend, TtsBackendKind::Bouyomi);
        assert_eq!(cfg.tts.options.bouyomi_host, default_bouyomi_host());
        assert_eq!(cfg.tts.options.bouyomi_port, 50003);
        assert_eq!(cfg.tts.options.bouyomi_speed, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_volume, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_tone, default_minus_one());
        assert_eq!(cfg.tts.options.bouyomi_voice, 0);
        assert_eq!(cfg.tts.options.voicevox_url, default_voicevox_url());
        assert_eq!(cfg.tts.options.voicevox_speaker, default_voicevox_speaker());
        assert!(!cfg.tts.options.read_name);
        assert_eq!(cfg.tts.options.omit_url, default_true());
        assert_eq!(cfg.tts.options.strip_emoji, default_true());
        assert_eq!(cfg.tts.options.max_length, default_max_read_len());
        assert_eq!(cfg.ui.max_buffer, 321);
        assert_eq!(cfg.youtube_overrides, YoutubeOverrides::default());
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
