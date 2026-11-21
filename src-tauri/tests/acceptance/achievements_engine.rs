// registered in main.rs by story ACH-002
//! 評価エンジンと進捗ストア(acceptance.md §3.4、D1〜D19)。時刻はすべて `now_ms` 引数。
use crate::achievements_common::*;
use fast_comment_lib::achievements::engine::{
    is_shared_anonymous, AchievementEngine, AchievementUnlocked, DEDUP_CAPACITY, EPOCH_IDLE_MS, STALE_SESSION_MS,
};
use fast_comment_lib::achievements::model::{CurrencyMode, Metric, SubjectKind, Window};
use fast_comment_lib::achievements::store::{delete_listener_progress, PROGRESS_FILE};
use fast_comment_lib::model::{ChatMessage, ListenerInfo, MessageKind, Platform};
use fast_comment_lib::stats::{ChannelStatus, StatsSnapshot, ViewerCountKind};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Barrier};

fn open(dir: &Path, list: Vec<fast_comment_lib::achievements::model::Achievement>, now: u64) -> AchievementEngine {
    AchievementEngine::open(dir, mk_defs(list), now)
}

/// `n` 件の通常コメント(id は `{prefix}{i}`)を同じ時刻で流し、解除を全部集める。
fn feed(e: &AchievementEngine, prefix: &str, n: usize, now: u64) -> Vec<AchievementUnlocked> {
    let mut out = vec![];
    for i in 0..n {
        out.extend(e.observe(&cm(&format!("{prefix}{i}")), now));
    }
    out
}

fn progress(dir: &Path) -> Value {
    read_json(&dir.join(PROGRESS_FILE))
}

fn contains_zero_number(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.iter().any(contains_zero_number),
        Value::Object(o) => o.values().any(contains_zero_number),
        _ => false,
    }
}

// ---------------- 起点と保存 ----------------

#[test]
fn enabled_at_set_on_first_open_and_kept_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c", Metric::Comments, 100.0)], T0);
    assert_eq!(e.enabled_at_ms(), T0);
    e.observe(&cm("m1"), T0 + 10);
    e.flush().unwrap();
    drop(e);
    let e2 = open(dir.path(), vec![viewer_count("c", Metric::Comments, 100.0)], T0 + 5_000);
    assert_eq!(e2.enabled_at_ms(), T0);
}

#[test]
fn counts_start_at_enable_ignoring_listener_totals() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c3", Metric::Comments, 3.0)], T0);
    let info: ListenerInfo = serde_json::from_value(json!({
        "commentCount": 9999, "giftCount": 50, "firstSeenMs": 1, "isFirstEver": false, "ttsMuted": false
    }))
    .expect("ListenerInfo を JSON から作れる");
    let mut all = vec![];
    for i in 0..2 {
        let mut m = cm(&format!("m{i}"));
        m.listener = Some(info.clone());
        all.extend(e.observe(&m, T0 + 1 + i as u64));
    }
    assert!(all.is_empty(), "既存の累計 9999 を読んではいけない");
    let mut third = cm("m2");
    third.listener = Some(info);
    assert_eq!(ids(&e.observe(&third, T0 + 10)), vec!["c3"]);
}

#[test]
fn counters_survive_flush_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let list = || vec![viewer_count("c3", Metric::Comments, 3.0)];
    let e = open(dir.path(), list(), T0);
    assert!(feed(&e, "a", 2, T0 + 1).is_empty());
    e.flush().unwrap();
    drop(e);
    let e2 = open(dir.path(), list(), T0 + 100);
    assert_eq!(ids(&e2.observe(&cm("b0"), T0 + 101)), vec!["c3"]);
}

#[test]
fn unlock_not_repeated_after_flush_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let list = || vec![viewer_count("c2", Metric::Comments, 2.0)];
    let e = open(dir.path(), list(), T0);
    assert_eq!(ids(&feed(&e, "a", 2, T0 + 1)), vec!["c2"]);
    e.flush().unwrap();
    drop(e);
    let e2 = open(dir.path(), list(), T0 + 100);
    assert!(feed(&e2, "b", 3, T0 + 101).is_empty());
}

