// registered in main.rs by story TC3-302 (unknown_official_api_base_key_is_ignored also needs TC3-301's official_endpoints)
//! PR3' 受け入れテスト 4.8: `twitcasting.mode` の設定形式・保存。
#![allow(dead_code)]

use fast_comment_lib::config::{commit_config, AppConfig, ConfigCommitError, TwitcastingMode};
use fast_comment_lib::sources::twitcasting_api::official_endpoints;
use fast_comment_lib::sources::twitcasting_auth::TwitcastingEndpoints;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

fn parse(v: Value) -> AppConfig {
    serde_json::from_value(v).expect("設定として読める")
}

/// 一度保存して revision=1 にした状態(ディスク・メモリ・watch が一致)。
fn baseline() -> (tempfile::TempDir, AtomicU64, tokio::sync::watch::Sender<AppConfig>, AppConfig) {
    let dir = tempfile::tempdir().unwrap();
    let rev = AtomicU64::new(0);
    let mut cur = AppConfig::default();
    let (tx, _rx) = tokio::sync::watch::channel(cur.clone());
    let r = commit_config(dir.path(), &rev, &tx, &mut cur, AppConfig::default(), None).expect("初回保存");
    assert_eq!(r, 1);
    (dir, rev, tx, cur)
}

#[test]
fn mode_defaults_to_auto_when_field_missing() {
    let a = parse(json!({"twitcasting": {"callbackPort": 39822}}));
    assert_eq!(a.twitcasting.mode, TwitcastingMode::Auto);
    let b = parse(json!({}));
    assert_eq!(b.twitcasting.mode, TwitcastingMode::Auto);
}

#[test]
fn mode_round_trips_lowercase() {
    for (mode, text) in [
        (TwitcastingMode::Auto, "auto"),
        (TwitcastingMode::Unofficial, "unofficial"),
        (TwitcastingMode::Official, "official"),
    ] {
        assert_eq!(serde_json::to_value(mode).unwrap(), json!(text));
        let mut cfg = AppConfig::default();
        cfg.twitcasting.mode = mode;
        let v = serde_json::to_value(&cfg).unwrap();
        assert_eq!(v["twitcasting"]["mode"], json!(text));
        let back: AppConfig = serde_json::from_value(v).unwrap();
        assert_eq!(back.twitcasting.mode, mode);
    }
}

#[test]
fn unknown_or_non_string_mode_falls_back_to_auto() {
    for bad in [json!("OFFICIAL"), json!("x"), json!(1), json!(null)] {
        let cfg = parse(json!({"twitcasting": {"callbackPort": 40000, "mode": bad.clone()}}));
        assert_eq!(cfg.twitcasting.mode, TwitcastingMode::Auto, "mode={bad}");
        assert_eq!(cfg.twitcasting.callback_port, 40000, "他のフィールドは保たれる(mode={bad})");
    }
}

#[test]
fn stale_mode_save_changes_nothing() {
    let (dir, rev, tx, mut cur) = baseline();
    let path = AppConfig::path_in(dir.path());
    let bytes_before = std::fs::read(&path).unwrap();
    let cur_before = cur.clone();
    let watch_before = tx.borrow().clone();
    let rev_before = rev.load(Ordering::SeqCst);

    let mut next = cur.clone();
    next.twitcasting.mode = TwitcastingMode::Official;
    let result = commit_config(dir.path(), &rev, &tx, &mut cur, next, Some(rev_before - 1));

    assert!(matches!(result, Err(ConfigCommitError::Stale)), "古い revision は Stale のはず");
    assert_eq!(std::fs::read(&path).unwrap(), bytes_before, "ディスクが変わった");
    assert_eq!(cur, cur_before, "メモリの設定が変わった");
    assert_eq!(*tx.borrow(), watch_before, "watch が変わった");
    assert_eq!(rev.load(Ordering::SeqCst), rev_before, "revision が進んだ");
}

#[test]
fn fresh_mode_save_persists_and_bumps_revision() {
    let (dir, rev, tx, mut cur) = baseline();
    let old = rev.load(Ordering::SeqCst);
    let mut next = cur.clone();
    next.twitcasting.mode = TwitcastingMode::Official;

    let new = commit_config(dir.path(), &rev, &tx, &mut cur, next, Some(old)).expect("正しい revision なら保存できる");

    assert_eq!(new, old + 1);
    let disk: Value = serde_json::from_slice(&std::fs::read(AppConfig::path_in(dir.path())).unwrap()).unwrap();
    assert_eq!(disk["twitcasting"]["mode"], json!("official"));
    assert_eq!(tx.borrow().twitcasting.mode, TwitcastingMode::Official);
    assert_eq!(cur.twitcasting.mode, TwitcastingMode::Official);
}

#[test]
fn unknown_official_api_base_key_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let text = json!({
        "twitcastingOverrides": {"endpoints": {"officialApiBase": "https://evil.example"}}
    })
    .to_string();
    std::fs::write(AppConfig::path_in(dir.path()), text).unwrap();
    let cfg = AppConfig::load(dir.path()).expect("未知のキーがあっても設定は読める");
    assert_eq!(official_endpoints(&cfg).api_base, TwitcastingEndpoints::default().api_base);
}
