//! PR2 受け入れテスト: ApiComment の形・`GET /comments`・フィード(設計書 §1.3 / §5.4)。
//! 実装を見ずに、設計書と付録 D の公開面だけから書く。

use crate::common::*;
use axum::http::StatusCode;
use fast_comment_lib::api::api_comment_json;
use fast_comment_lib::model::{
    Amount, Badge, Fragment, ListenerInfo, MessageKind, ModerationEvent, Platform,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;

const COMMENTS: &str = "/api/v1/comments";

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("オブジェクト").keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn ids(j: &Value) -> Vec<String> {
    j["items"]
        .as_array()
        .expect("items は配列")
        .iter()
        .map(|i| i["id"].as_str().expect("id").to_string())
        .collect()
}

fn seqs(j: &Value) -> Vec<u64> {
    j["items"].as_array().unwrap().iter().map(|i| i["seq"].as_u64().expect("seq")).collect()
}

fn names(prefix: &str, range: std::ops::RangeInclusive<usize>) -> Vec<String> {
    range.map(|i| format!("{prefix}{i}")).collect()
}

fn push_n(env: &FeedEnv, prefix: &str, range: std::ops::RangeInclusive<usize>) {
    for i in range {
        env.feed.push_chat(&msg(&format!("{prefix}{i}")));
    }
}

/// 全欄を埋めた ChatMessage(内部欄・出さない欄も埋める)。
fn full_message() -> fast_comment_lib::model::ChatMessage {
    let mut m = msg("full-1");
    m.native_id = true;
    m.skip_tts = true;
    m.injected_test = true;
    m.raw = Some(json!({"secret": "raw"}));
    m.listener = Some(ListenerInfo {
        comment_count: 3,
        gift_count: 1,
        first_seen_ms: 10,
        prev_seen_ms: Some(5),
        is_first_ever: false,
        label: Some("常連".to_string()),
        tts_muted: false,
        tts_voice: None,
    });
    m.author.nickname = Some("ありす".to_string());
    m.author.avatar_url = Some("https://example.invalid/a.png".to_string());
    m.author.display_color = Some("#FF7F50".to_string());
    m.author.badges = vec![
        Badge {
            kind: "member".to_string(),
            label: "メンバー".to_string(),
            image_url: Some("https://example.invalid/b.png".to_string()),
        },
        Badge { kind: "highlight".to_string(), label: "highlight".to_string(), image_url: None },
    ];
    m.author.roles.member = true;
    m.fragments = vec![
        Fragment::text("hi "),
        Fragment::Emote {
            id: "e1".to_string(),
            name: "Kappa".to_string(),
            url: "https://example.invalid/e.png".to_string(),
        },
    ];
    m.kind = MessageKind::SuperChat;
    m.amount = Some(Amount { value: 500.0, currency: "JPY".to_string(), raw_text: "¥500".to_string() });
    m
}

// ---- ApiComment の形(api_comment_json) ----

#[test]
fn api_comment_has_exactly_the_documented_keys() {
    let j = api_comment_json(7, &full_message());
    assert_eq!(
        keys(&j),
        set(&[
            "seq", "id", "platform", "channel", "author", "fragments", "text", "kind", "amount",
            "timestampMs", "highlight"
        ])
    );
    assert_eq!(j["seq"], 7);
    assert_eq!(
        keys(&j["author"]),
        set(&["id", "name", "nickname", "avatarUrl", "displayColor", "badges", "roles"])
    );
    assert_eq!(
        keys(&j["author"]["roles"]),
        set(&["broadcaster", "moderator", "member", "subscriber", "vip"])
    );
    assert_eq!(keys(&j["author"]["badges"][0]), set(&["kind", "label", "imageUrl"]));
    assert_eq!(keys(&j["amount"]), set(&["value", "currency", "rawText"]));
    assert_eq!(keys(&j["fragments"][0]), set(&["type", "text"]));
    assert_eq!(keys(&j["fragments"][1]), set(&["type", "id", "name", "url"]));
}

#[test]
fn api_comment_omits_raw_listener_native_id_and_internal_flags() {
    let j = api_comment_json(1, &full_message());
    for k in ["raw", "listener", "nativeId", "skipTts", "injectedTest", "native_id", "skip_tts"] {
        assert!(j.get(k).is_none(), "{k} を出してはいけない: {j}");
    }
}

#[test]
fn api_comment_omits_absent_optional_fields() {
    let mut m = msg("plain-1");
    m.author.badges = vec![Badge { kind: "member".to_string(), label: "m".to_string(), image_url: None }];
    let j = api_comment_json(1, &m);
    assert!(j.get("amount").is_none(), "amount: {j}");
    for k in ["nickname", "avatarUrl", "displayColor"] {
        assert!(j["author"].get(k).is_none(), "author.{k}: {j}");
    }
    assert!(j["author"]["badges"][0].get("imageUrl").is_none(), "badge.imageUrl: {j}");
}

#[test]
fn api_comment_text_joins_text_and_emote_names() {
    let j = api_comment_json(1, &full_message());
    assert_eq!(j["text"], "hi Kappa");
}

#[test]
fn api_comment_highlight_true_and_highlight_badge_removed() {
    let j = api_comment_json(1, &full_message());
    assert_eq!(j["highlight"], true);
    let kinds: Vec<&str> = j["author"]["badges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["member"], "highlight バッジだけが除かれる");

    let no_hl = api_comment_json(2, &msg("plain-2"));
    assert_eq!(no_hl["highlight"], false);
}

#[test]
fn api_comment_kind_and_platform_use_existing_wire_names() {
    let kinds = [
        (MessageKind::Normal, "normal"),
        (MessageKind::SuperChat, "superChat"),
        (MessageKind::Membership, "membership"),
        (MessageKind::Bits, "bits"),
        (MessageKind::Gift, "gift"),
    ];
    for (k, wire) in kinds {
        let mut m = msg("k");
        m.kind = k;
        assert_eq!(api_comment_json(1, &m)["kind"], wire);
    }
    let platforms = [
        (Platform::Twitch, "twitch"),
        (Platform::Youtube, "youtube"),
        (Platform::X, "x"),
        (Platform::Niconico, "niconico"),
    ];
    for (p, wire) in platforms {
        let mut m = msg("p");
        m.platform = p;
        assert_eq!(api_comment_json(1, &m)["platform"], wire);
    }
}

// ---- GET /comments: 件数・カーソル ----

#[tokio::test]
async fn comments_without_after_return_latest_limit_in_ascending_seq() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=150);
    let r = env.get(&format!("{COMMENTS}?limit=50")).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(ids(&j), names("m", 101..=150));
    let s = seqs(&j);
    assert!(s.windows(2).all(|w| w[0] < w[1]), "seq は昇順: {s:?}");
    assert_eq!(j["gap"], false);
    let epoch = env.epoch().await;
    assert_eq!(j["nextCursor"], format!("{epoch}.{}", s.last().unwrap()));
}