#[test]
fn progress_v1_fixture_loads_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = r#"{"version":1,"enabledAtMs":1000,
      "epoch":{"key":"e:1000","openedAtMs":1000,"lastActivityMs":5000,"uniqueCommenters":["youtube:UC1"],
               "streamer":{"comments":4,"peakConcurrentViewers":12}},
      "sessions":{"twitcasting:42":{"lastActivityMs":5000,"ended":false}},
      "seenStreams":["twitcasting:42"],
      "streamer":{"comments":10,"donation":{"JPY":500000,"BITS":100000},"streams":1},
      "viewers":{"youtube:UC1":{"comments":3,"superChatCount":1,"donation":{"¥":500000},
                 "streamsAttended":1,"lastEpoch":"e:1000","epochComments":1}},
      "unlocks":{"viewer-first-comment|youtube:UC1|*":1500}}"#;
    std::fs::write(dir.path().join(PROGRESS_FILE), fixture).unwrap();
    let e = open(
        dir.path(),
        vec![viewer_count("viewer-first-comment", Metric::Comments, 1.0), viewer_count("c5", Metric::Comments, 5.0)],
        6000,
    );
    assert_eq!(e.enabled_at_ms(), 1000);
    let recs = e.listener_unlocks("youtube", "UC1");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].achievement_id, "viewer-first-comment");
    assert_eq!(recs[0].window_key, "*");
    assert_eq!(recs[0].unlocked_at_ms, 1500);

    let m1 = chat("y1", Platform::Youtube, "UCchan", "UC1");
    let m2 = chat("y2", Platform::Youtube, "UCchan", "UC1");
    assert!(e.observe(&m1, 6000).is_empty());
    assert_eq!(ids(&e.observe(&m2, 6001)), vec!["c5"], "3 + 2 = 5 で解除。既存の解除は再通知しない");
}

#[test]
fn progress_flush_matches_v1_shape() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("first", Metric::Comments, 1.0)], T0);
    let sc = super_chat("sc1", "UC1", 500.0, "¥");
    assert_eq!(ids(&e.observe(&sc, T0 + 500)), vec!["first"]);
    e.flush().unwrap();
    let p = progress(dir.path());
    assert_eq!(p["version"], 1);
    assert_eq!(p["enabledAtMs"], T0);
    assert_eq!(p["epoch"]["key"], format!("e:{}", T0 + 500));
    assert!(p["epoch"]["uniqueCommenters"].as_array().unwrap().contains(&json!("youtube:UC1")));
    let v = &p["viewers"]["youtube:UC1"];
    assert_eq!(v["comments"].as_i64(), Some(1));
    assert_eq!(v["superChatCount"].as_i64(), Some(1));
    assert_eq!(v["donation"]["¥"].as_i64(), Some(500_000), "金額はミリ単位の整数");
    assert_eq!(p["streamer"]["comments"].as_i64(), Some(1));
    assert_eq!(p["unlocks"]["first|youtube:UC1|*"].as_u64(), Some(T0 + 500));
}

#[test]
fn progress_omits_zero_counters() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    e.observe(&cm("m1"), T0 + 1);
    e.flush().unwrap();
    let p = progress(dir.path());
    let v = &p["viewers"]["twitch:u1"];
    assert!(v.get("superChatCount").is_none(), "0 のカウンタは書かない");
    assert!(v.get("donation").is_none());
    assert!(p["streamer"].get("donation").is_none());
    assert!(p["streamer"].get("streams").is_none());
    assert!(!contains_zero_number(&p), "どこにも 0 を書かない");
}

#[test]
fn progress_unknown_version_backed_up_as_broken_and_empty() {
    let dir = tempfile::tempdir().unwrap();
    let original = br#"{"version":99,"enabledAtMs":1,"unlocks":{"c2|twitch:u1|*":5}}"#.to_vec();
    std::fs::write(dir.path().join(PROGRESS_FILE), &original).unwrap();
    let now = 7_000_000u64;
    let e = open(dir.path(), vec![viewer_count("c2", Metric::Comments, 2.0)], now);
    let backups = files_with_prefix(dir.path(), &format!("{PROGRESS_FILE}.broken-"));
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0].file_name().unwrap().to_string_lossy(), format!("{PROGRESS_FILE}.broken-{now}"));
    assert_eq!(read(&backups[0]), original);
    assert_eq!(e.enabled_at_ms(), now, "空で開き直すので起点は now");
    assert!(e.listener_unlocks("twitch", "u1").is_empty());
}

#[test]
fn progress_backup_failure_refuses_flush_and_keeps_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let original = b"{broken progress".to_vec();
    std::fs::write(dir.path().join(PROGRESS_FILE), &original).unwrap();
    let io = TestIo::new();
    io.fail_backups(true);
    let e = AchievementEngine::open_with(dir.path(), mk_defs(vec![viewer_count("c1", Metric::Comments, 1.0)]), T0, io.clone());
    assert!(e.observe(&cm("m1"), T0 + 1).is_empty(), "保存拒否の間は解除を返さない");
    assert!(e.flush().is_err(), "flush は Err");
    assert_eq!(read(&dir.path().join(PROGRESS_FILE)), original, "原本のバイトが同じ");
    assert_eq!(io.write_attempts(), 0);
}

// ---------------- 1 回だけ(D12) ----------------

#[test]
fn comment_threshold_unlocks_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c3", Metric::Comments, 3.0)], T0);
    let mut per_call = vec![];
    for i in 0..6 {
        per_call.push(e.observe(&cm(&format!("m{i}")), T0 + 1 + i).len());
    }
    assert_eq!(per_call, vec![0, 0, 1, 0, 0, 0]);
}

