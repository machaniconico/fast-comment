//! テストコメント生成。UI と HTTP API で共用する。
use crate::api::TestCommentRequest;
use crate::model::{Amount, Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
static SESSION_ID: LazyLock<uuid::Uuid> = LazyLock::new(uuid::Uuid::new_v4);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn build_test_messages(req: &TestCommentRequest) -> Result<Vec<ChatMessage>, String> {
    let platform = match req.platform.as_str() {
        "twitch" => Platform::Twitch,
        "youtube" => Platform::Youtube,
        "x" => Platform::X,
        "niconico" => Platform::Niconico,
        "twitcasting" => Platform::Twitcasting,
        other => return Err(format!("不正な platform です: {other}")),
    };
    let kind = match req.kind.as_str() {
        "normal" => MessageKind::Normal,
        "superChat" => MessageKind::SuperChat,
        "membership" => MessageKind::Membership,
        "bits" => MessageKind::Bits,
        "gift" => MessageKind::Gift,
        other => return Err(format!("不正な kind です: {other}")),
    };
    let count = req.count;
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("現在時刻取得失敗: {e}"))?
        .as_millis();
    let timestamp_ms = i64::try_from(epoch_millis).unwrap_or(i64::MAX);
    let author_name = req.name.trim();
    let author_name = if author_name.is_empty() {
        "テストユーザー"
    } else {
        author_name
    };
    let body = req.text.trim();
    let body = if body.is_empty() {
        "テストコメントです"
    } else {
        body
    };

    let mut messages = Vec::with_capacity(count as usize);
    for i in 0..count {
        let value = req
            .amount
            .filter(|_| matches!(kind, MessageKind::SuperChat | MessageKind::Bits))
            .map(|value| if value == 0.0 { 0.0 } else { value });
        messages.push(ChatMessage {
            listener: None,
            native_id: false,
            id: format!(
                "injected-test:{}-{}",
                *SESSION_ID,
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ),
            platform,
            channel: "test".to_string(),
            author: Author {
                nickname: None,
                avatar_url: None,
                id: format!("test{i}"),
                name: author_name.to_string(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::text(body.to_string())],
            kind,
            amount: value.map(|value| Amount {
                value,
                currency: "JPY".to_string(),
                raw_text: format!("¥{value}"),
            }),
            timestamp_ms: timestamp_ms.saturating_add(i64::from(i)),
            raw: None,
            skip_tts: false,
            injected_test: true,
        });
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_zero_amount_is_normalized() {
        for kind in ["superChat", "bits"] {
            let req: TestCommentRequest = serde_json::from_value(serde_json::json!({
                "platform": "youtube",
                "kind": kind,
                "amount": -0.0,
            }))
            .unwrap();
            assert!(req.amount.unwrap().is_sign_negative());
            let messages = build_test_messages(&req).unwrap();
            let amount = messages[0].amount.as_ref().unwrap();
            assert_eq!(amount.value, 0.0);
            assert!(amount.value.is_sign_positive());
            assert_eq!(amount.raw_text, "¥0");
        }
    }

    #[test]
    fn twitcasting_platform_is_accepted() {
        let req: TestCommentRequest = serde_json::from_value(serde_json::json!({
            "platform": "twitcasting",
        }))
        .unwrap();
        let messages = build_test_messages(&req).unwrap();
        assert_eq!(messages[0].platform, Platform::Twitcasting);
    }
}
