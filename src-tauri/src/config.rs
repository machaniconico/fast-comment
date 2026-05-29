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
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            port: default_obs_port(),
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
}

impl Default for UiConfig {
    fn default() -> Self {
        UiConfig {
            max_buffer: default_max_buffer(),
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
fn default_max_buffer() -> usize {
    2000
}
fn default_max_read_len() -> usize {
    140
}
