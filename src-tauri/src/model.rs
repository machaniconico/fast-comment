//! 全プラットフォーム共通の統一コメントモデル。
//!
//! Source 層が各プラットフォーム固有の形式をここに正規化し、Bus 層が
//! そのまま UI(Tauri IPC) と OBS(WebSocket) へ流す。JSON は camelCase。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Twitch,
    Youtube,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MessageKind {
    /// 通常コメント
    Normal,
    /// YouTube SuperChat
    SuperChat,
    /// YouTube メンバーシップ(加入/継続)
    Membership,
    /// Twitch Bits(cheer)
    Bits,
    /// システム/通知メッセージ
    System,
}

/// 著者の役割フラグ。プラットフォーム差を吸収した共通表現。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roles {
    pub broadcaster: bool,
    pub moderator: bool,
    pub member: bool,
    pub subscriber: bool,
    pub vip: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Badge {
    /// 例: "subscriber", "moderator", "member"
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Author {
    pub id: String,
    pub name: String,
    /// 表示色(例 "#FF7F50")。プラットフォーム/ユーザー設定由来。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_color: Option<String>,
    #[serde(default)]
    pub badges: Vec<Badge>,
    #[serde(default)]
    pub roles: Roles,
}

/// 本文の断片。テキストとエモートが混在する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Fragment {
    Text { text: String },
    Emote { id: String, name: String, url: String },
}

impl Fragment {
    pub fn text(s: impl Into<String>) -> Self {
        Fragment::Text { text: s.into() }
    }
}

/// SuperChat / Bits などの金額情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Amount {
    pub value: f64,
    /// ISO 4217 等。不明なら元表記から推定 or 空。
    pub currency: String,
    /// 元の表示文字列(例 "¥500", "$5.00")。
    pub raw_text: String,
}

/// 参加型配信の参加者。
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub platform: String,
    pub user_id: String,
    pub name: String,
    pub picked: bool,
}

/// 正規化済みコメント。Source → Bus → UI/OBS を一貫して流れる単一型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// 内部一意ID(uuid v4)。重複排除/個別操作に使う。
    pub id: String,
    pub platform: Platform,
    /// 配信チャンネル識別子(Twitchはチャンネル名、YouTubeはvideoId)。
    pub channel: String,
    pub author: Author,
    pub fragments: Vec<Fragment>,
    pub kind: MessageKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<Amount>,
    /// 受信時刻(unix ms)。
    pub timestamp_ms: i64,
    /// デバッグ用の原データ(任意・通常は None)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
    /// 内部制御用: 表示はするが TTS には流さない。
    #[serde(skip)]
    pub skip_tts: bool,
}

impl ChatMessage {
    /// 本文をプレーンテキストに連結(TTS/検索用)。
    /// emote は name で埋めて境界なし連結する既存挙動を維持する。
    pub fn plain_text(&self) -> String {
        self.fragments
            .iter()
            .map(|f| match f {
                Fragment::Text { text } => text.as_str(),
                Fragment::Emote { name, .. } => name.as_str(),
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// NG/ハイライト判定用: Text fragment のテキストのみを個別に返す。
    /// emote name を含めないことで fragment 境界またぎの誤マッチを防ぐ。
    pub fn text_fragments(&self) -> impl Iterator<Item = &str> {
        self.fragments.iter().filter_map(|f| match f {
            Fragment::Text { text } => Some(text.as_str()),
            Fragment::Emote { .. } => None,
        })
    }

    pub fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }
}
