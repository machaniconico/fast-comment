//! 自動バックアップの受け入れテスト(.omc/plans/auto-backup.md「公開の境界」「受け入れ基準」)。
//! 基準作成側(test-engineer)が実装より先に書いた。実装者は編集しない。
//! テスト用の config_dir に実物と同じファイル名でダミーを並べてから呼ぶ。
use fast_comment_lib::backup::{
    apply_pending_restore, create_backup, due_at_startup, list_backups, prune, stage_restore,
    BackupError, BackupSettings,
};
use fast_comment_lib::config::CredentialsConfig;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const T0: u64 = 1_700_000_000_000;
const HOUR_MS: u64 = 3_600_000;

const X_AUTH: &str = "SECRET-X-AUTH-TOKEN-7f3a91";
const X_CSRF: &str = "SECRET-X-CSRF-TOKEN-b82c40";
const TWITCH: &str = "oauth:SECRET-TWITCH-OAUTH-5d19e7";
const YT_KEY: &str = "SECRET-YOUTUBE-API-KEY-c60d28";

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap()
}

// 実際のストアの読み込みと同じ型で読める最小の固定データ。名前・値は中身の差を見分けるためのもの。
const LISTENERS_OLD: &str = r#"{"twitch:old":{"name":"listeners-old"}}"#;
const LISTENERS_CURRENT: &str = r#"{"twitch:old":{"name":"listeners-current"}}"#;
const LISTENERS_CHANGED: &str = r#"{"twitch:old":{"name":"listeners-changed"}}"#;
const ACHIEVEMENTS_OLD: &str = r#"{"version":1,"revision":1,"enabled":false,"rates":{"JPY":1.0},"ratesUpdatedAtMs":null,"symbolOverrides":{"JPY":"achievements-old"},"achievements":[]}"#;
const ACHIEVEMENTS_CURRENT: &str = r#"{"version":1,"revision":2,"enabled":false,"rates":{"JPY":1.0},"ratesUpdatedAtMs":null,"symbolOverrides":{"JPY":"achievements-current"},"achievements":[]}"#;
const PROGRESS_OLD: &str = r#"{"version":1,"enabledAtMs":0,"sessions":{},"seenStreams":[],"streamer":{},"viewers":{},"unlocks":{"progress-old":1}}"#;
const SOUNDS_OLD: &str = r#"{"version":1,"sounds":[{"id":"sounds-old","name":"ding","ext":"wav","size":9}]}"#;

fn config_json(keep: u32, creds: [&str; 4]) -> String {
    json!({
        "backup": { "enabled": true, "keep": keep },
        "credentials": {
            "xAuthToken": creds[0],
            "xCsrfToken": creds[1],
            "twitchOauth": creds[2],
            "youtubeApiKey": creds[3],
        },
    })
    .to_string()
}

/// 実物と同じファイル名でダミーを並べた config_dir(除外対象も含む)。
fn populated(dir: &Path) {
    write(dir, "config.json", &config_json(10, [X_AUTH, X_CSRF, TWITCH, YT_KEY]));
    write(dir, "listeners.json", LISTENERS_OLD);
    write(dir, "achievements.json", ACHIEVEMENTS_OLD);
    write(dir, "achievement-progress.json", PROGRESS_OLD);
    write(dir, "sounds.json", SOUNDS_OLD);
    write(dir, "sounds/ding.wav", "RIFF-ding");
    write(dir, "templates/custom/mine/index.html", "<html>mine</html>");
    write(dir, "skin/theme.css", "body{}");
    // 除外されるもの
    write(dir, "integrations.json", r#"{"webhook":"https://example.invalid/hook?sig=SECRET-HOOK"}"#);
    write(dir, "x-autopost-ledger.json", r#"{"posts":[]}"#);
    write(dir, "logs/app.log", "log line");
    write(dir, "exports/export.json", "{}");
    write(dir, "backups/note.txt", "not a backup");
    write(dir, "config.json.tmp", "tmp");
    write(dir, "listeners.json.broken-1", "broken");
    write(dir, "achievements.json.corrupt-2", "corrupt");
    write(dir, "listeners.json.bak.3", "bak");
}

/// zip の全エントリ(ディレクトリ項目を除く)を 名前 -> 中身 で返す。
fn zip_entries(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut ar = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let mut out = BTreeMap::new();
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).unwrap();
        if f.is_dir() {
            continue;
        }
        let name = String::from_utf8_lossy(f.name_raw()).into_owned();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        out.insert(name, buf);
    }
    out
}

fn valid_manifest(entries: &[&str]) -> String {
    json!({
        "schema": "fast-comment-backup",
        "version": 1,
        "createdAtMs": T0,
        "appVersion": "0.0.0",
        "entries": entries,
    })
    .to_string()
}

