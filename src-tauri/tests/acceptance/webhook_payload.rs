//! PR4 受け入れテスト: 送信内容の組み立てと署名(設計書 §3.3・§5.6)。
//!
//! 実装者が満たす契約(付録 D に加えて、このファイルが前提にする物):
//! - `webhook::payload::discord(&Matched, &ChatMessage) -> serde_json::Value`、
//!   `webhook::payload::generic(event_id, &Matched, &ChatMessage, sent_at_ms) -> serde_json::Value`、
//!   `webhook::sign::signature_header(secret: &[u8], timestamp_secs, body: &[u8]) -> String`。
//! - Discord の `embeds[0]` は `title` / `description` / `author{name, icon_url?}` / `footer{text}` / `timestamp`。
//!   `footer.text` はプラットフォーム名(YouTube は `"YouTube"`)。`timestamp` は `timestamp_ms` の
//!   ISO 8601 UTC・ミリ秒付き(`2024-11-14T16:00:00.000Z`)。トップレベルに `content` は無い。
//! - 長さの上限は `chars()` で数える。切る時は文字境界で切り、末尾に `…` を付ける。`…` を含めた長さが上限以内。
//!   description は 1000 文字、author.name は 256 文字。embed 全体(title + description + author.name + footer.text)は
//!   6000 文字以内(title が極端に長い時は title を切って守る)。
//! - Markdown の無効化は、`\` `*` `_` `~` `` ` `` `|` `>` `[` `]` `(` `)` `#` の 12 文字それぞれの前に `\` を置く。
//!   description・author.name・keyword きっかけの title に含まれる語に適用する。
//! - 汎用 JSON の `matchedKeyword` は `keyword` きっかけの時だけ現れる(他は欠落。`null` も許容)。
//!   `trigger` は `"donation" | "membership" | "keyword" | "test"`。
//! - `comment` は `api::api_comment_json(0, msg)` から `seq` を除いた物と一致する。

use crate::webhook_common::*;
use fast_comment_lib::api::api_comment_json;
use fast_comment_lib::model::{Fragment, MessageKind, Platform};
use fast_comment_lib::webhook::payload;
use fast_comment_lib::webhook::sign::signature_header;
use fast_comment_lib::webhook::{Matched, Trigger};

fn donation() -> Matched {
    Matched { trigger: Trigger::Donation, keyword: None }
}

fn keyword(k: &str) -> Matched {
    Matched { trigger: Trigger::Keyword, keyword: Some(k.into()) }
}

fn embed(v: &serde_json::Value) -> &serde_json::Value {
    &v["embeds"][0]
}

fn chars(v: &serde_json::Value) -> usize {
    v.as_str().unwrap_or_default().chars().count()
}

const ALL_TWELVE: &str = "\\*_~`|>[]()#";
const ALL_TWELVE_ESCAPED: &str = "\\\\\\*\\_\\~\\`\\|\\>\\[\\]\\(\\)\\#";

// ---- Discord ----

#[test]
fn discord_payload_sets_allowed_mentions_parse_empty() {
    let v = payload::discord(&donation(), &superchat("a", "¥500"));
    assert_eq!(v["allowed_mentions"], serde_json::json!({ "parse": [] }));
}

#[test]
fn discord_payload_escapes_markdown_and_masked_links() {
    let m = with_kind("b", MessageKind::SuperChat, "[click](http://evil.example) **x** @everyone");
    let v = payload::discord(&donation(), &m);
    assert_eq!(
        embed(&v)["description"].as_str().unwrap(),
        "\\[click\\]\\(http://evil.example\\) \\*\\*x\\*\\* @everyone"
    );
}

#[test]
fn discord_payload_escapes_author_name() {
    let mut m = superchat("c", "¥500");
    m.author.name = ALL_TWELVE.into();
    let v = payload::discord(&donation(), &m);
    assert_eq!(embed(&v)["author"]["name"].as_str().unwrap(), ALL_TWELVE_ESCAPED);
}