#[test]
fn concurrent_observes_cross_threshold_unlock_once() {
    for round in 0..20u64 {
        let dir = tempfile::tempdir().unwrap();
        let e = Arc::new(open(dir.path(), vec![viewer_count("c2", Metric::Comments, 2.0)], T0));
        let barrier = Arc::new(Barrier::new(2));
        let total: usize = std::thread::scope(|s| {
            let hs: Vec<_> = (0..2)
                .map(|t| {
                    let (e, b) = (e.clone(), barrier.clone());
                    s.spawn(move || {
                        b.wait();
                        e.observe(&cm(&format!("r{round}-t{t}")), T0 + 1).len()
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(total, 1, "round {round}: 同時に閾値をまたいでも解除は 1 回");
    }
}

#[test]
fn unlock_returned_only_after_record_written() {
    let dir = tempfile::tempdir().unwrap();
    let io = TestIo::new();
    let e = AchievementEngine::open_with(dir.path(), mk_defs(vec![viewer_count("c2", Metric::Comments, 2.0)]), T0, io.clone());
    assert!(e.observe(&cm("m1"), T0 + 1).is_empty());
    let got = e.observe(&cm("m2"), T0 + 2);
    assert_eq!(got.len(), 1);
    let saved = io.last_text_of(PROGRESS_FILE).expect("返る前に進捗を書いている");
    assert!(saved.contains("c2|twitch:u1|*"), "解除キーを含む bytes を書いてから返す");
}

#[test]
fn persist_failure_suppresses_unlock_and_retries_next_event() {
    let dir = tempfile::tempdir().unwrap();
    let io = TestIo::new();
    let e = AchievementEngine::open_with(dir.path(), mk_defs(vec![viewer_count("c1", Metric::Comments, 1.0)]), T0, io.clone());
    io.fail_next_writes(1);
    assert!(e.observe(&cm("m1"), T0 + 1).is_empty(), "保存に失敗したら通知しない");
    assert!(e.listener_unlocks("twitch", "u1").is_empty(), "キーは外される");
    let retry = e.observe(&cm("m2"), T0 + 2);
    assert_eq!(ids(&retry), vec!["c1"], "次のイベントで再評価して 1 件");
    assert_eq!(e.listener_unlocks("twitch", "u1").len(), 1);
}

// ---------------- 計上の除外と重複(D1・D11・D15) ----------------

#[test]
fn uncounted_route_messages_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c1", Metric::Comments, 1.0)], T0);
    assert!(e.observe(&uncounted(cm("h1")), T0 + 1).is_empty());
    assert!(e.observe(&uncounted(cm("h2")), T0 + 2).is_empty());
    assert_eq!(ids(&e.observe(&cm("n1"), T0 + 3)), vec!["c1"]);
}

#[test]
fn system_injected_and_empty_author_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c2", Metric::Comments, 2.0)], T0);
    let mut sys = cm("s1");
    sys.kind = MessageKind::System;
    let mut inj = cm("s2");
    inj.injected_test = true;
    let noauthor = cm_by("s3", "");
    assert!(e.observe(&sys, T0 + 1).is_empty());
    assert!(e.observe(&inj, T0 + 2).is_empty());
    assert!(e.observe(&noauthor, T0 + 3).is_empty());
    assert!(e.observe(&cm("n1"), T0 + 4).is_empty(), "3 件は数えていないので 1 件目");
    assert_eq!(ids(&e.observe(&cm("n2"), T0 + 5)), vec!["c2"]);
}

#[test]
fn duplicate_native_id_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c2", Metric::Comments, 2.0)], T0);
    let mut a = cm("dup");
    a.native_id = true;
    assert!(e.observe(&a, T0 + 1).is_empty());
    assert!(e.observe(&a, T0 + 2).is_empty(), "同じ ID は数えない");
    assert_eq!(ids(&e.observe(&cm("other"), T0 + 3)), vec!["c2"]);
}

#[test]
fn stable_non_native_id_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("g2", Metric::GiftCount, 2.0)], T0);
    let mut g = chat("tc-gift-7", Platform::Twitcasting, "alice", "tc:bob");
    g.kind = MessageKind::Gift;
    g.native_id = false;
    assert!(e.observe(&g, T0 + 1).is_empty());
    assert!(e.observe(&g, T0 + 2).is_empty());
    let mut g8 = chat("tc-gift-8", Platform::Twitcasting, "alice", "tc:bob");
    g8.kind = MessageKind::Gift;
    assert_eq!(ids(&e.observe(&g8, T0 + 3)), vec!["g2"]);
}

#[test]
fn youtube_combo_update_same_id_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("g2", Metric::GiftCount, 2.0)], T0);
    let mut combo = chat("yt-combo-1", Platform::Youtube, "UCchan", "UC1");
    combo.kind = MessageKind::Gift;
    combo.native_id = false;
    assert!(e.observe(&combo, T0 + 1).is_empty());
    assert!(e.observe(&combo, T0 + 2).is_empty(), "コンボ更新は同じ ID なので 1 回");
    let mut next = chat("yt-combo-2", Platform::Youtube, "UCchan", "UC1");
    next.kind = MessageKind::Gift;
    assert_eq!(ids(&e.observe(&next, T0 + 3)), vec!["g2"]);
}