#[tokio::test]
async fn comments_default_limit_is_100() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=150);
    let j = env.get(COMMENTS).await.json();
    assert_eq!(ids(&j), names("m", 51..=150));
}

#[tokio::test]
async fn comments_limit_0_and_501_are_400_and_500_is_ok() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=600);
    for bad in ["0", "501"] {
        let r = env.get(&format!("{COMMENTS}?limit={bad}")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "limit={bad}");
        assert_eq!(r.error_code(), "invalid_request", "limit={bad}");
    }
    let ok = env.get(&format!("{COMMENTS}?limit=500")).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ids(&ok.json()), names("m", 101..=600));
}

#[tokio::test]
async fn comments_after_returns_only_newer_items_oldest_first() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=5);
    let epoch = env.epoch().await;
    let all = env.get(COMMENTS).await.json();
    let second = seqs(&all)[1];
    let r = env.get(&format!("{COMMENTS}?after={epoch}.{second}")).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(ids(&j), names("m", 3..=5));
    assert_eq!(j["gap"], false);
    assert_eq!(j["nextCursor"], format!("{epoch}.{}", seqs(&j).last().unwrap()));
}

#[tokio::test]
async fn comments_after_head_returns_empty_and_same_cursor() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=3);
    let head = env.get(COMMENTS).await.json()["nextCursor"].as_str().unwrap().to_string();
    let j = env.get(&format!("{COMMENTS}?after={head}")).await.json();
    assert_eq!(j["items"], json!([]));
    assert_eq!(j["nextCursor"], head);
    assert_eq!(j["gap"], false);
}

