//! YouTube Data API v3 `liveChatMessages.streamList` の公式低遅延受信。
//!
//! APIキーが設定されている場合だけ使用し、接続・認証・quota等で失敗したら
//! 呼び出し元が従来のInnerTubeポーリングへフォールバックする。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use prost::Message;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tonic::codec::ProstCodec;
use tonic::metadata::MetadataValue;
use tonic::transport::Endpoint;

use crate::model::{Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};

use super::RecentMessageIds;

const STREAM_ENDPOINT: &str = "https://youtube.googleapis.com";
const STREAM_PATH: &str = "/youtube.api.v3.V3DataLiveChatMessageService/StreamList";
const VIDEOS_ENDPOINT: &str = "https://www.googleapis.com/youtube/v3/videos";

const TYPE_TEXT: i32 = 1;
const TYPE_NEW_SPONSOR: i32 = 7;
const TYPE_SUPER_CHAT: i32 = 15;
const TYPE_SUPER_STICKER: i32 = 16;
const TYPE_MEMBER_MILESTONE: i32 = 17;
const TYPE_MEMBERSHIP_GIFTING: i32 = 18;
const TYPE_GIFT_MEMBERSHIP_RECEIVED: i32 = 19;
const TYPE_GIFT: i32 = 21;

pub(super) async fn stream_live_chat(
    video_id: &str,
    api_key: &str,
    tx: &broadcast::Sender<ChatMessage>,
    cancel: &CancellationToken,
    seen: &mut RecentMessageIds,
) -> anyhow::Result<()> {
    let live_chat_id = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = resolve_live_chat_id(video_id, api_key) => result?,
    };

    let endpoint = Endpoint::from_static(STREAM_ENDPOINT);
    let channel = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = endpoint.connect() => {
            result.context("公式streamListのgRPC接続に失敗")?
        },
    };
    let mut grpc = tonic::client::Grpc::new(channel);
    let metadata = MetadataValue::try_from(api_key)
        .context("YouTube Data APIキーをgRPC metadataへ設定できません")?;
    let path = tonic::codegen::http::uri::PathAndQuery::from_static(STREAM_PATH);
    let codec = ProstCodec::<LiveChatMessageListRequest, LiveChatMessageListResponse>::default();
    let mut first_response = true;
    let mut next_page_token: Option<String> = None;

    loop {
        grpc.ready()
            .await
            .map_err(|e| anyhow::anyhow!("公式streamListサービスがreadyになりません: {e}"))?;
        let mut request = tonic::Request::new(LiveChatMessageListRequest {
            live_chat_id: Some(live_chat_id.clone()),
            hl: Some("ja".to_string()),
            profile_image_size: Some(32),
            page_token: next_page_token.clone(),
            part: vec!["snippet".to_string(), "authorDetails".to_string()],
        });
        request
            .metadata_mut()
            .insert("x-goog-api-key", metadata.clone());

        let response = grpc
            .server_streaming(request, path.clone(), codec.clone())
            .await
            .context("公式streamListの開始に失敗")?;
        let mut stream = response.into_inner();
        let mut received_response = false;

        loop {
            let response = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                result = stream.message() => result.context("公式streamListの受信に失敗")?,
            };
            let Some(response) = response else {
                break;
            };
            received_response = true;
            if response.next_page_token.is_some() {
                next_page_token = response.next_page_token.clone();
            }

            for item in response.items {
                if let Some(mut message) = to_chat_message(item, video_id) {
                    message.skip_tts = first_response;
                    if seen.insert(&message.id) {
                        let _ = tx.send(message);
                    }
                }
            }
            first_response = false;
        }

        if !received_response || next_page_token.is_none() {
            anyhow::bail!("公式streamListが継続トークンなしで終了しました");
        }
        tracing::debug!("youtube:{video_id} 公式streamListを継続トークンで再接続");
    }
}