#[test]
fn history_catchup_combo_new_sequence_counts() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_count("comments2", Metric::Comments, 2.0),
            viewer_count("comments3", Metric::Comments, 3.0),
            viewer_count("gifts1", Metric::GiftCount, 1.0),
            viewer_count("gifts2", Metric::GiftCount, 2.0),
        ],
        T0,
    );
    let gift = |id: &str| {
        let mut m = chat(id, Platform::Youtube, "UCchan", "UC1");
        m.kind = MessageKind::Gift;
        m
    };
    let mut got = vec![];
    got.extend(e.observe(&uncounted(gift("g1")), T0 + 1)); // 1 履歴
    got.extend(e.observe(&gift("g1"), T0 + 2)); // 2 追い上げ(ID は既出)
    assert!(got.is_empty());
    got.extend(e.observe(&gift("g2"), T0 + 3)); // 3 ギフト
    got.extend(e.observe(&uncounted(gift("g2")), T0 + 4)); // 4 コンボ更新
    got.extend(e.observe(&chat("n1", Platform::Youtube, "UCchan", "UC1"), T0 + 5)); // 5 新着
    assert_eq!(ids(&got), vec!["comments2", "gifts1"], "Comments +2、GiftCount +1");
}

#[test]
fn dedup_scope_is_per_platform_channel() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            streamer_count("s3", Window::Lifetime, Metric::Comments, 3.0),
            streamer_count("s4", Window::Lifetime, Metric::Comments, 4.0),
        ],
        T0,
    );
    let a = chat("same", Platform::Twitch, "chan", "u1");
    let b = chat("same", Platform::Twitch, "other", "u1");
    let c = chat("same", Platform::Youtube, "chan", "u1");
    assert!(e.observe(&a, T0 + 1).is_empty());
    assert!(e.observe(&b, T0 + 2).is_empty());
    assert_eq!(ids(&e.observe(&c, T0 + 3)), vec!["s3"], "別の (platform, channel) なら同じ ID でも数える");
    assert!(e.observe(&a, T0 + 4).is_empty(), "同じ範囲での再受信は数えない");
}

#[test]
fn two_anonymous_viewers_no_viewer_unlock_but_streamer_counts() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_count("v2", Metric::Comments, 2.0),
            streamer_count("s2", Window::Lifetime, Metric::Comments, 2.0),
        ],
        T0,
    );
    let mut a = chat("a1", Platform::Twitcasting, "alice", "tc:anonymous");
    a.author.name = "A".to_string();
    let mut b = chat("b1", Platform::Twitcasting, "alice", "tc:anonymous");
    b.author.name = "B".to_string();
    let mut all = e.observe(&a, T0 + 1);
    all.extend(e.observe(&b, T0 + 2));
    assert_eq!(ids(&all), vec!["s2"]);
    assert!(matches!(all[0].subject.kind, SubjectKind::Streamer));
}

#[test]
fn twitch_anonymous_gifter_excluded_from_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_count("v2", Metric::GiftCount, 2.0),
            streamer_count("s2", Window::Lifetime, Metric::GiftCount, 2.0),
        ],
        T0,
    );
    let gift = |id: &str| {
        let mut m = chat(id, Platform::Twitch, "chan", "274598607");
        m.kind = MessageKind::Gift;
        m
    };
    let mut all = e.observe(&gift("g1"), T0 + 1);
    all.extend(e.observe(&gift("g2"), T0 + 2));
    assert_eq!(ids(&all), vec!["s2"], "AnAnonymousGifter は視聴者に入らない");
}

#[test]
fn is_shared_anonymous_matches_d15_cases() {
    assert!(is_shared_anonymous(&chat("a", Platform::Twitcasting, "alice", "tc:anonymous")));
    assert!(is_shared_anonymous(&chat("b", Platform::Twitch, "chan", "274598607")));
    assert!(is_shared_anonymous(&with_badge(cm("c"), "anonymous")));
    assert!(!is_shared_anonymous(&cm("d")));
    assert!(!is_shared_anonymous(&with_badge(cm("e"), "subscriber")));
}

// ---------------- メトリクスと通貨 ----------------

#[test]
fn first_superchat_unlocks_viewer_bronze() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("sc1", Metric::SuperChatCount, 1.0)], T0);
    assert!(e.observe(&cm("plain"), T0 + 1).is_empty());
    let got = e.observe(&super_chat("sc", "UC1", 500.0, "¥"), T0 + 2);
    assert_eq!(ids(&got), vec!["sc1"]);
    assert_eq!(to_json(&got[0].tier), "bronze");
}

#[test]
fn donation_jpy_threshold_across_currencies() {
    let dir = tempfile::tempdir().unwrap();
    let mut d = mk_defs(vec![viewer_amount("jpy1000", 1000.0, CurrencyMode::JpyConverted)]);
    d.rates.insert("USD".to_string(), 150.0);
    let e = AchievementEngine::open(dir.path(), d, T0);
    assert!(e.observe(&super_chat("s1", "UC1", 500.0, "¥"), T0 + 1).is_empty());
    assert!(e.observe(&super_chat("s2", "UC1", 2.0, "$"), T0 + 2).is_empty(), "500 + 2×150 = 800 円");
    assert_eq!(ids(&e.observe(&super_chat("s3", "UC1", 200.0, "¥"), T0 + 3)), vec!["jpy1000"]);
}

