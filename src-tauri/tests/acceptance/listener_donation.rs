//! ユーザー別ギフト金額(.omc/plans/listener-donation.md)。実装者は編集しない。
use crate::common::msg;
use fast_comment_lib::achievements::currency::RateTable;
use fast_comment_lib::listeners::{donation_summary, ListenerStore};
use fast_comment_lib::model::{Amount, ChatMessage, MessageKind};
use std::collections::BTreeMap;

fn paid(id: &str, author: &str, kind: MessageKind, value: f64, currency: &str) -> ChatMessage {
    let mut m = msg(id);
    m.author.id = author.to_string();
    m.kind = kind;
    m.amount = Some(Amount {
        value,
        currency: currency.to_string(),
        raw_text: format!("{value} {currency}"),
    });
    m
}

fn observe(store: &ListenerStore, mut m: ChatMessage) {
    store.observe(&mut m);
}

fn donation_of(store: &ListenerStore, author: &str) -> BTreeMap<String, i64> {
    store.get("twitch", author).expect("listener exists").donation
}

fn map(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[test]
fn super_chat_bits_and_gift_amounts_accumulate_per_currency() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 500.0, "JPY"));
    observe(&s, paid("b", "u1", MessageKind::SuperChat, 1000.0, "JPY"));
    observe(&s, paid("c", "u1", MessageKind::Bits, 100.0, "BITS"));
    observe(&s, paid("d", "u1", MessageKind::Gift, 4.99, "USD"));
    assert_eq!(
        donation_of(&s, "u1"),
        map(&[("BITS", 100_000), ("JPY", 1_500_000), ("USD", 4_990)])
    );
}

#[test]
fn subgift_membership_normal_and_missing_amount_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::Gift, 5.0, "SUBGIFT"));
    observe(&s, paid("b", "u1", MessageKind::Membership, 490.0, "JPY"));
    observe(&s, paid("c", "u1", MessageKind::Normal, 300.0, "JPY"));
    let mut none = msg("d");
    none.author.id = "u1".to_string();
    none.kind = MessageKind::Gift;
    none.amount = None;
    observe(&s, none);
    assert!(donation_of(&s, "u1").is_empty());
}

#[test]
fn non_finite_zero_or_negative_values_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    for (i, v) in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -5.0].into_iter().enumerate() {
        observe(&s, paid(&format!("m{i}"), "u1", MessageKind::SuperChat, v, "JPY"));
    }
    assert!(donation_of(&s, "u1").is_empty());
}

#[test]
fn currency_symbol_is_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 1.0, "¥"));
    observe(&s, paid("b", "u2", MessageKind::SuperChat, 2.0, "$"));
    observe(&s, paid("c", "u3", MessageKind::SuperChat, 3.0, " jpy "));
    // normalize_currency("$") は "USD"(known_symbol の表)。
    assert_eq!(donation_of(&s, "u1"), map(&[("JPY", 1_000)]));
    assert_eq!(donation_of(&s, "u2"), map(&[("USD", 2_000)]));
    assert_eq!(donation_of(&s, "u3"), map(&[("JPY", 3_000)]));
}

#[test]
fn saturating_add_never_wraps() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 9e15, "JPY"));
    assert_eq!(donation_of(&s, "u1"), map(&[("JPY", 9_000_000_000_000_000_000)]));
    observe(&s, paid("b", "u1", MessageKind::SuperChat, 9e15, "JPY"));
    assert_eq!(donation_of(&s, "u1"), map(&[("JPY", i64::MAX)]));
}