async fn resolve_live_chat_id(video_id: &str, api_key: &str) -> anyhow::Result<String> {
    let response = reqwest::Client::new()
        .get(VIDEOS_ENDPOINT)
        .query(&[
            ("part", "liveStreamingDetails"),
            ("id", video_id),
            ("key", api_key),
        ])
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!("YouTube liveChatIdの取得に失敗: {}", error.without_url())
        })?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("YouTube Data APIがliveChatId取得を拒否しました (HTTP {status})");
    }
    let response = response
        .json::<VideoListResponse>()
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "YouTube liveChatIdレスポンスの解析に失敗: {}",
                error.without_url()
            )
        })?;

    response
        .items
        .into_iter()
        .find_map(|item| item.live_streaming_details?.active_live_chat_id)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("配信中のliveChatIdが見つかりません"))
}

fn to_chat_message(item: LiveChatMessage, video_id: &str) -> Option<ChatMessage> {
    let snippet = item.snippet?;
    let type_id = snippet.r#type.unwrap_or_default();
    if !matches!(
        type_id,
        TYPE_TEXT
            | TYPE_NEW_SPONSOR
            | TYPE_SUPER_CHAT
            | TYPE_SUPER_STICKER
            | TYPE_MEMBER_MILESTONE
            | TYPE_MEMBERSHIP_GIFTING
            | TYPE_GIFT_MEMBERSHIP_RECEIVED
            | TYPE_GIFT
    ) {
        return None;
    }

    let author_details = item.author_details.unwrap_or_default();
    let member = author_details.is_chat_sponsor.unwrap_or(false);
    let moderator = author_details.is_chat_moderator.unwrap_or(false);
    let broadcaster = author_details.is_chat_owner.unwrap_or(false);
    let mut badges = Vec::new();
    if broadcaster {
        badges.push(text_badge("broadcaster", "配信者"));
    }
    if moderator {
        badges.push(text_badge("moderator", "モデレーター"));
    }
    if member {
        badges.push(text_badge("member", "メンバー"));
    }
    let author = Author {
        id: author_details
            .channel_id
            .or_else(|| snippet.author_channel_id.clone())
            .unwrap_or_default(),
        name: author_details.display_name.unwrap_or_default(),
        display_color: None,
        badges,
        roles: Roles {
            broadcaster,
            moderator,
            member,
            subscriber: false,
            vip: false,
        },
    };

    let (kind, amount, fallback_text) = match type_id {
        TYPE_SUPER_CHAT => {
            let details = snippet.super_chat_details.as_ref();
            (
                MessageKind::SuperChat,
                details.map(amount_from_super_chat),
                details.and_then(|d| d.user_comment.clone()),
            )
        }
        TYPE_SUPER_STICKER => {
            let details = snippet.super_sticker_details.as_ref();
            (
                MessageKind::SuperChat,
                details.map(amount_from_super_sticker),
                details
                    .and_then(|d| d.super_sticker_metadata.as_ref())
                    .and_then(|m| m.alt_text.clone())
                    .map(|text| format!("[SuperSticker: {text}]")),
            )
        }
        TYPE_NEW_SPONSOR
        | TYPE_MEMBER_MILESTONE
        | TYPE_MEMBERSHIP_GIFTING
        | TYPE_GIFT_MEMBERSHIP_RECEIVED => (
            MessageKind::Membership,
            None,
            membership_text(&snippet, type_id),
        ),
        TYPE_GIFT => (
            MessageKind::Gift,
            None,
            Some(static_gift_text(
                snippet.gift_details.as_ref(),
                snippet.display_message.as_deref(),
            )),
        ),
        _ => (MessageKind::Normal, None, None),
    };

    let text = fallback_text
        .filter(|text| !text.is_empty())
        .or_else(|| snippet.display_message.filter(|text| !text.is_empty()))
        .or_else(|| {
            snippet
                .text_message_details
                .and_then(|details| details.message_text)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or_else(|| match kind {
            MessageKind::Membership => "[Membership]".to_string(),
            MessageKind::SuperChat => "[SuperChat]".to_string(),
            _ => "[YouTube event]".to_string(),
        });

    Some(ChatMessage {
        id: item
            .id
            .filter(|id| !id.is_empty())
            .unwrap_or_else(ChatMessage::new_id),
        platform: Platform::Youtube,
        channel: video_id.to_string(),
        author,
        fragments: vec![Fragment::text(text)],
        kind,
        amount,
        timestamp_ms: snippet
            .published_at
            .as_deref()
            .and_then(parse_timestamp_ms)
            .unwrap_or_else(now_ms),
        raw: None,
        skip_tts: false,
    })
}

fn membership_text(snippet: &LiveChatMessageSnippet, type_id: i32) -> Option<String> {
    match type_id {
        TYPE_MEMBER_MILESTONE => snippet
            .member_milestone_chat_details
            .as_ref()
            .and_then(|details| details.user_comment.clone()),
        TYPE_MEMBERSHIP_GIFTING => snippet
            .membership_gifting_details
            .as_ref()
            .and_then(|details| details.gift_memberships_count)
            .map(|count| format!("メンバーシップを{count}件プレゼントしました")),
        TYPE_GIFT_MEMBERSHIP_RECEIVED => Some("メンバーシップギフトを受け取りました".to_string()),
        TYPE_NEW_SPONSOR => Some("メンバーになりました".to_string()),
        _ => None,
    }
}

fn static_gift_text(
    details: Option<&LiveChatGiftDetails>,
    display_message: Option<&str>,
) -> String {
    if let Some(details) = details {
        let gift_name = details
            .gift_name
            .as_deref()
            .or(details.alt_text.as_deref())
            .unwrap_or_default()
            .trim();
        if !gift_name.is_empty() {
            let mut metadata = Vec::new();
            if let Some(jewels) = details.jewels_amount.filter(|value| *value > 0) {
                metadata.push(format!("{jewels} Jewels"));
            }
            if let Some(combo) = details.combo_count.filter(|value| *value > 1) {
                metadata.push(format!("×{combo}"));
            }
            return if metadata.is_empty() {
                gift_name.to_string()
            } else {
                format!("{gift_name}（{}）", metadata.join("・"))
            };
        }
    }

    let text = display_message.unwrap_or_default().trim();
    if text.is_empty() {
        "[ギフト]".to_string()
    } else {
        text.to_string()
    }
}

fn amount_from_super_chat(details: &LiveChatSuperChatDetails) -> Amount {
    Amount {
        value: details.amount_micros.unwrap_or_default() as f64 / 1_000_000.0,
        currency: details.currency.clone().unwrap_or_default(),
        raw_text: details.amount_display_string.clone().unwrap_or_default(),
    }
}

fn amount_from_super_sticker(details: &LiveChatSuperStickerDetails) -> Amount {
    Amount {
        value: details.amount_micros.unwrap_or_default() as f64 / 1_000_000.0,
        currency: details.currency.clone().unwrap_or_default(),
        raw_text: details.amount_display_string.clone().unwrap_or_default(),
    }
}

fn text_badge(kind: &str, label: &str) -> Badge {
    Badge {
        kind: kind.to_string(),
        label: label.to_string(),
        image_url: None,
    }
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct VideoListResponse {
    #[serde(default)]
    items: Vec<VideoItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoItem {
    live_streaming_details: Option<LiveStreamingDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveStreamingDetails {
    active_live_chat_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMessageListRequest {
    #[prost(string, optional, tag = "1")]
    live_chat_id: Option<String>,
    #[prost(string, optional, tag = "2")]
    hl: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    profile_image_size: Option<u32>,
    #[prost(string, optional, tag = "99")]
    page_token: Option<String>,
    #[prost(string, repeated, tag = "100")]
    part: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMessageListResponse {
    #[prost(string, optional, tag = "100602")]
    next_page_token: Option<String>,
    #[prost(message, repeated, tag = "1007")]
    items: Vec<LiveChatMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMessage {
    #[prost(string, optional, tag = "101")]
    id: Option<String>,
    #[prost(message, optional, tag = "2")]
    snippet: Option<LiveChatMessageSnippet>,
    #[prost(message, optional, tag = "3")]
    author_details: Option<LiveChatMessageAuthorDetails>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMessageAuthorDetails {
    #[prost(string, optional, tag = "10101")]
    channel_id: Option<String>,
    #[prost(string, optional, tag = "103")]
    display_name: Option<String>,
    #[prost(bool, optional, tag = "5")]
    is_chat_owner: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    is_chat_sponsor: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    is_chat_moderator: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMessageSnippet {
    #[prost(enumeration = "LiveChatMessageType", optional, tag = "1")]
    r#type: Option<i32>,
    #[prost(string, optional, tag = "301")]
    author_channel_id: Option<String>,
    #[prost(string, optional, tag = "4")]
    published_at: Option<String>,
    #[prost(string, optional, tag = "16")]
    display_message: Option<String>,
    #[prost(message, optional, tag = "19")]
    text_message_details: Option<LiveChatTextMessageDetails>,
    #[prost(message, optional, tag = "27")]
    super_chat_details: Option<LiveChatSuperChatDetails>,
    #[prost(message, optional, tag = "28")]
    super_sticker_details: Option<LiveChatSuperStickerDetails>,
    #[prost(message, optional, tag = "30")]
    member_milestone_chat_details: Option<LiveChatMemberMilestoneChatDetails>,
    #[prost(message, optional, tag = "31")]
    membership_gifting_details: Option<LiveChatMembershipGiftingDetails>,
    #[prost(message, optional, tag = "34")]
    gift_details: Option<LiveChatGiftDetails>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum LiveChatMessageType {
    Invalid = 0,
    Text = TYPE_TEXT,
    NewSponsor = TYPE_NEW_SPONSOR,
    SuperChat = TYPE_SUPER_CHAT,
    SuperSticker = TYPE_SUPER_STICKER,
    MemberMilestone = TYPE_MEMBER_MILESTONE,
    MembershipGifting = TYPE_MEMBERSHIP_GIFTING,
    GiftMembershipReceived = TYPE_GIFT_MEMBERSHIP_RECEIVED,
    Gift = TYPE_GIFT,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatTextMessageDetails {
    #[prost(string, optional, tag = "1")]
    message_text: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatSuperChatDetails {
    #[prost(uint64, optional, tag = "1")]
    amount_micros: Option<u64>,
    #[prost(string, optional, tag = "2")]
    currency: Option<String>,
    #[prost(string, optional, tag = "3")]
    amount_display_string: Option<String>,
    #[prost(string, optional, tag = "4")]
    user_comment: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatSuperStickerDetails {
    #[prost(uint64, optional, tag = "1")]
    amount_micros: Option<u64>,
    #[prost(string, optional, tag = "2")]
    currency: Option<String>,
    #[prost(string, optional, tag = "3")]
    amount_display_string: Option<String>,
    #[prost(message, optional, tag = "5")]
    super_sticker_metadata: Option<SuperStickerMetadata>,
}

#[derive(Clone, PartialEq, Message)]
struct SuperStickerMetadata {
    #[prost(string, optional, tag = "2")]
    alt_text: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMemberMilestoneChatDetails {
    #[prost(string, optional, tag = "3")]
    user_comment: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatMembershipGiftingDetails {
    #[prost(int32, optional, tag = "1")]
    gift_memberships_count: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct LiveChatGiftDetails {
    #[prost(string, optional, tag = "1")]
    gift_name: Option<String>,
    #[prost(int32, optional, tag = "3")]
    jewels_amount: Option<i32>,
    #[prost(string, optional, tag = "5")]
    alt_text: Option<String>,
    #[prost(int32, optional, tag = "8")]
    combo_count: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, PartialEq, Message)]
    struct GiftMessageFixture {
        #[prost(string, optional, tag = "101")]
        id: Option<String>,
        #[prost(message, optional, tag = "2")]
        snippet: Option<GiftSnippetFixture>,
        #[prost(message, optional, tag = "3")]
        author_details: Option<LiveChatMessageAuthorDetails>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct GiftSnippetFixture {
        #[prost(int32, optional, tag = "1")]
        r#type: Option<i32>,
        #[prost(string, optional, tag = "16")]
        display_message: Option<String>,
        #[prost(message, optional, tag = "34")]
        gift_details: Option<GiftDetailsFixture>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct GiftDetailsFixture {
        #[prost(string, optional, tag = "1")]
        gift_name: Option<String>,
        #[prost(int32, optional, tag = "3")]
        jewels_amount: Option<i32>,
        #[prost(int32, optional, tag = "8")]
        combo_count: Option<i32>,
    }

    #[test]
    fn converts_text_message_with_roles_and_timestamp() {
        let item = LiveChatMessage {
            id: Some("message-1".to_string()),
            snippet: Some(LiveChatMessageSnippet {
                r#type: Some(TYPE_TEXT),
                author_channel_id: Some("author-1".to_string()),
                published_at: Some("2026-07-21T01:02:03.456Z".to_string()),
                display_message: Some("こんにちは".to_string()),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("配信者".to_string()),
                is_chat_owner: Some(true),
                ..Default::default()
            }),
        };

        let message = to_chat_message(item, "video-1").expect("chat message");
        assert_eq!(message.id, "message-1");
        assert_eq!(message.plain_text(), "こんにちは");
        assert_eq!(message.kind, MessageKind::Normal);
        assert!(message.author.roles.broadcaster);
        assert_eq!(message.timestamp_ms, 1_784_595_723_456);
    }

    #[test]
    fn converts_super_chat_amount() {
        let item = LiveChatMessage {
            id: Some("super-1".to_string()),
            snippet: Some(LiveChatMessageSnippet {
                r#type: Some(TYPE_SUPER_CHAT),
                super_chat_details: Some(LiveChatSuperChatDetails {
                    amount_micros: Some(250_000_000),
                    currency: Some("JPY".to_string()),
                    amount_display_string: Some("¥250".to_string()),
                    user_comment: Some("応援しています".to_string()),
                }),
                ..Default::default()
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("視聴者".to_string()),
                ..Default::default()
            }),
        };

        let message = to_chat_message(item, "video-1").expect("super chat");
        assert_eq!(message.kind, MessageKind::SuperChat);
        assert_eq!(message.plain_text(), "応援しています");
        let amount = message.amount.expect("amount");
        assert_eq!(amount.value, 250.0);
        assert_eq!(amount.raw_text, "¥250");
    }

    #[test]
    fn converts_jewels_gift_into_static_comment() {
        let wire_item = GiftMessageFixture {
            id: Some("gift-1".to_string()),
            snippet: Some(GiftSnippetFixture {
                r#type: Some(TYPE_GIFT),
                display_message: Some("Aliceさんがバラのギフトを贈りました".to_string()),
                gift_details: Some(GiftDetailsFixture {
                    gift_name: Some("バラ".to_string()),
                    jewels_amount: Some(100),
                    combo_count: Some(3),
                }),
            }),
            author_details: Some(LiveChatMessageAuthorDetails {
                display_name: Some("Alice".to_string()),
                ..Default::default()
            }),
        };
        let item = LiveChatMessage::decode(wire_item.encode_to_vec().as_slice())
            .expect("decode gift fixture with the official stream field numbers");

        let message = to_chat_message(item, "video-1").expect("gift event");
        assert_eq!(message.kind, MessageKind::Gift);
        assert_eq!(message.plain_text(), "バラ（100 Jewels・×3）");
    }

    #[test]
    fn recent_ids_reject_duplicates_and_remain_bounded() {
        let mut recent = RecentMessageIds::new();
        assert!(recent.insert("same"));
        assert!(!recent.insert("same"));
        for index in 0..=super::super::RECENT_MESSAGE_IDS {
            assert!(recent.insert(&format!("id-{index}")));
        }
        assert!(recent.ids.len() <= super::super::RECENT_MESSAGE_IDS);
    }
}
