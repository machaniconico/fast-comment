//! PR4 受け入れテスト: きっかけの判定と重複排除(設計書 §3.2・§5.6)。
//!
//! 実装者が満たす契約(付録 D に加えて、このファイルが前提にする物):
//! - `webhook::rules::decide(&WebhookDef, &ChatMessage) -> Option<Matched>` は純関数。
//! - `webhook::{Trigger, Matched}` は `Debug + Clone + PartialEq`(`Trigger` は `Copy + Eq` も)。
//!   `Matched.keyword` は `keyword` きっかけの時だけ `Some`。値は「設定された語」そのまま(大文字小文字を直さない)。
//! - `webhook::rules::Dedup::new(capacity)` / `first_seen(platform, id) -> bool`。
//!   容量を超えたら最も古い物から忘れる。同じ物を見ても順序は更新しない(挿入順の追い出しで足りる)。

use crate::webhook_common::*;
use fast_comment_lib::integrations::WebhookFormat;
use fast_comment_lib::model::{Fragment, MessageKind, Platform};
use fast_comment_lib::webhook::rules::{decide, Dedup};
use fast_comment_lib::webhook::Trigger;

fn all(keywords: &[&str]) -> fast_comment_lib::integrations::WebhookDef {
    def(WebhookFormat::Discord, triggers(true, true, true), keywords)
}

fn kw_only(keywords: &[&str]) -> fast_comment_lib::integrations::WebhookDef {
    def(WebhookFormat::Discord, triggers(false, false, true), keywords)
}

fn trigger_of(def: &fast_comment_lib::integrations::WebhookDef, m: &fast_comment_lib::model::ChatMessage) -> Option<Trigger> {
    decide(def, m).map(|x| x.trigger)
}

#[test]
fn donation_matches_superchat_bits_and_gift() {
    let d = def(WebhookFormat::Discord, triggers(true, false, false), &[]);
    for kind in [MessageKind::SuperChat, MessageKind::Bits, MessageKind::Gift] {
        let m = with_kind("d1", kind, "ありがとう");
        assert_eq!(trigger_of(&d, &m), Some(Trigger::Donation), "{kind:?}");
    }
}

#[test]
fn donation_ignores_normal_and_membership() {
    let d = def(WebhookFormat::Discord, triggers(true, false, false), &[]);
    assert!(decide(&d, &with_kind("n1", MessageKind::Normal, "hi")).is_none());
    assert!(decide(&d, &with_kind("m1", MessageKind::Membership, "hi")).is_none());
}

#[test]
fn membership_matches_membership_only() {
    let d = def(WebhookFormat::Discord, triggers(false, true, false), &[]);
    assert_eq!(trigger_of(&d, &with_kind("m1", MessageKind::Membership, "new")), Some(Trigger::Membership));
    for kind in [MessageKind::Normal, MessageKind::SuperChat, MessageKind::Bits, MessageKind::Gift] {
        assert!(decide(&d, &with_kind("x", kind, "new")).is_none(), "{kind:?}");
    }
}

#[test]
fn keyword_matches_substring_ignoring_ascii_case() {
    let d = kw_only(&["Hello"]);
    let m = decide(&d, &normal("k1", "say HELLO world")).expect("当たる");
    assert_eq!(m.trigger, Trigger::Keyword);
    assert_eq!(m.keyword.as_deref(), Some("Hello"));
}

#[test]
fn keyword_matches_japanese_substring() {
    let d = kw_only(&["質問"]);
    let m = decide(&d, &normal("k1", "これは質問です")).expect("当たる");
    assert_eq!(m.keyword.as_deref(), Some("質問"));
}

#[test]
fn keyword_metacharacters_are_literal() {
    let paren = kw_only(&["(笑)"]);
    assert!(decide(&paren, &normal("a", "面白い(笑)")).is_some());
    assert!(decide(&paren, &normal("b", "面白い笑")).is_none(), "括弧をグループ扱いしない");

    let star = kw_only(&[".*"]);
    assert!(decide(&star, &normal("c", "ここに.*がある")).is_some());
    assert!(decide(&star, &normal("d", "何でもない本文")).is_none(), "`.*` を正規表現として全一致させない");

    let bad = kw_only(&["[", "a++", "\\"]);
    assert!(decide(&bad, &normal("e", "x[y")).is_some(), "壊れた正規表現になる語でも落ちない");
}

#[test]
fn keyword_does_not_match_across_fragments() {
    let d = kw_only(&["質問"]);
    let mut m = normal("f1", "");
    m.fragments = vec![
        Fragment::text("質"),
        Fragment::Emote { id: "e1".into(), name: "".into(), url: "https://example.test/e.png".into() },
        Fragment::text("問"),
    ];
    assert!(decide(&d, &m).is_none());
}