#[tokio::test]
async fn comments_paging_by_next_cursor_has_no_gap_or_duplicate() {
    let env = feed_env(1000);
    // `after` なしは「最新 limit 件」なので、起点カーソルは先頭の 1 件から取る。
    env.feed.push_chat(&msg("seed"));
    let mut cursor = env.get(COMMENTS).await.json()["nextCursor"].as_str().unwrap().to_string();
    push_n(&env, "m", 1..=250);

    let mut collected = Vec::new();
    for expected_len in [100usize, 100, 50] {
        let j = env.get(&format!("{COMMENTS}?after={cursor}&limit=100")).await.json();
        assert_eq!(j["gap"], false);
        assert_eq!(ids(&j).len(), expected_len);
        collected.extend(ids(&j));
        cursor = j["nextCursor"].as_str().unwrap().to_string();
    }
    assert_eq!(collected, names("m", 1..=250));
}

#[tokio::test]
async fn comments_empty_feed_without_after_has_null_next_cursor() {
    let env = feed_env(1000);
    let j = env.get(COMMENTS).await.json();
    assert_eq!(j["items"], json!([]));
    assert!(j["nextCursor"].is_null(), "nextCursor: {j}");
    assert_eq!(j["gap"], false);
}

#[tokio::test]
async fn comments_malformed_cursor_is_400() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=3);
    for bad in ["abc", ".5", "a1b2c3d4e5f6.x", "a1b2c3d4e5f6.99999999999999999999999"] {
        let r = env.get(&format!("{COMMENTS}?after={bad}")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "after={bad}");
        assert_eq!(r.error_code(), "invalid_request", "after={bad}");
    }
}

#[tokio::test]
async fn comments_cursor_from_other_epoch_returns_tail_with_gap_true() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=150);
    let epoch = env.epoch().await;
    let other = if epoch == "ffffffffffff" { "000000000000" } else { "ffffffffffff" };
    let tail = env.get(COMMENTS).await.json();
    let j = env.get(&format!("{COMMENTS}?after={other}.5")).await.json();
    assert_eq!(j["gap"], true);
    assert_eq!(ids(&j), ids(&tail), "after なしと同じ結果");
    assert_eq!(j["nextCursor"], tail["nextCursor"]);
}

#[tokio::test]
async fn comments_cursor_older_than_buffer_sets_gap_true() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=100);
    let cursor_100 = env.get(COMMENTS).await.json()["nextCursor"].as_str().unwrap().to_string();
    push_n(&env, "m", 101..=1500);

    let j = env.get(&format!("{COMMENTS}?after={cursor_100}&limit=500")).await.json();
    assert_eq!(j["gap"], true);
    // バッファに残る最古(501 番目)から古い順に返る
    assert_eq!(ids(&j), names("m", 501..=1000));
}

#[tokio::test]
async fn comments_buffer_keeps_only_latest_1000() {
    let env = feed_env(1000);
    env.feed.push_chat(&msg("seed"));
    let start = env.get(COMMENTS).await.json()["nextCursor"].as_str().unwrap().to_string();
    push_n(&env, "m", 1..=1499);

    let mut collected = Vec::new();
    let mut cursor = start;
    loop {
        let j = env.get(&format!("{COMMENTS}?after={cursor}&limit=500")).await.json();
        let page = ids(&j);
        if page.is_empty() {
            break;
        }
        collected.extend(page);
        cursor = j["nextCursor"].as_str().unwrap().to_string();
    }
    assert_eq!(collected.len(), 1000);
    assert_eq!(collected, names("m", 500..=1499));
}