#[test]
fn donation_only_currency_ignores_others() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_amount("usd5", 5.0, CurrencyMode::Only("USD".to_string()))], T0);
    assert!(e.observe(&super_chat("s1", "UC1", 10_000.0, "¥"), T0 + 1).is_empty());
    assert!(e.observe(&super_chat("s2", "UC1", 3.0, "$"), T0 + 2).is_empty());
    assert_eq!(ids(&e.observe(&super_chat("s3", "UC1", 2.0, "USD"), T0 + 3)), vec!["usd5"]);
}

#[test]
fn donation_excludes_subgift() {
    let dir = tempfile::tempdir().unwrap();
    let mut d = mk_defs(vec![
        viewer_amount("jpy1", 1.0, CurrencyMode::JpyConverted),
        viewer_amount("only-subgift", 1.0, CurrencyMode::Only("SUBGIFT".to_string())),
    ]);
    d.rates.insert("SUBGIFT".to_string(), 100.0);
    let e = AchievementEngine::open(dir.path(), d, T0);
    let m = with_amount(chat("sg", Platform::Twitch, "chan", "u1"), MessageKind::Gift, 5.0, "SUBGIFT");
    assert!(e.observe(&m, T0 + 1).is_empty(), "SUBGIFT は DonationAmount に入れない");
}

#[test]
fn subgift_value_counts_as_gift_count() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("g5", Metric::GiftCount, 5.0)], T0);
    let m = with_amount(chat("sg", Platform::Twitch, "chan", "u1"), MessageKind::Gift, 5.0, "SUBGIFT");
    assert_eq!(ids(&e.observe(&m, T0 + 1)), vec!["g5"], "value 5 を件数 5 として足す");
}

#[test]
fn platform_filter_excludes_other_platforms() {
    let dir = tempfile::tempdir().unwrap();
    let ach = with_platforms(streamer_count("yt2", Window::Lifetime, Metric::Comments, 2.0), vec![Platform::Youtube]);
    let e = open(dir.path(), vec![ach], T0);
    for i in 0..3 {
        assert!(e.observe(&chat(&format!("t{i}"), Platform::Twitch, "chan", "u1"), T0 + 1 + i).is_empty());
    }
    assert!(e.observe(&chat("y1", Platform::Youtube, "UCchan", "u1"), T0 + 10).is_empty());
    assert_eq!(ids(&e.observe(&chat("y2", Platform::Youtube, "UCchan", "u1"), T0 + 11)), vec!["yt2"]);
}

#[test]
fn unlock_carries_milli_value_threshold_and_currency() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_amount("jpy1000", 1000.0, CurrencyMode::JpyConverted),
            viewer_amount("usd1", 1.0, CurrencyMode::Only("USD".to_string())),
            viewer_count("c2", Metric::Comments, 2.0),
        ],
        T0,
    );
    let first = e.observe(&super_chat("s1", "UC1", 1500.0, "¥"), T0 + 7);
    assert_eq!(ids(&first), vec!["jpy1000"]);
    let u = &first[0];
    assert_eq!((u.value_milli, u.threshold_milli), (1_500_000, 1_000_000));
    assert_eq!(u.currency.as_deref(), Some("JPY"));
    assert_eq!(u.window_key, "*");
    assert_eq!(u.unlocked_at_ms, T0 + 7);
    assert!(!u.id.is_empty());
    assert_eq!((u.name.as_str(), u.description.as_str()), ("name-jpy1000", "desc-jpy1000"));

    let usd = e.observe(&super_chat("s2", "UC1", 3.0, "$"), T0 + 8);
    let c2 = usd.iter().find(|u| u.achievement_id == "c2").expect("2 件目のコメントで c2");
    assert_eq!((c2.value_milli, c2.threshold_milli, c2.currency.clone()), (2_000, 2_000, None));
    let u1 = usd.iter().find(|u| u.achievement_id == "usd1").expect("usd1");
    assert_eq!((u1.value_milli, u1.threshold_milli), (3_000, 1_000));
    assert_eq!(u1.currency.as_deref(), Some("USD"));
}

// ---------------- epoch とセッション(D4・D10・D19) ----------------

#[test]
fn per_stream_unlock_refires_in_next_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_per_stream("ps2", Metric::Comments, 2.0)], T0);
    assert!(e.observe(&cm("a1"), T0).is_empty());
    let first = e.observe(&cm("a2"), T0 + 10);
    assert_eq!(ids(&first), vec!["ps2"]);
    assert_eq!(first[0].window_key, format!("e:{T0}"));

    let t2 = T0 + 10 + EPOCH_IDLE_MS + 100;
    assert!(e.observe(&cm("b1"), t2).is_empty());
    let second = e.observe(&cm("b2"), t2 + 1);
    assert_eq!(ids(&second), vec!["ps2"], "次の epoch で再び解除");
    assert_eq!(second[0].window_key, format!("e:{t2}"));
}

