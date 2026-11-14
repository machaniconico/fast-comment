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
    /// X (Twitter) ライブ配信 (broadcasts)。
    X,
    /// ニコニコ生放送。
    Niconico,
}

/// 配信 1 回分の情報(X 自動告知の入力)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveStream {
    pub platform: Platform,
    /// 設定の ChannelConfig.identifier をそのまま(印の照合キー)
    pub channel_identifier: String,
    /// 配信 1 回を一意に表す ID: YouTube=videoId / Twitch=Helix stream id / ニコ生=lv 番組 ID
    pub stream_id: String,
    pub title: Option<String>,
    /// 視聴 URL(Source 側で組み立てる)
    pub url: String,
    /// 配信開始時刻(epoch ms)。取れなければ None
    pub started_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Started(LiveStream),
    /// 未配信を確認した。起動直後の最初の確認(未確認→未配信)でも送る
    Ended { platform: Platform, channel_identifier: String },
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
    /// YouTube Jewelsギフト(メンバーシップギフトとは別)
    Gift,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
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
    Text {
        text: String,
    },
    Emote {
        id: String,
        name: String,
        url: String,
    },
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

/// YouTube が1回の更新で通知した絵文字別リアクション増分。
///
/// リアクションは匿名で、チャットメッセージではないため `ChatMessage` へ混ぜず、
/// UI の短命な演出専用バッチとして送る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeReaction {
    /// 配信識別子(videoId)。複数配信の同時接続でも出所を失わない。
    pub channel: String,
    /// YouTube が返した Unicode 絵文字。未知の種類も固定マッピングせず保持する。
    pub emoji: String,
    /// この更新に含まれる同種リアクション件数。
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerInfo {
    pub comment_count: u64,
    pub gift_count: u64,
    pub first_seen_ms: u64,
    pub prev_seen_ms: Option<u64>,
    pub is_first_ever: bool,
    pub label: Option<String>,
    pub tts_muted: bool,
    #[serde(default)]
    pub tts_voice: Option<crate::listeners::TtsVoiceOverride>,
}

/// 配信プラットフォーム側で行われた削除・BAN の通知(P6 Phase 3)。
///
/// Source が受信して side channel へ流し、UI へは 1 フレーム単位のバッチで送る。
/// `channel` は `ChatMessage.channel` と同じ識別子(Twitch はチャンネル名、YouTube は videoId)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ModerationEvent {
    /// 1 コメントの削除。`message_id` は `ChatMessage.id` と同じ形式。
    MessageDeleted {
        platform: Platform,
        channel: String,
        message_id: String,
    },
    /// ユーザーのタイムアウト(`duration_secs` = Some)または永久 BAN(None)。
    UserCleared {
        platform: Platform,
        channel: String,
        author_id: String,
        duration_secs: Option<u64>,
        /// `duration_secs` が確定情報か。false(InnerTube の著者削除など種別不明)なら
        /// UI は既存のタイムアウト / BAN 表示を上書きしない。
        certain: bool,
        /// 適用範囲の境界(サーバー時刻 ms)。`timestamp_ms <= before_ms` のコメントだけが対象。
        /// None なら境界なし(全コメント)。
        before_ms: Option<i64>,
    },
    /// チャット全消去(Twitch `/clear`)。`before_ms` は UserCleared と同じ意味。
    ChatCleared {
        platform: Platform,
        channel: String,
        before_ms: Option<i64>,
    },
}

impl ModerationEvent {
    /// このイベントが `msg` を対象にするか。TTS キューからの除外判定に使う。
    pub fn matches(&self, msg: &ChatMessage) -> bool {
        match self {
            ModerationEvent::MessageDeleted {
                platform,
                channel,
                message_id,
            } => msg.platform == *platform && msg.channel == *channel && msg.id == *message_id,
            ModerationEvent::UserCleared {
                platform,
                channel,
                author_id,
                before_ms,
                ..
            } => {
                msg.platform == *platform
                    && msg.channel == *channel
                    && msg.author.id == *author_id
                    && within(*before_ms, msg)
            }
            ModerationEvent::ChatCleared {
                platform,
                channel,
                before_ms,
            } => msg.platform == *platform && msg.channel == *channel && within(*before_ms, msg),
        }
    }
}

