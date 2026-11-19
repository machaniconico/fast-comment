//! Public API schema: explicitly copy only the documented fields.
use crate::model::{ChatMessage, Fragment, MessageKind, Platform};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiComment<'a> {
    seq: u64,
    id: &'a str,
    platform: &'static str,
    channel: &'a str,
    author: ApiAuthor<'a>,
    fragments: Vec<ApiFragment<'a>>,
    text: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<ApiAmount<'a>>,
    timestamp_ms: i64,
    highlight: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiAuthor<'a> {
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nickname: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_color: Option<&'a str>,
    badges: Vec<ApiBadge<'a>>,
    roles: ApiRoles,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiBadge<'a> {
    kind: &'a str,
    label: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<&'a str>,
}
#[derive(Serialize)]
struct ApiRoles {
    broadcaster: bool,
    moderator: bool,
    member: bool,
    subscriber: bool,
    vip: bool,
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ApiFragment<'a> {
    Text {
        text: &'a str,
    },
    Emote {
        id: &'a str,
        name: &'a str,
        url: &'a str,
    },
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiAmount<'a> {
    value: f64,
    currency: &'a str,
    raw_text: &'a str,
}

pub fn api_comment_json(seq: u64, msg: &ChatMessage) -> Value {
    let author = &msg.author;
    let roles = &author.roles;
    let dto = ApiComment {
        seq,
        id: &msg.id,
        channel: &msg.channel,
        platform: match msg.platform {
            Platform::Twitch => "twitch",
            Platform::Youtube => "youtube",
            Platform::X => "x",
            Platform::Niconico => "niconico",
            Platform::Twitcasting => "twitcasting",
        },
        author: ApiAuthor {
            id: &author.id,
            name: &author.name,
            nickname: author.nickname.as_deref(),
            avatar_url: author.avatar_url.as_deref(),
            display_color: author.display_color.as_deref(),
            badges: author
                .badges
                .iter()
                .filter(|b| b.kind != "highlight")
                .map(|b| ApiBadge {
                    kind: &b.kind,
                    label: &b.label,
                    image_url: b.image_url.as_deref(),
                })
                .collect(),
            roles: ApiRoles {
                broadcaster: roles.broadcaster,
                moderator: roles.moderator,
                member: roles.member,
                subscriber: roles.subscriber,
                vip: roles.vip,
            },
        },
        fragments: msg
            .fragments
            .iter()
            .map(|f| match f {
                Fragment::Text { text } => ApiFragment::Text { text },
                Fragment::Emote { id, name, url } => ApiFragment::Emote { id, name, url },
            })
            .collect(),
        text: msg.plain_text(),
        kind: match msg.kind {
            MessageKind::Normal => "normal",
            MessageKind::SuperChat => "superChat",
            MessageKind::Membership => "membership",
            MessageKind::Bits => "bits",
            MessageKind::Gift => "gift",
            MessageKind::System => "system",
        },
        amount: msg.amount.as_ref().map(|a| ApiAmount {
            value: a.value,
            currency: &a.currency,
            raw_text: &a.raw_text,
        }),
        timestamp_ms: msg.timestamp_ms,
        highlight: author.badges.iter().any(|b| b.kind == "highlight"),
    };
    serde_json::to_value(dto).expect("API comment serialization")
}
