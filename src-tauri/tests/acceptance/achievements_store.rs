// registered in main.rs by story ACH-001
//! 定義ストア(acceptance.md §3.3、D8・D14)。I/O の失敗は `StoreIo` の差し替えで起こす。
use crate::achievements_common::*;
use fast_comment_lib::achievements::model::{presets, Metric, ValidationError};
use fast_comment_lib::achievements::store::{
    AchievementDefs, CommitError, DefsStore, DEFS_FILE, PROGRESS_FILE, STORE_VERSION,
};
use std::path::PathBuf;

fn defs_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(DEFS_FILE)
}

fn committed(r: Result<u64, CommitError>) -> u64 {
    match r {
        Ok(rev) => rev,
        Err(_) => panic!("commit が失敗した"),
    }
}

/// 現在の定義を 1 回 commit して revision 1 にした状態。
fn store_at_rev1(dir: &tempfile::TempDir) -> DefsStore {
    let s = DefsStore::load(dir.path(), T0);
    let mut d = s.current();
    d.enabled = true;
    assert_eq!(committed(s.commit(0, d)), 1);
    s
}

#[test]
fn defs_default_when_missing_has_presets_disabled_and_jpy_rate() {
    let dir = tempfile::tempdir().unwrap();
    let d = DefsStore::load(dir.path(), T0).current();
    assert_eq!(d.version, STORE_VERSION);
    assert_eq!(d.revision, 0);
    assert!(!d.enabled);
    assert_eq!(d.rates.len(), 1);
    assert_eq!(d.rates.get("JPY"), Some(&1.0));
    assert!(d.rates.get("BITS").is_none() && d.rates.get("PT").is_none());
    let want: Vec<String> = presets().into_iter().map(|a| a.id).collect();
    let got: Vec<String> = d.achievements.iter().map(|a| a.id.clone()).collect();
    assert_eq!(got, want);
}

#[test]
fn defs_roundtrip_preserves_rates_overrides_and_achievements() {
    let dir = tempfile::tempdir().unwrap();
    let s = DefsStore::load(dir.path(), T0);
    let mut d = s.current();
    d.enabled = true;
    d.rates.insert("USD".to_string(), 150.5);
    d.rates_updated_at_ms = Some(T0 + 5);
    d.symbol_overrides.insert("R$".to_string(), "BRL".to_string());
    d.achievements.push(viewer_count("custom-1", Metric::Comments, 42.0));
    let want_ach = to_json(&d.achievements);
    committed(s.commit(0, d));

    let r = DefsStore::load(dir.path(), T0 + 100).current();
    assert!(r.enabled);
    assert_eq!(r.revision, 1);
    assert_eq!(r.rates.get("USD"), Some(&150.5));
    assert_eq!(r.rates.get("JPY"), Some(&1.0));
    assert_eq!(r.rates_updated_at_ms, Some(T0 + 5));
    assert_eq!(r.symbol_overrides.get("R$").map(String::as_str), Some("BRL"));
    assert_eq!(to_json(&r.achievements), want_ach);
}

#[test]
fn defs_commit_returns_incremented_revision() {
    let dir = tempfile::tempdir().unwrap();
    let s = DefsStore::load(dir.path(), T0);
    assert_eq!(committed(s.commit(0, s.current())), 1);
    assert_eq!(s.current().revision, 1);
    assert_eq!(committed(s.commit(1, s.current())), 2);
    assert_eq!(s.current().revision, 2);
}

#[test]
fn defs_commit_stale_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let s = store_at_rev1(&dir);
    let bytes_before = read(&defs_path(&dir));
    let before = s.current();
    let (rev, enabled, ach) = (before.revision, before.enabled, to_json(&before.achievements));

    let mut next = s.current();
    next.enabled = false;
    next.rates.insert("USD".to_string(), 999.0);
    next.achievements.push(viewer_count("late", Metric::Comments, 1.0));
    let r = s.commit(0, next);
    assert!(matches!(r, Err(CommitError::Stale)), "古い base_revision は Stale");

    assert_eq!(read(&defs_path(&dir)), bytes_before, "ディスクのバイトが同じ");
    let after = s.current();
    assert_eq!(after.revision, rev, "revision が同じ");
    assert_eq!(after.enabled, enabled);
    assert!(after.rates.get("USD").is_none());
    assert_eq!(to_json(&after.achievements), ach, "current() が同じ");
}