#[test]
fn epoch_closes_after_idle_without_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    assert_eq!(e.current_epoch(), None);
    e.observe(&cm("a"), T0);
    assert_eq!(e.current_epoch(), Some(format!("e:{T0}")));
    let t1 = T0 + EPOCH_IDLE_MS - 1;
    e.observe(&cm("b"), t1);
    assert_eq!(e.current_epoch(), Some(format!("e:{T0}")), "30 分未満なら同じ epoch");
    let t2 = t1 + EPOCH_IDLE_MS;
    e.observe(&cm("c"), t2);
    assert_eq!(e.current_epoch(), Some(format!("e:{t2}")), "30 分以上空けば新しい epoch");
}

#[test]
fn stale_session_minus_one_keeps_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    e.on_live_event(&started(Platform::Twitch, "streamer", "S1"), T0);
    assert_eq!(e.current_epoch(), Some(format!("e:{T0}")));
    // セッションに属さないチャンネルのコメント。S1 の活動にはならない。
    e.observe(&chat("x1", Platform::Twitch, "other", "u1"), T0 + STALE_SESSION_MS - 1);
    assert_eq!(e.current_epoch(), Some(format!("e:{T0}")));
}

#[test]
fn stale_session_exact_opens_new_epoch_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    e.on_live_event(&started(Platform::Twitch, "streamer", "S1"), T0);
    let t = T0 + STALE_SESSION_MS;
    e.observe(&chat("x1", Platform::Twitch, "other", "u1"), t);
    assert_eq!(e.current_epoch(), Some(format!("e:{t}")), "さらに 30 分は待たず、その計上で新しい epoch");
}

#[test]
fn streams_attended_counts_distinct_epochs() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("att2", Metric::StreamsAttended, 2.0)], T0);
    assert!(e.observe(&cm("a1"), T0).is_empty());
    assert!(e.observe(&cm("a2"), T0 + 10).is_empty(), "同じ epoch は 1 回");
    let got = e.observe(&cm("b1"), T0 + 10 + EPOCH_IDLE_MS + 1);
    assert_eq!(ids(&got), vec!["att2"]);
}

#[test]
fn streams_metric_dedups_same_stream_id() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![streamer_count("st2", Window::Lifetime, Metric::Streams, 2.0)], T0);
    assert!(e.on_live_event(&started(Platform::Youtube, "chan", "S1"), T0).is_empty());
    assert!(e.on_live_event(&ended(Platform::Youtube, "chan"), T0 + 1).is_empty());
    assert!(e.on_live_event(&started(Platform::Youtube, "chan", "S1"), T0 + 2).is_empty(), "同じ stream_id は 1 本");
    let got = e.on_live_event(&started(Platform::Youtube, "chan", "S2"), T0 + 3);
    assert_eq!(ids(&got), vec!["st2"]);
    assert!(matches!(got[0].subject.kind, SubjectKind::Streamer));
}

#[test]
fn unique_commenters_per_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![streamer_count("uc2", Window::PerStream, Metric::UniqueCommenters, 2.0)], T0);
    for i in 0..3 {
        assert!(e.observe(&cm_by(&format!("a{i}"), "u1"), T0 + i).is_empty(), "同じ人は 1 人");
    }
    let first = e.observe(&cm_by("b1", "u2"), T0 + 10);
    assert_eq!(ids(&first), vec!["uc2"]);
    assert_eq!(first[0].window_key, format!("e:{T0}"));

    let t2 = T0 + 10 + EPOCH_IDLE_MS + 100;
    assert!(e.observe(&cm_by("c1", "u1"), t2).is_empty(), "次の epoch は数え直し");
    let second = e.observe(&cm_by("c2", "u2"), t2 + 1);
    assert_eq!(ids(&second), vec!["uc2"]);
    assert_eq!(second[0].window_key, format!("e:{t2}"));
}

fn status(platform: &str, ident: &str, viewers: u32, kind: ViewerCountKind) -> ChannelStatus {
    ChannelStatus {
        platform: platform.to_string(),
        identifier: ident.to_string(),
        viewers: Some(viewers),
        viewers_kind: kind,
        live: Some(true),
        ..Default::default()
    }
}

fn snapshot(list: Vec<ChannelStatus>) -> StatsSnapshot {
    StatsSnapshot { channel_status: list, ..Default::default() }
}