/// 境界以前(同時刻を含む)のコメントか。境界なしなら常に true。
fn within(before_ms: Option<i64>, msg: &ChatMessage) -> bool {
    match before_ms {
        Some(b) => msg.timestamp_ms <= b,
        None => true,
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// 正規化済みコメント。Source → Bus → UI/OBS を一貫して流れる単一型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<ListenerInfo>,
    /// 内部一意ID(uuid v4)。重複排除/個別操作に使う。
    pub id: String,
    /// `id` がプラットフォーム由来(モデレーション API に渡せる)の時だけ true。
    #[serde(default, skip_serializing_if = "is_false")]
    pub native_id: bool,
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
    /// 内部制御用: inject_test_comment が注入したテストコメント。シリアライズしない。
    #[serde(skip)]
    pub injected_test: bool,
}

impl ChatMessage {
    /// inject_test_comment が立てる内部フラグ(id 接頭辞は見ない)。
    pub fn is_injected_test_comment(&self) -> bool {
        self.injected_test
    }

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

#[cfg(test)]
mod tests {
    use super::YoutubeReaction;

    #[test]
    fn legacy_chat_message_without_listener_fields_deserializes() {
        let msg: super::ChatMessage = serde_json::from_value(serde_json::json!({
            "id": "old", "platform": "twitch", "channel": "channel",
            "author": {"id": "user", "name": "Alice"},
            "fragments": [], "kind": "normal", "timestampMs": 1
        }))
        .unwrap();
        assert!(msg.author.nickname.is_none());
        assert!(msg.author.avatar_url.is_none());
        assert!(msg.listener.is_none());
        assert!(!msg.native_id);
        let serialized = serde_json::to_value(&msg).unwrap();
        assert!(serialized.get("listener").is_none());
        assert!(serialized.get("nativeId").is_none());
        assert!(serialized["author"].get("nickname").is_none());
        assert!(serialized["author"].get("avatarUrl").is_none());
    }

    #[test]
    fn moderation_event_serializes_with_type_tag_and_camel_case_fields() {
        use super::{ModerationEvent, Platform};
        let deleted = serde_json::to_value(ModerationEvent::MessageDeleted {
            platform: Platform::Twitch,
            channel: "ch".into(),
            message_id: "m1".into(),
        })
        .unwrap();
        assert_eq!(
            deleted,
            serde_json::json!({"type": "messageDeleted", "platform": "twitch", "channel": "ch", "messageId": "m1"})
        );
        let cleared = serde_json::to_value(ModerationEvent::UserCleared {
            platform: Platform::Youtube,
            channel: "vid".into(),
            author_id: "UC1".into(),
            duration_secs: None,
            certain: false,
            before_ms: None,
        })
        .unwrap();
        assert_eq!(cleared["type"], "userCleared");
        assert_eq!(cleared["authorId"], "UC1");
        assert!(cleared["durationSecs"].is_null());
        assert_eq!(cleared["certain"], false);
        assert!(cleared["beforeMs"].is_null());
        let chat = serde_json::to_value(ModerationEvent::ChatCleared {
            platform: Platform::Twitch,
            channel: "ch".into(),
            before_ms: Some(5),
        })
        .unwrap();
        assert_eq!(chat["type"], "chatCleared");
        assert_eq!(chat["beforeMs"], 5);
    }

    #[test]
    fn moderation_event_matches_only_up_to_before_ms() {
        // 「旧コメント → CLEARCHAT → 新コメント」で新コメント(境界より後)は対象外。
        use super::{ModerationEvent, Platform};
        let mut msg: super::ChatMessage = serde_json::from_value(serde_json::json!({
            "id": "m", "platform": "twitch", "channel": "ch",
            "author": {"id": "u", "name": "U"},
            "fragments": [], "kind": "normal", "timestampMs": 100
        }))
        .unwrap();
        let clear = |before_ms| ModerationEvent::ChatCleared {
            platform: Platform::Twitch,
            channel: "ch".into(),
            before_ms,
        };
        assert!(clear(Some(100)).matches(&msg));
        assert!(clear(None).matches(&msg));
        assert!(!clear(Some(99)).matches(&msg));
        msg.timestamp_ms = 101;
        let user = ModerationEvent::UserCleared {
            platform: Platform::Twitch,
            channel: "ch".into(),
            author_id: "u".into(),
            duration_secs: Some(60),
            certain: true,
            before_ms: Some(100),
        };
        assert!(!user.matches(&msg));
    }

    #[test]
    fn youtube_reaction_serializes_for_camel_case_ipc() {
        let reaction = YoutubeReaction {
            channel: "video-1".to_string(),
            emoji: "🎉".to_string(),
            count: 3,
        };

        let value = serde_json::to_value(reaction).expect("serialize YouTube reaction");
        assert_eq!(value["channel"], "video-1");
        assert_eq!(value["emoji"], "🎉");
        assert_eq!(value["count"], 3);
    }
}