#[test]
fn keyword_does_not_match_emote_name_or_author_name() {
    let d = kw_only(&["質問"]);
    let mut m = normal("g1", "hello");
    m.author.name = "質問者".into();
    m.author.nickname = Some("質問".into());
    m.fragments = vec![
        Fragment::text("hello"),
        Fragment::Emote { id: "e1".into(), name: "質問".into(), url: "https://example.test/e.png".into() },
    ];
    assert!(decide(&d, &m).is_none());
}

#[test]
fn keyword_fullwidth_and_halfwidth_are_distinct() {
    let d = kw_only(&["abc"]);
    assert!(decide(&d, &normal("h1", "ａｂｃ")).is_none());
    let d2 = kw_only(&["ａｂｃ"]);
    assert!(decide(&d2, &normal("h2", "abc")).is_none());
    assert!(decide(&d2, &normal("h3", "ａｂｃ")).is_some());
}

#[test]
fn keyword_hiragana_and_katakana_are_distinct() {
    let d = kw_only(&["しつもん"]);
    assert!(decide(&d, &normal("i1", "シツモン")).is_none());
    assert!(decide(&d, &normal("i2", "しつもんです")).is_some());
}

#[test]
fn blank_keywords_never_match() {
    let d = kw_only(&["", " ", "\u{3000}", "\t"]);
    assert!(decide(&d, &normal("j1", "どんな 本文 でも")).is_none());
    assert!(decide(&d, &normal("j2", "")).is_none());
}

#[test]
fn keyword_trigger_off_ignores_keywords() {
    let d = def(WebhookFormat::Discord, triggers(true, true, false), &["質問"]);
    assert!(decide(&d, &normal("k1", "質問です")).is_none());
}

#[test]
fn superchat_containing_keyword_yields_one_donation_match() {
    let d = all(&["質問"]);
    let mut m = superchat("s1", "¥500");
    m.fragments = vec![Fragment::text("質問です")];
    let got = decide(&d, &m).expect("当たる");
    assert_eq!(got.trigger, Trigger::Donation);
    assert_eq!(got.keyword, None, "投げ銭が優先され、キーワードは付かない");
}

#[test]
fn matched_keyword_reports_the_first_configured_hit() {
    let d = kw_only(&["質問", "初見"]);
    let m = decide(&d, &normal("m1", "初見です。質問があります")).expect("当たる");
    assert_eq!(m.keyword.as_deref(), Some("質問"), "本文での出現順ではなく設定順");
}

#[test]
fn system_kind_never_matches() {
    let d = all(&["質問"]);
    assert!(decide(&d, &with_kind("sys", MessageKind::System, "質問")).is_none());
}

#[test]
fn injected_test_never_matches() {
    let d = all(&["質問"]);
    for kind in [MessageKind::Normal, MessageKind::SuperChat, MessageKind::Membership] {
        let mut m = with_kind("inj", kind, "質問");
        m.injected_test = true;
        assert!(decide(&d, &m).is_none(), "{kind:?}");
    }
}

#[test]
fn skip_tts_message_never_matches() {
    let d = all(&["質問"]);
    for kind in [MessageKind::Normal, MessageKind::SuperChat, MessageKind::Membership, MessageKind::Gift] {
        let mut m = with_kind("skip", kind, "質問");
        m.skip_tts = true;
        assert!(decide(&d, &m).is_none(), "{kind:?}");
    }
}

#[test]
fn disabled_or_paused_webhook_never_matches() {
    let m = superchat("p1", "¥500");
    let mut disabled = all(&[]);
    disabled.enabled = false;
    assert!(decide(&disabled, &m).is_none());

    let mut p = all(&[]);
    p.paused = paused("unauthorized");
    assert!(decide(&p, &m).is_none());
}

#[test]
fn dedup_same_platform_and_id_is_seen_once() {
    let mut d = Dedup::new(8192);
    assert!(d.first_seen(Platform::Youtube, "abc"));
    assert!(!d.first_seen(Platform::Youtube, "abc"));
    assert!(d.first_seen(Platform::Youtube, "abd"));
}

#[test]
fn dedup_same_id_on_other_platform_is_separate() {
    let mut d = Dedup::new(8192);
    assert!(d.first_seen(Platform::Youtube, "same"));
    assert!(d.first_seen(Platform::Twitch, "same"));
    assert!(!d.first_seen(Platform::Twitch, "same"));
    assert!(!d.first_seen(Platform::Youtube, "same"));
}

#[test]
fn dedup_forgets_oldest_beyond_8192() {
    let mut d = Dedup::new(8192);
    for i in 0..=8192 {
        assert!(d.first_seen(Platform::Twitch, &i.to_string()), "初回 {i}");
    }
    // 0..=8192 の 8193 件を入れたので、最古の "0" だけが忘れられている。
    assert!(!d.first_seen(Platform::Twitch, "8192"), "新しい物は覚えている");
    assert!(!d.first_seen(Platform::Twitch, "1"), "2 番目に古い物はまだ覚えている");
    assert!(d.first_seen(Platform::Twitch, "0"), "最古の物は忘れている");
}