#[test]
fn discord_description_is_cut_at_1000_chars_on_char_boundary_with_ellipsis() {
    let text = "あ😀".repeat(1000); // 2000 文字
    assert_eq!(text.chars().count(), 2000);
    let v = payload::discord(&donation(), &with_kind("d", MessageKind::SuperChat, &text));
    let d = embed(&v)["description"].as_str().unwrap();
    let n = d.chars().count();
    assert!((999..=1000).contains(&n), "文字数 {n}");
    assert!(d.ends_with('…'));
    let body = d.trim_end_matches('…');
    assert!(text.starts_with(body), "元の本文の先頭をそのまま残す(文字の途中で切らない)");
}

#[test]
fn discord_author_name_is_cut_at_256_chars() {
    let mut m = superchat("e", "¥500");
    m.author.name = "名".repeat(300);
    let v = payload::discord(&donation(), &m);
    let name = embed(&v)["author"]["name"].as_str().unwrap();
    let n = name.chars().count();
    assert!((255..=256).contains(&n), "文字数 {n}");
    assert!(name.ends_with('…'));
}

#[test]
fn discord_embed_total_is_within_6000_chars() {
    let mut m = superchat("f", &"¥".repeat(7000));
    m.fragments = vec![Fragment::text("本".repeat(5000))];
    m.author.name = "名".repeat(3000);
    let v = payload::discord(&donation(), &m);
    let e = embed(&v);
    let total = chars(&e["title"])
        + chars(&e["description"])
        + chars(&e["author"]["name"])
        + chars(&e["footer"]["text"]);
    assert!(total <= 6000, "合計 {total}");
}

#[test]
fn discord_title_for_donation_contains_amount_raw_text() {
    let v = payload::discord(&donation(), &superchat("g", "¥500"));
    assert!(embed(&v)["title"].as_str().unwrap().contains("¥500"));
}

#[test]
fn discord_title_for_keyword_contains_matched_keyword_escaped() {
    let m = normal("h", "a_b*c を含む");
    let v = payload::discord(&keyword("a_b*c"), &m);
    let title = embed(&v)["title"].as_str().unwrap();
    assert!(title.contains("キーワード一致"), "{title}");
    assert!(title.contains("a\\_b\\*c"), "{title}");
}

#[test]
fn discord_icon_url_only_when_https() {
    let mut m = superchat("i", "¥500");
    m.author.avatar_url = Some("https://img.example.test/a.png".into());
    let v = payload::discord(&donation(), &m);
    assert_eq!(embed(&v)["author"]["icon_url"], "https://img.example.test/a.png");

    m.author.avatar_url = Some("http://img.example.test/a.png".into());
    let v = payload::discord(&donation(), &m);
    assert!(embed(&v)["author"].get("icon_url").map_or(true, |x| x.is_null()), "http は載せない");

    m.author.avatar_url = None;
    let v = payload::discord(&donation(), &m);
    assert!(embed(&v)["author"].get("icon_url").map_or(true, |x| x.is_null()));
}

#[test]
fn discord_payload_has_username_and_single_embed_and_no_content() {
    let mut m = superchat("j", "¥500");
    m.platform = Platform::Youtube;
    m.timestamp_ms = 1_731_600_000_000;
    let v = payload::discord(&donation(), &m);
    assert_eq!(v["username"], "fast-comment");
    assert_eq!(v["embeds"].as_array().unwrap().len(), 1);
    assert!(v.get("content").is_none(), "content は付けない");
    assert_eq!(embed(&v)["footer"]["text"], "YouTube");
    assert_eq!(embed(&v)["timestamp"], "2024-11-14T16:00:00.000Z");
}

// ---- 汎用 JSON ----