/// 任意の名前で zip を作る(悪意ある zip の再現用)。
fn write_zip(dir: &Path, file_name: &str, entries: &[(&str, &str)]) {
    let auto = dir.join("backups").join("auto");
    fs::create_dir_all(&auto).unwrap();
    let mut zw = zip::ZipWriter::new(fs::File::create(auto.join(file_name)).unwrap());
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);
    for (name, body) in entries {
        zw.start_file(*name, opts).unwrap();
        zw.write_all(body.as_bytes()).unwrap();
    }
    zw.finish().unwrap();
}

fn auto_dir(dir: &Path) -> std::path::PathBuf {
    dir.join("backups").join("auto")
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn backup_contains_expected_entries_and_manifest() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());

    let path = create_backup(dir.path(), T0).unwrap().expect("初回は作られる");

    assert!(path.exists());
    assert!(path.ends_with(format!("backups/auto/fc-backup-{T0}.zip")));
    let entries = zip_entries(&path);
    let names: Vec<&str> = entries.keys().map(String::as_str).collect();
    for want in [
        "manifest.json",
        "config.json",
        "listeners.json",
        "achievements.json",
        "achievement-progress.json",
        "sounds.json",
        "sounds/ding.wav",
        "templates/custom/mine/index.html",
        "skin/theme.css",
    ] {
        assert!(names.contains(&want), "{want} が入っていない: {names:?}");
    }
    for name in &names {
        assert!(
            *name != "integrations.json" && *name != "x-autopost-ledger.json",
            "{name} は入れない"
        );
        for prefix in ["logs/", "exports/", "backups/"] {
            assert!(!name.starts_with(prefix), "{name} は入れない");
        }
        assert!(!name.ends_with(".tmp"), "{name} は入れない");
        for infix in [".broken-", ".corrupt-", ".bak."] {
            assert!(!name.contains(infix), "{name} は入れない");
        }
    }

    let m: Value = serde_json::from_slice(&entries["manifest.json"]).unwrap();
    assert_eq!(m["schema"], "fast-comment-backup");
    assert_eq!(m["version"], 1);
    assert_eq!(m["createdAtMs"], T0);
    assert_eq!(m["appVersion"], env!("CARGO_PKG_VERSION"));
    let listed: Vec<&str> =
        m["entries"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    for want in ["config.json", "listeners.json", "sounds/ding.wav", "skin/theme.css"] {
        assert!(listed.contains(&want), "manifest.entries に {want} が無い: {listed:?}");
    }
    assert!(!listed.contains(&"integrations.json") && !listed.contains(&"x-autopost-ledger.json"));
}

#[test]
fn backup_strips_credentials() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());

    let path = create_backup(dir.path(), T0).unwrap().unwrap();

    let entries = zip_entries(&path);
    for (name, body) in &entries {
        let text = String::from_utf8_lossy(body);
        for secret in [X_AUTH, X_CSRF, TWITCH, YT_KEY, "SECRET-HOOK"] {
            assert!(!text.contains(secret), "{name} に秘密 {secret} が残っている");
        }
    }
    let cfg: Value = serde_json::from_slice(&entries["config.json"]).unwrap();
    let creds: CredentialsConfig =
        serde_json::from_value(cfg.get("credentials").cloned().unwrap_or(json!({}))).unwrap();
    assert_eq!(creds, CredentialsConfig::default());
    // 資格情報以外の設定は残る
    assert_eq!(cfg["backup"]["keep"], 10);
}

#[test]
fn unchanged_content_is_not_backed_up_twice() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());

    assert!(create_backup(dir.path(), T0).unwrap().is_some());
    assert!(create_backup(dir.path(), T0 + 1000).unwrap().is_none());
    assert_eq!(list_backups(dir.path()).len(), 1);

    write(dir.path(), "listeners.json", LISTENERS_CHANGED);
    let again = create_backup(dir.path(), T0 + 2000).unwrap();
    assert!(again.is_some(), "中身が変われば作る");
    assert_eq!(list_backups(dir.path()).len(), 2);
}

#[test]
fn prune_keeps_newest_and_ignores_foreign_files() {
    let dir = tempfile::tempdir().unwrap();
    let auto = auto_dir(dir.path());
    fs::create_dir_all(&auto).unwrap();
    // 文字列順と数値順が食い違う番号(数値で新旧を決めること)
    for ms in [9u64, 100, 20_000, 300_000] {
        fs::write(auto.join(format!("fc-backup-{ms}.zip")), b"x").unwrap();
    }
    fs::write(auto.join("notes.txt"), b"keep me").unwrap();
    fs::write(auto.join("fc-backup-abc.zip"), b"keep me too").unwrap();

    let removed = prune(dir.path(), 2).unwrap();

    assert_eq!(removed, 2);
    assert_eq!(
        names_in(&auto),
        vec!["fc-backup-20000.zip", "fc-backup-300000.zip", "fc-backup-abc.zip", "notes.txt"]
    );
    let listed: Vec<String> =
        list_backups(dir.path()).into_iter().map(|e| e.file_name).collect();
    assert_eq!(listed, vec!["fc-backup-300000.zip", "fc-backup-20000.zip"]);
}