#[test]
fn defs_commit_invalid_rejected_and_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let s = DefsStore::load(dir.path(), T0);
    let mut next = s.current();
    let mut bad = viewer_count("bad-one", Metric::Comments, 1.0);
    bad.name = String::new();
    next.achievements.push(bad);
    match s.commit(0, next) {
        Err(CommitError::Invalid(id, ValidationError::EmptyName)) => assert_eq!(id, "bad-one"),
        _ => panic!("EmptyName の Invalid になるはず"),
    }
    assert_eq!(s.current().revision, 0);
    assert!(s.current().achievements.iter().all(|a| a.id != "bad-one"));
}

/// 壊れた原本を置いて load し、`.broken-<now>` へ退避されて Default で開くことを確かめる。
fn assert_broken_backed_up(original: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(defs_path(&dir), original).unwrap();
    let now = 1_234_567u64;
    let s = DefsStore::load(dir.path(), now);
    let backups = files_with_prefix(dir.path(), &format!("{DEFS_FILE}.broken-"));
    assert_eq!(backups.len(), 1, "退避はちょうど 1 つ");
    assert_eq!(backups[0].file_name().unwrap().to_string_lossy(), format!("{DEFS_FILE}.broken-{now}"));
    assert_eq!(read(&backups[0]), original, "退避は原本と同じバイト");
    let d = s.current();
    assert_eq!(d.revision, 0);
    assert!(!d.enabled);
    assert_eq!(d.achievements.len(), presets().len());
}

#[test]
fn defs_missing_version_backed_up_as_broken_and_default() {
    assert_broken_backed_up(br#"{"revision":5,"enabled":true,"achievements":[]}"#);
}

#[test]
fn defs_unknown_version_backed_up_as_broken_and_default() {
    let other = STORE_VERSION + 98;
    assert_broken_backed_up(format!(r#"{{"version":{other},"revision":5,"enabled":true}}"#).as_bytes());
}

#[test]
fn defs_corrupt_json_backed_up_as_broken_and_default() {
    assert_broken_backed_up(b"{not json at all");
}

#[test]
fn defs_broken_backup_adds_counter_when_name_taken() {
    let dir = tempfile::tempdir().unwrap();
    let taken = dir.path().join(format!("{DEFS_FILE}.broken-5000"));
    std::fs::write(&taken, b"older backup").unwrap();
    std::fs::write(defs_path(&dir), b"{corrupt").unwrap();
    let _ = DefsStore::load(dir.path(), 5000);
    assert_eq!(read(&taken), b"older backup", "既存のバックアップは変わらない");
    let next = dir.path().join(format!("{DEFS_FILE}.broken-5000-1"));
    assert_eq!(read(&next), b"{corrupt");
}

#[test]
fn defs_backup_failure_refuses_commit_and_keeps_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let original = b"{corrupt original".to_vec();
    let old_backup = dir.path().join(format!("{DEFS_FILE}.broken-1000"));
    std::fs::write(defs_path(&dir), &original).unwrap();
    std::fs::write(&old_backup, b"keep me").unwrap();
    let io = TestIo::new();
    io.fail_backups(true);

    let s = DefsStore::load_with(dir.path(), 2000, io.clone());
    let mut next: AchievementDefs = s.current();
    next.enabled = true;
    let r = s.commit(0, next);
    assert!(matches!(r, Err(CommitError::SaveRefused)), "退避に失敗した store は保存拒否");
    assert_eq!(read(&defs_path(&dir)), original, "原本のバイトが同じ");
    assert_eq!(read(&old_backup), b"keep me", "既存のバックアップのバイトが同じ");
    assert_eq!(io.write_attempts(), 0, "何も書こうとしない");
}

#[test]
fn defs_write_leaves_no_tmp_file() {
    let dir = tempfile::tempdir().unwrap();
    let s = DefsStore::load(dir.path(), T0);
    committed(s.commit(0, s.current()));
    committed(s.commit(1, s.current()));
    assert_eq!(file_names(dir.path()), vec![DEFS_FILE.to_string()]);
    let _ = PROGRESS_FILE;
}