#[test]
fn peak_viewers_sums_filtered_concurrent_channels() {
    let dir = tempfile::tempdir().unwrap();
    let ach = with_platforms(
        streamer_count("peak100", Window::PerStream, Metric::PeakConcurrentViewers, 100.0),
        vec![Platform::Youtube, Platform::Twitch],
    );
    let e = open(dir.path(), vec![ach], T0);
    e.on_live_event(&started(Platform::Youtube, "chan", "S1"), T0); // epoch を開く
    let noise = |yt: u32, tw: u32| {
        snapshot(vec![
            status("youtube", "chan", yt, ViewerCountKind::Concurrent),
            status("twitch", "tw", tw, ViewerCountKind::Concurrent),
            status("youtube", "cum", 500, ViewerCountKind::Cumulative), // 累計は除く
            status("x", "xx", 1000, ViewerCountKind::Concurrent),       // 絞り込み外のプラットフォーム
        ])
    };
    assert!(e.on_stats(&noise(60, 30), T0 + 10).is_empty(), "60 + 30 = 90 < 100");
    let got = e.on_stats(&noise(60, 45), T0 + 20);
    assert_eq!(ids(&got), vec!["peak100"], "60 + 45 = 105");
    assert_eq!(got[0].value_milli, 105_000);
}

#[test]
fn twitcasting_message_maps_to_movie_session() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    e.on_live_event(&started(Platform::Twitcasting, "https://twitcasting.tv/Alice", "42"), T0);
    let m = chat("t1", Platform::Twitcasting, "alice", "tc:bob");
    assert_eq!(e.session_for(&m), Some("twitcasting:42".to_string()));
    let other = chat("t2", Platform::Twitcasting, "bob", "tc:bob");
    assert_eq!(e.session_for(&other), None);
}

#[test]
fn started_on_channel_named_like_other_stream_id_keeps_that_stream() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c100", Metric::Comments, 100.0)], T0);
    e.on_live_event(&started(Platform::Twitch, "alice", "42"), T0);
    // チャンネル名 "42" は alice の配信 ID と同じ文字列だが別チャンネル。
    e.on_live_event(&started(Platform::Twitch, "42", "99"), T0 + 10);
    let m = chat("a1", Platform::Twitch, "alice", "tw:bob");
    assert_eq!(e.session_for(&m), Some("twitch:42".to_string()));
}

// ---------------- 定義の変更(D9) ----------------

#[test]
fn set_definitions_new_achievement_unlocks_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("far", Metric::Comments, 100_000.0)], T0);
    assert!(feed(&e, "m", 100, T0 + 1).is_empty());
    let defs = mk_defs(vec![
        viewer_count("far", Metric::Comments, 100_000.0),
        viewer_count("c100", Metric::Comments, 100.0),
    ]);
    let got = e.set_definitions(defs, T0 + 2);
    assert_eq!(ids(&got), vec!["c100"], "イベントを待たずに返る");
}

#[test]
fn set_definitions_lowered_threshold_unlocks_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c", Metric::Comments, 50.0)], T0);
    assert!(feed(&e, "m", 10, T0 + 1).is_empty());
    let got = e.set_definitions(mk_defs(vec![viewer_count("c", Metric::Comments, 10.0)]), T0 + 2);
    assert_eq!(ids(&got), vec!["c"]);
}

#[test]
fn set_definitions_does_not_reunlock_existing() {
    let dir = tempfile::tempdir().unwrap();
    let list = || vec![viewer_count("c2", Metric::Comments, 2.0)];
    let e = open(dir.path(), list(), T0);
    assert_eq!(ids(&feed(&e, "m", 2, T0 + 1)), vec!["c2"]);
    let mut again = list();
    again.push(viewer_count("far", Metric::Comments, 100_000.0));
    assert!(e.set_definitions(mk_defs(again), T0 + 2).is_empty());
}

#[test]
fn set_definitions_can_complete_platinum() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![viewer_count("a", Metric::Comments, 100_000.0), platinum("plat", SubjectKind::Viewer)],
        T0,
    );
    assert!(feed(&e, "m", 3, T0 + 1).is_empty());
    let got = e.set_definitions(
        mk_defs(vec![viewer_count("a", Metric::Comments, 3.0), platinum("plat", SubjectKind::Viewer)]),
        T0 + 2,
    );
    assert_eq!(ids(&got), vec!["a", "plat"]);
}

#[test]
fn disabled_achievement_never_unlocks() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![disabled(viewer_count("off", Metric::Comments, 1.0))], T0);
    assert!(feed(&e, "m", 3, T0 + 1).is_empty());
    let got = e.set_definitions(mk_defs(vec![disabled(viewer_count("off", Metric::Comments, 1.0))]), T0 + 2);
    assert!(got.is_empty());
}

// ---------------- プラチナ(D6) ----------------

#[test]
fn platinum_unlocks_when_all_lifetime_unlocked() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_count("a", Metric::Comments, 1.0),
            viewer_count("b", Metric::SuperChatCount, 1.0),
            platinum("plat", SubjectKind::Viewer),
        ],
        T0,
    );
    assert_eq!(ids(&e.observe(&chat("m1", Platform::Youtube, "UCchan", "u1"), T0 + 1)), vec!["a"], "b が残っているので plat はまだ");
    let got = e.observe(&super_chat("m2", "u1", 100.0, "¥"), T0 + 2);
    let mut want = vec!["b".to_string(), "plat".to_string()];
    want.sort();
    assert_eq!(ids(&got), want);
    let plat = got.iter().find(|u| u.achievement_id == "plat").unwrap();
    assert_eq!(plat.window_key, "*");
}