#[test]
fn settings_keep_is_clamped() {
    let n = |keep| BackupSettings { enabled: true, keep }.normalized().keep;
    assert_eq!(n(0), 1);
    assert_eq!(n(1), 1);
    assert_eq!(n(10), 10);
    assert_eq!(n(50), 50);
    assert_eq!(n(99), 50);
    let d = BackupSettings::default();
    assert!(d.enabled);
    assert_eq!(d.keep, 10);
}

#[test]
fn due_at_startup_after_20_hours_or_when_empty() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    assert!(due_at_startup(dir.path(), T0), "1 件も無ければ作る");

    create_backup(dir.path(), T0).unwrap().unwrap();

    assert!(!due_at_startup(dir.path(), T0 + 19 * HOUR_MS));
    assert!(due_at_startup(dir.path(), T0 + 20 * HOUR_MS), "20 時間ちょうどは以上");
    assert!(due_at_startup(dir.path(), T0 + 21 * HOUR_MS));
}

#[test]
fn stage_restore_rejects_bad_names_and_traversal() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    let good_entry = ("manifest.json", valid_manifest(&["listeners.json"]));

    for bad in ["../x.zip", "C:\\a.zip", "fc-backup-1.zip/../x"] {
        let r = stage_restore(dir.path(), bad, T0);
        assert!(matches!(r, Err(BackupError::Invalid(_))), "{bad:?} は Invalid: {r:?}");
    }
    let r = stage_restore(dir.path(), "fc-backup-999.zip", T0);
    assert!(matches!(r, Err(BackupError::NotFound)), "存在しない名前は NotFound: {r:?}");

    for (n, evil) in ["../evil", "/abs", "integrations.json"].into_iter().enumerate() {
        let file = format!("fc-backup-{}.zip", 10 + n);
        write_zip(
            dir.path(),
            &file,
            &[(good_entry.0, &good_entry.1), (evil, "pwned"), ("listeners.json", "{}")],
        );
        let r = stage_restore(dir.path(), &file, T0 + 1);
        assert!(matches!(r, Err(BackupError::Invalid(_))), "{evil:?} を含む zip は Invalid: {r:?}");
    }

    assert!(!dir.path().join("backups").join("restore-pending").exists(), "検証で落ちたら展開しない");
    assert!(!dir.path().join("backups").join("evil").exists());
    assert!(!dir.path().join("evil").exists());
    assert_eq!(
        read(dir.path(), "integrations.json"),
        r#"{"webhook":"https://example.invalid/hook?sig=SECRET-HOOK"}"#
    );
}

#[test]
fn stage_restore_takes_safety_backup_first() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    create_backup(dir.path(), T0).unwrap().unwrap();
    write(dir.path(), "listeners.json", LISTENERS_CURRENT);
    let before = list_backups(dir.path()).len();

    stage_restore(dir.path(), &format!("fc-backup-{T0}.zip"), T0 + 5000).unwrap();

    let after = list_backups(dir.path());
    assert_eq!(after.len(), before + 1);
    assert_eq!(after[0].file_name, format!("fc-backup-{}.zip", T0 + 5000));
    assert_eq!(after[0].created_at_ms, T0 + 5000);
    let safety = zip_entries(&auto_dir(dir.path()).join(&after[0].file_name));
    assert_eq!(
        String::from_utf8_lossy(&safety["listeners.json"]),
        LISTENERS_CURRENT,
        "事前バックアップは復元前の今の状態"
    );
    assert!(dir.path().join("backups/restore-pending/READY").exists());
    // 適用は再起動後。この時点では今のファイルを触らない
    assert_eq!(read(dir.path(), "listeners.json"), LISTENERS_CURRENT);
}