// ---- GET /comments: 絞り込み ----

#[tokio::test]
async fn comments_filter_by_channel_is_exact_match() {
    let env = feed_env(1000);
    for (id, ch) in [("a", "ab"), ("b", "abc"), ("c", "AB"), ("d", "ab"), ("e", "a")] {
        let mut m = msg(id);
        m.channel = ch.to_string();
        env.feed.push_chat(&m);
    }
    let j = env.get(&format!("{COMMENTS}?channel=ab")).await.json();
    assert_eq!(ids(&j), vec!["a", "d"]);
}

#[tokio::test]
async fn comments_filter_by_kind_list() {
    let env = feed_env(1000);
    for (id, k) in [
        ("n", MessageKind::Normal),
        ("s", MessageKind::SuperChat),
        ("m", MessageKind::Membership),
        ("b", MessageKind::Bits),
    ] {
        let mut m = msg(id);
        m.kind = k;
        env.feed.push_chat(&m);
    }
    let j = env.get(&format!("{COMMENTS}?kind=superChat,bits")).await.json();
    assert_eq!(ids(&j), vec!["s", "b"]);
}

#[tokio::test]
async fn comments_unknown_kind_is_400() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=2);
    for q in ["kind=bogus", "kind=superChat,bogus"] {
        let r = env.get(&format!("{COMMENTS}?{q}")).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(r.error_code(), "invalid_request", "{q}");
    }
}

// ---- モデレーションの反映 ----

fn at(id: &str, author: &str, ts: i64) -> fast_comment_lib::model::ChatMessage {
    let mut m = msg(id);
    m.author.id = author.to_string();
    m.timestamp_ms = ts;
    m
}

#[tokio::test]
async fn comments_drop_message_deleted_by_id() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=3);
    env.feed.push_moderation(&ModerationEvent::MessageDeleted {
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        message_id: "m2".to_string(),
    });
    assert_eq!(ids(&env.get(COMMENTS).await.json()), vec!["m1", "m3"]);
}

#[tokio::test]
async fn comments_drop_user_cleared_up_to_before_ms() {
    let env = feed_env(1000);
    for m in [
        at("early", "u1", 1000),
        at("edge", "u1", 2000),
        at("late", "u1", 2001),
        at("other-user", "u2", 1000),
    ] {
        env.feed.push_chat(&m);
    }
    env.feed.push_moderation(&ModerationEvent::UserCleared {
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        author_id: "u1".to_string(),
        duration_secs: Some(600),
        certain: true,
        before_ms: Some(2000),
    });
    assert_eq!(ids(&env.get(COMMENTS).await.json()), vec!["late", "other-user"]);
}

#[tokio::test]
async fn comments_drop_chat_cleared_up_to_before_ms() {
    let env = feed_env(1000);
    for m in [at("early", "u1", 1000), at("edge", "u2", 2000), at("late", "u3", 2001)] {
        env.feed.push_chat(&m);
    }
    env.feed.push_moderation(&ModerationEvent::ChatCleared {
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        before_ms: Some(2000),
    });
    assert_eq!(ids(&env.get(COMMENTS).await.json()), vec!["late"]);
}

#[tokio::test]
async fn comments_moderation_for_other_channel_or_platform_removes_nothing() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=3);
    let events = [
        ModerationEvent::MessageDeleted {
            platform: Platform::Twitch,
            channel: "other".to_string(),
            message_id: "m1".to_string(),
        },
        ModerationEvent::MessageDeleted {
            platform: Platform::Youtube,
            channel: "chan".to_string(),
            message_id: "m1".to_string(),
        },
        ModerationEvent::UserCleared {
            platform: Platform::Twitch,
            channel: "other".to_string(),
            author_id: "u1".to_string(),
            duration_secs: None,
            certain: true,
            before_ms: None,
        },
        ModerationEvent::ChatCleared {
            platform: Platform::Youtube,
            channel: "chan".to_string(),
            before_ms: None,
        },
    ];
    for e in &events {
        env.feed.push_moderation(e);
    }
    assert_eq!(ids(&env.get(COMMENTS).await.json()), vec!["m1", "m2", "m3"]);
}