#[test]
fn old_listeners_json_without_donation_loads_and_keeps_fields() {
    let dir = tempfile::tempdir().unwrap();
    let old = r#"{"twitch:u1":{"platform":"twitch","authorId":"u1","name":"Alice","nickname":"Ali","memo":"memo","commentCount":7,"giftCount":2,"firstSeenMs":10,"lastSeenMs":20,"ttsMuted":true},"twitch:u2":{"platform":"twitch","authorId":"u2","name":"Bob"}}"#;
    std::fs::write(dir.path().join("listeners.json"), old).unwrap();
    let s = ListenerStore::load(dir.path());
    // observe は呼ばない。別の人を delete して dirty にし、u1 を含む全体を書き直させる。
    assert!(s.delete("twitch", "u2"));
    s.flush().unwrap();
    let r = ListenerStore::load(dir.path()).get("twitch", "u1").unwrap();
    assert_eq!(r.name, "Alice");
    assert_eq!(r.nickname.as_deref(), Some("Ali"));
    assert_eq!(r.memo.as_deref(), Some("memo"));
    assert_eq!(r.comment_count, 7);
    assert_eq!(r.gift_count, 2);
    assert_eq!(r.first_seen_ms, 10);
    assert_eq!(r.last_seen_ms, 20);
    assert!(r.tts_muted);
    assert!(r.donation.is_empty());
}

#[test]
fn donation_persists_across_flush_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 500.0, "JPY"));
    observe(&s, paid("b", "u1", MessageKind::Bits, 100.0, "BITS"));
    s.flush().unwrap();
    let r = ListenerStore::load(dir.path());
    assert_eq!(donation_of(&r, "u1"), map(&[("BITS", 100_000), ("JPY", 500_000)]));
}

#[test]
fn reset_donation_clears_one_listener_only() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 500.0, "JPY"));
    observe(&s, paid("b", "u2", MessageKind::SuperChat, 700.0, "JPY"));
    assert!(s.reset_donation("twitch", "u1"));
    assert!(donation_of(&s, "u1").is_empty());
    assert_eq!(donation_of(&s, "u2"), map(&[("JPY", 700_000)]));
    assert_eq!(s.get("twitch", "u1").unwrap().comment_count, 1);
}

#[test]
fn reset_donation_unknown_listener_is_false() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    assert!(!s.reset_donation("twitch", "nobody"));
}

#[test]
fn reset_all_donations_returns_cleared_count() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 500.0, "JPY"));
    observe(&s, paid("b", "u2", MessageKind::Bits, 50.0, "BITS"));
    let mut plain = msg("c");
    plain.author.id = "u3".to_string();
    observe(&s, plain);
    assert_eq!(s.reset_all_donations(), 2);
    assert!(donation_of(&s, "u1").is_empty());
    assert!(donation_of(&s, "u2").is_empty());
    assert_eq!(s.reset_all_donations(), 0);
}

#[test]
fn delete_removes_donation() {
    let dir = tempfile::tempdir().unwrap();
    let s = ListenerStore::load(dir.path());
    observe(&s, paid("a", "u1", MessageKind::SuperChat, 500.0, "JPY"));
    assert!(s.delete("twitch", "u1"));
    observe(&s, msg("b"));
    assert!(donation_of(&s, "u1").is_empty());
}

#[test]
fn summary_orders_by_amount_and_estimates_jpy() {
    let rates: RateTable = [("JPY".to_string(), 1.0), ("USD".to_string(), 150.0)].into();
    let d = map(&[("JPY", 1_000_000), ("USD", 10_000), ("BITS", 500_000)]);
    let s = donation_summary(&d, &rates, &BTreeMap::new());
    assert_eq!(
        s.totals,
        vec![
            ("JPY".to_string(), 1_000_000),
            ("BITS".to_string(), 500_000),
            ("USD".to_string(), 10_000),
        ]
    );
    assert_eq!(s.jpy_estimate_milli, Some(2_500_000));
    assert_eq!(s.unconvertible, vec!["BITS".to_string()]);
}

#[test]
fn summary_without_any_convertible_currency_is_none() {
    let rates: RateTable = RateTable::new();
    let d = map(&[("BITS", 500_000), ("PT", 1_500_000)]);
    let s = donation_summary(&d, &rates, &BTreeMap::new());
    assert_eq!(s.jpy_estimate_milli, None);
    assert_eq!(s.unconvertible, vec!["BITS".to_string(), "PT".to_string()]);
}