#[test]
fn platinum_ignores_per_stream_and_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![
            viewer_count("a", Metric::Comments, 1.0),
            viewer_per_stream("ps", Metric::Comments, 100_000.0),
            disabled(viewer_count("off", Metric::Comments, 100_000.0)),
            platinum("plat", SubjectKind::Viewer),
        ],
        T0,
    );
    assert_eq!(ids(&e.observe(&cm("m1"), T0 + 1)), vec!["a", "plat"]);
}

#[test]
fn platinum_not_revoked_when_achievement_added() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(
        dir.path(),
        vec![viewer_count("a", Metric::Comments, 1.0), platinum("plat", SubjectKind::Viewer)],
        T0,
    );
    assert_eq!(ids(&e.observe(&cm("m1"), T0 + 1)), vec!["a", "plat"]);
    let got = e.set_definitions(
        mk_defs(vec![
            viewer_count("a", Metric::Comments, 1.0),
            viewer_count("new", Metric::Comments, 100_000.0),
            platinum("plat", SubjectKind::Viewer),
        ]),
        T0 + 2,
    );
    assert!(got.is_empty(), "取り消さず、再通知もしない");
    let recs = e.listener_unlocks("twitch", "u1");
    assert!(recs.iter().any(|r| r.achievement_id == "plat"));
}

// ---------------- 解除の主体 ----------------

#[test]
fn viewer_unlock_has_platform_author_id_and_name() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c1", Metric::Comments, 1.0)], T0);
    let mut m = chat("m1", Platform::Youtube, "UCchan", "UC9");
    m.author.name = "Taro".to_string();
    let got = e.observe(&m, T0 + 1);
    assert_eq!(got.len(), 1);
    let s = &got[0].subject;
    assert!(matches!(s.kind, SubjectKind::Viewer));
    assert_eq!(s.platform, Some(Platform::Youtube));
    assert_eq!(s.author_id.as_deref(), Some("UC9"));
    assert_eq!(s.name.as_deref(), Some("Taro"));
}

#[test]
fn streamer_unlock_has_no_author() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![streamer_count("s1", Window::Lifetime, Metric::Comments, 1.0)], T0);
    let got = e.observe(&cm("m1"), T0 + 1);
    assert_eq!(got.len(), 1);
    let s = &got[0].subject;
    assert!(matches!(s.kind, SubjectKind::Streamer));
    assert_eq!(s.author_id, None);
    assert_eq!(s.name, None);
    assert!(e.streamer_unlocks().iter().any(|r| r.achievement_id == "s1"));
}

// ---------------- リスナー削除(D16) ----------------

#[test]
fn forget_listener_removes_counters_and_unlocks() {
    let dir = tempfile::tempdir().unwrap();
    let e = open(dir.path(), vec![viewer_count("c2", Metric::Comments, 2.0)], T0);
    assert_eq!(ids(&feed(&e, "a", 2, T0 + 1)), vec!["c2"]);
    assert_eq!(e.listener_unlocks("twitch", "u1").len(), 1);
    e.forget_listener("twitch", "u1").unwrap();
    assert!(e.listener_unlocks("twitch", "u1").is_empty());
    assert!(e.observe(&cm("b0"), T0 + 5).is_empty(), "カウンタは 0 から数え直す");
    assert_eq!(ids(&e.observe(&cm("b1"), T0 + 6)), vec!["c2"], "解除キーも消えているので再び解除");
    e.forget_listener("twitch", "u1").unwrap();
    e.flush().unwrap();
    let text = String::from_utf8(read(&dir.path().join(PROGRESS_FILE))).unwrap();
    assert!(!text.contains("twitch:u1"), "同期で書かれ、ファイルに残らない");
}

#[test]
fn delete_while_disabled_does_not_return_after_reenable() {
    let dir = tempfile::tempdir().unwrap();
    let list = || vec![viewer_count("c2", Metric::Comments, 2.0)];
    let e = open(dir.path(), list(), T0);
    assert_eq!(ids(&feed(&e, "a", 2, T0 + 1)), vec!["c2"]);
    e.flush().unwrap();
    drop(e);

    assert!(delete_listener_progress(dir.path(), "twitch", "u1").expect("消せる"));
    let text = String::from_utf8(read(&dir.path().join(PROGRESS_FILE))).unwrap();
    assert!(!text.contains("twitch:u1"));

    let e2 = open(dir.path(), list(), T0 + 100);
    assert!(e2.listener_unlocks("twitch", "u1").is_empty());
    assert!(e2.observe(&cm("b0"), T0 + 101).is_empty(), "カウンタは 0。1 件目では解除されない");
    assert_eq!(ids(&e2.observe(&cm("b1"), T0 + 102)), vec!["c2"]);
}

// ---------------- その他 ----------------

#[test]
fn engine_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AchievementEngine>();
    let _ = DEDUP_CAPACITY;
    let _: Option<ChatMessage> = None;
}
