use super::{Matched, Trigger};
use crate::model::{ChatMessage, MessageKind, Platform};
use serde_json::{json, Value};
fn escape(raw: &str) -> String {
    raw.chars()
        .flat_map(|c| {
            if "\\*_~`|>[]()#".contains(c) {
                vec!['\\', c]
            } else {
                vec![c]
            }
        })
        .collect()
}
fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.into()
    } else {
        s.chars().take(n.saturating_sub(1)).chain(['…']).collect()
    }
}
pub fn discord(matched: &Matched, msg: &ChatMessage) -> Value {
    let description = cut(&escape(&msg.plain_text()), 1000);
    let name = cut(&escape(&msg.author.name), 256);
    let platform = match msg.platform {
        Platform::Youtube => "YouTube",
        Platform::Twitch => "Twitch",
        Platform::X => "X",
        Platform::Niconico => "ニコニコ生放送",
        Platform::Twitcasting => "ツイキャス",
    };
    let title = match matched.trigger {
        Trigger::Donation => format!(
            "{} {}",
            match msg.kind {
                MessageKind::SuperChat => "スーパーチャット",
                MessageKind::Bits => "Bits",
                _ => "ギフト",
            },
            msg.amount
                .as_ref()
                .map(|a| a.raw_text.as_str())
                .unwrap_or_default()
        ),
        Trigger::Membership => "メンバー加入".into(),
        Trigger::Keyword => format!(
            "キーワード一致: {}",
            escape(matched.keyword.as_deref().unwrap_or_default())
        ),
        Trigger::Test => "Webhook テスト送信".into(),
    };
    // Discord title 自体の上限も守る。
    let title = cut(
        &escape_donation_title(matched, &title),
        256.min(
            6000 - description.chars().count() - name.chars().count() - platform.chars().count(),
        ),
    );
    let mut author = json!({"name": name});
    if let Some(raw) = &msg.author.avatar_url {
        if url::Url::parse(raw).is_ok_and(|u| u.scheme() == "https" && u.host().is_some()) {
            author["icon_url"] = json!(raw);
        }
    }
    let timestamp = chrono::DateTime::from_timestamp_millis(msg.timestamp_ms)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    json!({"username":"fast-comment", "allowed_mentions":{"parse":[]}, "embeds":[{"title":title,"description":description,"author":author,"color":15081751,"footer":{"text":platform},"timestamp":timestamp}]})
}
fn escape_donation_title(matched: &Matched, title: &str) -> String {
    if matched.trigger == Trigger::Donation {
        escape(title)
    } else {
        title.into()
    }
}
pub fn generic(event_id: &str, matched: &Matched, msg: &ChatMessage, sent_at_ms: u64) -> Value {
    let mut comment = crate::api::api_comment_json(0, msg);
    comment.as_object_mut().unwrap().remove("seq");
    let mut v = json!({"schema":"fast-comment.webhook.v1","eventId":event_id,"trigger":matched.trigger,"sentAtMs":sent_at_ms,"comment":comment});
    if matched.trigger == Trigger::Keyword {
        v["matchedKeyword"] = json!(matched.keyword);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webhook::Trigger;

    #[test]
    fn twitcasting_label_and_platform_key_are_used() {
        let req: crate::api::TestCommentRequest =
            serde_json::from_value(serde_json::json!({"platform": "twitcasting"})).unwrap();
        let msg = crate::inject::build_test_messages(&req).unwrap().remove(0);
        let matched = Matched {
            trigger: Trigger::Test,
            keyword: None,
        };
        assert!(discord(&matched, &msg).to_string().contains("ツイキャス"));
        let v = generic("e", &matched, &msg, 0);
        assert_eq!(v["comment"]["platform"], "twitcasting");
    }
}
