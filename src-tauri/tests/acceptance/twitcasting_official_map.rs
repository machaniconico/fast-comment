// registered in main.rs by story TC3-303
//! PR3' 受け入れテスト 4.4: 公式コメント → `ChatMessage` の写像(純粋)。
//! 入力の形は公式 API のコメント(`id` / `message` / `created`(秒)/ `from_user.{id,screen_id,name,image}`)。
#![allow(dead_code)]

use fast_comment_lib::model::{Fragment, MessageKind, Platform};
use fast_comment_lib::sources::twitcasting_official::{official_comment_to_chat, OfficialMapContext};
use serde_json::{json, Value};

const NOW: i64 = 42;

fn ctx<'a>(broadcaster: Option<&'a str>) -> OfficialMapContext<'a> {
    OfficialMapContext { channel: "alice", broadcaster_id: broadcaster, now_ms: NOW }
}

fn base() -> Value {
    json!({
        "id": "123",
        "message": "hello",
        "created": 1_700_000_000u64,
        "from_user": {
            "id": "u1",
            "screen_id": "scr",
            "name": "Alice",
            "image": "https://img.example/a.png"
        }
    })
}

#[test]
fn comment_maps_to_chat_message_fields() {
    let m = official_comment_to_chat(&base(), &ctx(Some("zzz"))).expect("写像できる");
    assert_eq!(m.id, "tc-123");
    assert!(!m.native_id);
    assert_eq!(m.platform, Platform::Twitcasting);
    assert_eq!(m.channel, "alice");
    assert_eq!(m.author.id, "u1");
    assert_eq!(m.author.name, "Alice");
    assert_eq!(m.author.avatar_url.as_deref(), Some("https://img.example/a.png"));
    assert_eq!(m.fragments, vec![Fragment::text("hello")]);
    assert_eq!(m.kind, MessageKind::Normal);
    assert!(m.amount.is_none());
    assert_eq!(m.timestamp_ms, 1_700_000_000_000);
}

#[test]
fn broadcaster_role_set_only_for_broadcaster_id() {
    let m = official_comment_to_chat(&base(), &ctx(Some("u1"))).expect("写像できる");
    assert!(m.author.roles.broadcaster);
    let m = official_comment_to_chat(&base(), &ctx(Some("someone-else"))).expect("写像できる");
    assert!(!m.author.roles.broadcaster);
    let m = official_comment_to_chat(&base(), &ctx(None)).expect("写像できる");
    assert!(!m.author.roles.broadcaster);
}

#[test]
fn whitespace_only_message_is_skipped() {
    let mut v = base();
    v["message"] = json!("  \n\t ");
    assert!(official_comment_to_chat(&v, &ctx(None)).is_none());
}

#[test]
fn missing_from_user_id_is_skipped() {
    let mut v = base();
    v["from_user"].as_object_mut().unwrap().remove("id");
    assert!(official_comment_to_chat(&v, &ctx(None)).is_none());
}

#[test]
fn empty_name_falls_back_to_screen_id() {
    let mut v = base();
    v["from_user"]["name"] = json!("");
    let m = official_comment_to_chat(&v, &ctx(None)).expect("写像できる");
    assert_eq!(m.author.name, "scr");
}

#[test]
fn non_https_avatar_is_dropped_and_protocol_relative_is_upgraded() {
    let mut v = base();
    v["from_user"]["image"] = json!("http://img.example/a.png");
    let m = official_comment_to_chat(&v, &ctx(None)).expect("写像できる");
    assert_eq!(m.author.avatar_url, None);

    v["from_user"]["image"] = json!("//a/b");
    let m = official_comment_to_chat(&v, &ctx(None)).expect("写像できる");
    assert_eq!(m.author.avatar_url.as_deref(), Some("https://a/b"));
}

#[test]
fn zero_or_missing_created_uses_now_and_ms_created_is_kept() {
    let mut v = base();
    v["created"] = json!(0);
    assert_eq!(official_comment_to_chat(&v, &ctx(None)).expect("0").timestamp_ms, NOW);

    v.as_object_mut().unwrap().remove("created");
    assert_eq!(official_comment_to_chat(&v, &ctx(None)).expect("欠落").timestamp_ms, NOW);

    v["created"] = json!(1_700_000_000_123u64);
    assert_eq!(official_comment_to_chat(&v, &ctx(None)).expect("13 桁").timestamp_ms, 1_700_000_000_123);
}

#[test]
fn numeric_json_ids_are_accepted() {
    let mut v = base();
    v["id"] = json!(123);
    let m = official_comment_to_chat(&v, &ctx(None)).expect("数値 ID でも写像できる");
    assert_eq!(m.id, "tc-123");
}

#[test]
fn message_text_is_preserved_verbatim() {
    let text = "  😀 line1\nline2 \u{3000}";
    let mut v = base();
    v["message"] = json!(text);
    let m = official_comment_to_chat(&v, &ctx(None)).expect("写像できる");
    assert_eq!(m.fragments, vec![Fragment::text(text)]);
}