#[test]
fn apply_restore_replaces_files_and_keeps_current_credentials() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    create_backup(dir.path(), T0).unwrap().unwrap();
    // バックアップ後に今の状態が変わった
    let now_creds = ["NOW-X-AUTH", "NOW-X-CSRF", "oauth:NOW-TWITCH", "NOW-YT-KEY"];
    write(dir.path(), "config.json", &config_json(7, now_creds));
    write(dir.path(), "listeners.json", LISTENERS_CURRENT);
    write(dir.path(), "achievements.json", ACHIEVEMENTS_CURRENT);
    write(dir.path(), "integrations.json", r#"{"webhook":"current-hook"}"#);
    write(dir.path(), "x-autopost-ledger.json", r#"{"posts":["current"]}"#);
    stage_restore(dir.path(), &format!("fc-backup-{T0}.zip"), T0 + 1000).unwrap();

    let applied = apply_pending_restore(dir.path(), T0 + 2000).unwrap();

    assert!(applied);
    assert_eq!(read(dir.path(), "listeners.json"), LISTENERS_OLD);
    assert_eq!(read(dir.path(), "achievements.json"), ACHIEVEMENTS_OLD);
    let cfg: Value = serde_json::from_str(&read(dir.path(), "config.json")).unwrap();
    assert_eq!(cfg["backup"]["keep"], 10, "資格情報以外はバックアップの内容");
    assert_eq!(cfg["credentials"]["xAuthToken"], "NOW-X-AUTH");
    assert_eq!(cfg["credentials"]["xCsrfToken"], "NOW-X-CSRF");
    assert_eq!(cfg["credentials"]["twitchOauth"], "oauth:NOW-TWITCH");
    assert_eq!(cfg["credentials"]["youtubeApiKey"], "NOW-YT-KEY");
    assert!(!dir.path().join("backups/restore-pending").exists());
    assert!(
        names_in(dir.path()).iter().any(|n| n.contains(".pre-restore-")),
        "退避が残る: {:?}",
        names_in(dir.path())
    );
    // バックアップに入らないものは触らない
    assert_eq!(read(dir.path(), "integrations.json"), r#"{"webhook":"current-hook"}"#);
    assert_eq!(read(dir.path(), "x-autopost-ledger.json"), r#"{"posts":["current"]}"#);
}

#[test]
fn apply_without_pending_is_false() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    assert!(!apply_pending_restore(dir.path(), T0).unwrap());

    // READY の無い restore-pending(展開の途中)は適用しない
    write(dir.path(), "backups/restore-pending/listeners.json", r#"{"twitch:x":{"name":"half"}}"#);
    assert!(!apply_pending_restore(dir.path(), T0 + 1).unwrap());
    assert_eq!(read(dir.path(), "listeners.json"), LISTENERS_OLD);
}

#[test]
fn failed_apply_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    let config_before = read(dir.path(), "config.json");
    // READY はあるが中身が壊れている(manifest と config.json が JSON でない)
    write(dir.path(), "backups/restore-pending/manifest.json", "{not json");
    write(dir.path(), "backups/restore-pending/config.json", "{broken");
    write(dir.path(), "backups/restore-pending/listeners.json", r#"{"twitch:x":{"name":"from-broken"}}"#);
    write(dir.path(), "backups/restore-pending/READY", "");

    let r = apply_pending_restore(dir.path(), T0);

    assert!(!matches!(r, Ok(true)), "壊れた復元は成功扱いにしない: {r:?}");
    assert_eq!(read(dir.path(), "config.json"), config_before);
    assert_eq!(read(dir.path(), "listeners.json"), LISTENERS_OLD);
    assert_eq!(read(dir.path(), "achievements.json"), ACHIEVEMENTS_OLD);
    assert_eq!(read(dir.path(), "sounds/ding.wav"), "RIFF-ding");
    assert!(!dir.path().join("backups/restore-pending").exists());
    assert!(
        names_in(&dir.path().join("backups")).iter().any(|n| n.starts_with("restore-failed-")),
        "restore-failed-* が残る: {:?}",
        names_in(&dir.path().join("backups"))
    );
}

#[test]
fn errors_do_not_contain_absolute_paths() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    // zip でないファイル
    fs::create_dir_all(auto_dir(dir.path())).unwrap();
    fs::write(auto_dir(dir.path()).join("fc-backup-7.zip"), b"this is not a zip").unwrap();
    write_zip(
        dir.path(),
        "fc-backup-8.zip",
        &[("manifest.json", &valid_manifest(&[])), ("../evil", "x")],
    );

    let errors: Vec<BackupError> = [
        stage_restore(dir.path(), "fc-backup-7.zip", T0).err(),
        stage_restore(dir.path(), "fc-backup-8.zip", T0).err(),
        stage_restore(dir.path(), "fc-backup-999.zip", T0).err(),
        stage_restore(dir.path(), "../x.zip", T0).err(),
    ]
    .into_iter()
    .flatten()
    .collect();

    assert_eq!(errors.len(), 4, "どれもエラーになる");
    let root = dir.path().to_string_lossy().into_owned();
    for e in &errors {
        let msg = e.to_string();
        assert!(!msg.contains(&root), "絶対パスを含む: {msg}");
        assert!(!msg.contains("/tmp") && !msg.contains(":\\"), "絶対パスを含む: {msg}");
        assert!(!msg.contains(X_AUTH) && !msg.contains(YT_KEY), "秘密を含む: {msg}");
        assert!(!msg.is_ascii(), "日本語のメッセージではない: {msg}");
    }
}
