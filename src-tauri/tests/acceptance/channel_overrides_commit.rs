// registered in main.rs by story OV-001
//! 枠ごとの上書き 受け入れテスト 3.2: `commit_channel_overrides` の排他(基準 revision)と保存。
#![allow(dead_code)]

use crate::channel_overrides_common::*;
use fast_comment_lib::config::{
    commit_channel_overrides, commit_config, AppConfig, ChannelOverridesCommitError, ConfigCommitError,
};
use serde_json::json;
use std::sync::atomic::Ordering;

fn rows() -> Vec<fast_comment_lib::config::ChannelConfig> {
    vec![chan("twitch", "alice"), chan_with("youtube", "vid", json!({"ttsEnabled": false})), chan("x", "zzz")]
}

#[test]
fn stale_overrides_save_changes_nothing() {
    let (dir, rev, tx, _rx, mut cur) = baseline_with(rows());
    let path = AppConfig::path_in(dir.path());
    let bytes_before = std::fs::read(&path).unwrap();
    let cur_before = cur.clone();
    let watch_before = tx.borrow().clone();
    let rev_before = rev.load(Ordering::SeqCst);

    let r = commit_channel_overrides(dir.path(), &rev, &tx, &mut cur, "twitch:alice", off(), rev_before - 1);

    assert!(matches!(r, Err(ChannelOverridesCommitError::Stale)), "古い revision は Stale のはず: {r:?}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before, "ディスクが変わった");
    assert_eq!(cur, cur_before, "メモリの設定が変わった");
    assert_eq!(*tx.borrow(), watch_before, "watch が変わった");
    assert_eq!(rev.load(Ordering::SeqCst), rev_before, "revision が進んだ");
}

#[test]
fn fresh_overrides_save_persists_and_bumps_revision_by_one() {
    let (dir, rev, tx, _rx, mut cur) = baseline_with(rows());
    let old = rev.load(Ordering::SeqCst);

    let new = commit_channel_overrides(dir.path(), &rev, &tx, &mut cur, "twitch:alice", off(), old)
        .expect("正しい revision なら保存できる");

    assert_eq!(new, old + 1);
    assert_eq!(rev.load(Ordering::SeqCst), new);
    assert_eq!(cur.channels[0].overrides, off(), "メモリに反映される");
    let disk = disk_json(dir.path());
    assert_eq!(disk["channels"][0]["overrides"], json!({"ttsEnabled": false}), "ディスクに反映される");
    assert_eq!(disk["channels"][1]["overrides"], json!({"ttsEnabled": false}), "他の行は変わらない");
    assert!(disk["channels"][2].get("overrides").is_none());
    assert_eq!(AppConfig::load(dir.path()).unwrap(), cur, "再読み込みしても同じ");
}

#[test]
fn overrides_save_unknown_key_changes_nothing() {
    let (dir, rev, tx, _rx, mut cur) = baseline_with(rows());
    let path = AppConfig::path_in(dir.path());
    let bytes_before = std::fs::read(&path).unwrap();
    let cur_before = cur.clone();
    let watch_before = tx.borrow().clone();
    let rev_before = rev.load(Ordering::SeqCst);

    let r = commit_channel_overrides(dir.path(), &rev, &tx, &mut cur, "twitch:nobody", off(), rev_before);

    assert!(matches!(r, Err(ChannelOverridesCommitError::NotFound)), "未知のキーは NotFound のはず: {r:?}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before, "ディスクが変わった");
    assert_eq!(cur, cur_before, "メモリの設定が変わった");
    assert_eq!(*tx.borrow(), watch_before, "watch が変わった");
    assert_eq!(rev.load(Ordering::SeqCst), rev_before, "revision が進んだ");
}

#[test]
fn overrides_save_then_old_base_full_save_is_stale() {
    let (dir, rev, tx, _rx, mut cur) = baseline_with(rows());
    let old = rev.load(Ordering::SeqCst);
    // 全体保存側が古い基準を握っている。
    let mut full_next = cur.clone();
    full_next.channels.push(chan("niconico", "lv1"));

    commit_channel_overrides(dir.path(), &rev, &tx, &mut cur, "twitch:alice", off(), old).unwrap();
    let bytes_after_overrides = std::fs::read(AppConfig::path_in(dir.path())).unwrap();

    let r = commit_config(dir.path(), &rev, &tx, &mut cur, full_next, Some(old));

    assert!(matches!(r, Err(ConfigCommitError::Stale)), "古い基準の全体保存は Stale のはず: {r:?}");
    assert_eq!(std::fs::read(AppConfig::path_in(dir.path())).unwrap(), bytes_after_overrides);
    assert_eq!(cur.channels.len(), 3, "全体保存の内容は入っていない");
}

#[test]
fn watch_receives_config_with_new_overrides() {
    let (dir, rev, tx, mut rx, mut cur) = baseline_with(rows());
    rx.mark_unchanged();
    let old = rev.load(Ordering::SeqCst);

    commit_channel_overrides(dir.path(), &rev, &tx, &mut cur, "x:zzz", voice_ov(Some(5), None, None), old).unwrap();

    assert!(rx.has_changed().unwrap(), "watch に新しい設定が流れていない");
    let seen = rx.borrow_and_update().clone();
    assert_eq!(seen.channels[2].overrides, voice_ov(Some(5), None, None));
    assert_eq!(seen, cur, "watch の中身はメモリの設定と同じ");
}