#[test]
fn generic_payload_has_schema_event_id_trigger_sent_at_and_comment() {
    let m = superchat("k", "¥500");
    let v = payload::generic("11111111-2222-4333-8444-555555555555", &donation(), &m, 1_731_600_123_456);
    assert_eq!(v["schema"], "fast-comment.webhook.v1");
    assert_eq!(v["eventId"], "11111111-2222-4333-8444-555555555555");
    assert_eq!(v["trigger"], "donation");
    assert_eq!(v["sentAtMs"], 1_731_600_123_456u64);
    assert!(v["comment"].is_object());
    assert_eq!(v["comment"]["id"], "k");
}

#[test]
fn generic_payload_matched_keyword_only_for_keyword_trigger() {
    let m = normal("l", "質問です");
    let kw = payload::generic("e1", &keyword("質問"), &m, 1);
    assert_eq!(kw["trigger"], "keyword");
    assert_eq!(kw["matchedKeyword"], "質問");

    for matched in [donation(), Matched { trigger: Trigger::Membership, keyword: None }, Matched { trigger: Trigger::Test, keyword: None }] {
        let v = payload::generic("e2", &matched, &m, 1);
        assert!(v.get("matchedKeyword").map_or(true, |x| x.is_null()), "{:?}", matched.trigger);
    }
    let t = payload::generic("e3", &Matched { trigger: Trigger::Test, keyword: None }, &m, 1);
    assert_eq!(t["trigger"], "test");
}

#[test]
fn generic_comment_matches_api_comment_without_seq() {
    let mut m = superchat("m", "¥500");
    m.fragments = vec![
        Fragment::text("こんにちは"),
        Fragment::Emote { id: "e1".into(), name: "kappa".into(), url: "https://example.test/k.png".into() },
    ];
    let v = payload::generic("e", &donation(), &m, 1);
    let mut expected = api_comment_json(0, &m);
    expected.as_object_mut().unwrap().remove("seq");
    assert_eq!(v["comment"], expected);
    assert!(v["comment"].get("seq").is_none());
}

// ---- 署名 ----

const KEY: &[u8] = b"test-secret";

#[test]
fn signature_matches_known_vector() {
    // Python の hmac.new(b"test-secret", b"1731600000." + b'{"a":1}', sha256) で別途計算した値。
    assert_eq!(
        signature_header(KEY, 1_731_600_000, br#"{"a":1}"#),
        "v1=d7a16a2c3cc4c78e9053522c96e295900c70551cf7def810e2bd507b09d95947"
    );
}

#[test]
fn signature_header_is_v1_equals_64_lowercase_hex() {
    let h = signature_header(KEY, 1, b"x");
    let hex = h.strip_prefix("v1=").expect("v1= で始まる");
    assert_eq!(hex.len(), 64);
    assert!(hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{hex}");
}

#[test]
fn signature_changes_when_timestamp_changes() {
    let a = signature_header(KEY, 1_731_600_000, br#"{"a":1}"#);
    let b = signature_header(KEY, 1_731_600_001, br#"{"a":1}"#);
    assert_ne!(a, b);
    assert_eq!(b, "v1=ee9cf51391e007ff9af92f36fad7b653d27133d5897ec46f8e981014e7d93e65");
}

#[test]
fn signature_is_over_exact_body_bytes() {
    let body = "{\n  \"text\": \"こんにちは\\n\"\n}\n".as_bytes();
    assert_eq!(
        signature_header(KEY, 1_731_600_000, body),
        "v1=bbae49db6c03d63982cf26b9760fd565d4fed97e7ceea76a8bc253e3a391e578"
    );
    let mut compact = body.to_vec();
    compact.retain(|b| *b != b'\n');
    assert_ne!(
        signature_header(KEY, 1_731_600_000, &compact),
        signature_header(KEY, 1_731_600_000, body),
        "改行 1 バイトの違いでも署名は変わる"
    );
    // 実装を使わない独立計算とも一致する。
    let mut signed = b"1731600000.".to_vec();
    signed.extend_from_slice(body);
    assert_eq!(signature_header(KEY, 1_731_600_000, body), format!("v1={}", hmac_sha256_hex(KEY, &signed)));
}