#[tokio::test]
async fn comments_same_id_redelivery_is_appended_with_new_seq() {
    let env = feed_env(1000);
    env.feed.push_chat(&msg("dup"));
    let mut again = msg("dup");
    again.fragments = vec![Fragment::text("updated")];
    env.feed.push_chat(&again);
    let j = env.get(COMMENTS).await.json();
    assert_eq!(ids(&j), vec!["dup", "dup"]);
    let s = seqs(&j);
    assert!(s[0] < s[1], "後着は新しい seq: {s:?}");
    assert_eq!(j["items"][1]["text"], "updated");
}

// ---- 有効 / 無効 ----

#[tokio::test]
async fn feed_disabled_buffers_nothing() {
    let env = feed_env(1000);
    env.feed.set_enabled(false);
    push_n(&env, "m", 1..=3);
    env.feed.push_moderation(&ModerationEvent::ChatCleared {
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        before_ms: None,
    });
    env.feed.set_enabled(true);
    let j = env.get(COMMENTS).await.json();
    assert_eq!(j["items"], json!([]));
}

#[tokio::test]
async fn feed_reenable_starts_empty_and_seq_keeps_increasing() {
    let env = feed_env(1000);
    env.feed.push_chat(&msg("before"));
    let first_seq = seqs(&env.get(COMMENTS).await.json())[0];
    let epoch = env.epoch().await;

    env.feed.set_enabled(false);
    env.feed.push_chat(&msg("while-off"));
    env.feed.set_enabled(true);
    assert_eq!(env.get(COMMENTS).await.json()["items"], json!([]), "有効化で空から始まる");

    env.feed.push_chat(&msg("after"));
    let j = env.get(COMMENTS).await.json();
    assert_eq!(ids(&j), vec!["after"]);
    assert!(seqs(&j)[0] > first_seq, "seq は巻き戻らない");
    assert_eq!(env.epoch().await, epoch, "epoch は起動中変わらない");
}

// ---- /status と認証 ----

#[tokio::test]
async fn status_head_cursor_tracks_latest_item() {
    let env = feed_env(1000);
    let empty = env.get("/api/v1/status").await.json();
    assert!(empty["headCursor"].is_null(), "0 件なら null: {empty}");

    let epoch = env.epoch().await;
    env.feed.push_chat(&msg("a"));
    let s1 = seqs(&env.get(COMMENTS).await.json())[0];
    assert_eq!(env.get("/api/v1/status").await.json()["headCursor"], format!("{epoch}.{s1}"));

    env.feed.push_chat(&msg("b"));
    let s2 = *seqs(&env.get(COMMENTS).await.json()).last().unwrap();
    assert!(s2 > s1);
    assert_eq!(env.get("/api/v1/status").await.json()["headCursor"], format!("{epoch}.{s2}"));
}

#[tokio::test]
async fn comments_and_events_require_auth() {
    let env = feed_env(1000);
    push_n(&env, "m", 1..=1);
    for path in [COMMENTS, "/api/v1/events"] {
        let none = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            send(&env.app, get(path)),
        )
        .await
        .expect("401 はストリームを開かず即返る");
        assert_eq!(none.status, StatusCode::UNAUTHORIZED, "{path} トークンなし");
        assert_eq!(none.error_code(), "unauthorized", "{path}");

        let wrong = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            send(&env.app, get(path).bearer(&fast_comment_lib::api::generate_token())),
        )
        .await
        .expect("401 はストリームを開かず即返る");
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{path} 違うトークン");
    }
}
