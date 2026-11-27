//! 起動時に開くアプリと URL の受け入れテスト(.omc/plans/startup-launch.md「受け入れ基準」)。
//! 公開の境界だけを使う。承認ストアはメモリ実装、保存先は一時ディレクトリ。
//! 型の derive(Clone / PartialEq / Debug)はプランに無いので前提にしない。

use fast_comment_lib::startup_launch::{
    approval_mac, load_for_startup, load_items, save_approved, validate_items, ApprovalStore,
    LaunchError, LaunchItem, LaunchKind, StartupDecision,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Mutex;

const FILE: &str = "launch.json";

#[derive(Default)]
struct MemStore(Mutex<Option<(Vec<u8>, Vec<u8>)>>);

impl ApprovalStore for MemStore {
    fn load(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>, LaunchError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, key: &[u8], mac: &[u8]) -> Result<(), LaunchError> {
        *self.0.lock().unwrap() = Some((key.to_vec(), mac.to_vec()));
        Ok(())
    }
}

fn item(id: &str, kind: LaunchKind, target: &str, args: &[&str], enabled: bool, label: Option<&str>) -> LaunchItem {
    LaunchItem {
        id: id.into(),
        kind,
        target: target.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        enabled,
        label: label.map(|s| s.to_string()),
        hide_window: false,
    }
}

fn exe(target: &str) -> LaunchItem {
    item("a", LaunchKind::Exe, target, &[], true, None)
}

fn url(target: &str) -> LaunchItem {
    item("u", LaunchKind::Url, target, &[], true, None)
}

fn is_ok(r: Result<(), LaunchError>) -> bool {
    r.is_ok()
}

fn ok_or_panic<T>(r: Result<T, LaunchError>, what: &str) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{what}: Err({e})"),
    }
}

/// 3 項目: 有効な exe / 無効な URL / 有効な URL。
fn sample() -> Vec<LaunchItem> {
    vec![
        item("a", LaunchKind::Exe, "C:\\Tools\\app.exe", &["--x", "1"], true, Some("アプリ")),
        item("b", LaunchKind::Url, "https://example.com/b", &[], false, Some("無効")),
        item("c", LaunchKind::Url, "https://example.com/c", &[], true, None),
    ]
}

fn save_sample(dir: &Path, store: &MemStore) {
    ok_or_panic(save_approved(dir, store, sample(), true), "save_approved");
}

fn read_json(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(FILE)).unwrap()).unwrap()
}

fn write_json(dir: &Path, v: &Value) {
    std::fs::write(dir.join(FILE), serde_json::to_string_pretty(v).unwrap()).unwrap();
}

fn run_ids(d: StartupDecision) -> Vec<String> {
    match d {
        StartupDecision::Run(v) => v.into_iter().map(|i| i.id).collect(),
        StartupDecision::Nothing => panic!("Run を期待したが Nothing"),
        StartupDecision::NeedsApproval => panic!("Run を期待したが NeedsApproval"),
    }
}

fn needs_approval(d: StartupDecision) -> bool {
    matches!(d, StartupDecision::NeedsApproval)
}

// ---- validate ----

#[test]
fn validate_accepts_exe_and_https_url() {
    assert!(is_ok(validate_items(&[exe("C:\\Tools\\app.exe")])));
    assert!(is_ok(validate_items(&[url("https://example.com/path?q=1")])));
    assert!(is_ok(validate_items(&[url("http://example.com")])));
    assert!(is_ok(validate_items(&[exe("C:\\Tools\\app.exe"), url("https://example.com")])));
}

#[test]
fn validate_rejects_non_exe_extensions() {
    for bad in [
        "C:\\Tools\\a.lnk",
        "C:\\Tools\\a.ps1",
        "C:\\Tools\\a.vbs",
        "C:\\Tools\\a.msi",
        "C:\\Tools\\noext",
    ] {
        assert!(!is_ok(validate_items(&[exe(bad)])), "拒否を期待: {bad}");
    }
    assert!(is_ok(validate_items(&[exe("C:\\Tools\\APP.EXE")])), ".EXE は受け入れる");
    assert!(is_ok(validate_items(&[exe("C:\\Tools\\app.Exe")])), ".Exe は受け入れる");
}

#[test]
fn validate_rejects_relative_paths() {
    for bad in ["app.exe", ".\\app.exe", "..\\x.exe"] {
        assert!(!is_ok(validate_items(&[exe(bad)])), "拒否を期待: {bad}");
    }
    assert!(is_ok(validate_items(&[exe("C:\\Tools\\app.exe")])));
    assert!(is_ok(validate_items(&[exe("\\\\server\\share\\app.exe")])));
}

#[test]
fn validate_rejects_bad_urls() {
    for bad in ["file:///C:/x.html", "javascript:alert(1)", "ftp://example.com/x", "https://user:pass@x", ""] {
        assert!(!is_ok(validate_items(&[url(bad)])), "拒否を期待: {bad:?}");
    }
}

#[test]
fn validate_limits() {
    // 項目数: 20 は可、21 は不可
    let many = |n: usize| -> Vec<LaunchItem> {
        (0..n).map(|i| item(&format!("id{i}"), LaunchKind::Url, "https://example.com", &[], true, None)).collect()
    };
    assert!(is_ok(validate_items(&many(20))));
    assert!(!is_ok(validate_items(&many(21))));

    // target: 2048 文字は可、2049 文字は不可(exe・URL の両方)
    let exe_len = |n: usize| format!("C:\\{}.exe", "a".repeat(n - 7));
    assert_eq!(exe_len(2048).chars().count(), 2048);
    assert!(is_ok(validate_items(&[exe(&exe_len(2048))])));
    assert!(!is_ok(validate_items(&[exe(&exe_len(2049))])));
    let url_len = |n: usize| format!("https://example.com/{}", "a".repeat(n - 20));
    assert_eq!(url_len(2048).chars().count(), 2048);
    assert!(is_ok(validate_items(&[url(&url_len(2048))])));
    assert!(!is_ok(validate_items(&[url(&url_len(2049))])));

    // args: 32 個は可、33 個は不可
    let with_args = |args: Vec<String>| {
        let mut i = exe("C:\\Tools\\app.exe");
        i.args = args;
        i
    };
    assert!(is_ok(validate_items(&[with_args(vec!["x".into(); 32])])));
    assert!(!is_ok(validate_items(&[with_args(vec!["x".into(); 33])])));

    // args 1 個: 1024 文字は可、1025 文字は不可。chars で数える(多バイト文字 1 つ = 1)
    assert!(is_ok(validate_items(&[with_args(vec!["あ".repeat(1024)])])));
    assert!(!is_ok(validate_items(&[with_args(vec!["あ".repeat(1025)])])));

    // label: 100 文字は可、101 文字は不可。chars で数える
    let with_label = |s: String| {
        let mut i = exe("C:\\Tools\\app.exe");
        i.label = Some(s);
        i
    };
    assert!(is_ok(validate_items(&[with_label("あ".repeat(100))])));
    assert!(!is_ok(validate_items(&[with_label("あ".repeat(101))])));
}

// ---- mac ----

#[test]
fn mac_ignores_label_but_covers_everything_else() {
    let key = [7u8; 32];
    let base = approval_mac(&key, &sample());
    assert!(!base.is_empty());
    assert_eq!(base, approval_mac(&key, &sample()), "同じ入力なら同じ値");
    assert_ne!(base, approval_mac(&[8u8; 32], &sample()), "鍵が違えば別の値");

    // label だけ違う: 同じ
    let mut v = sample();
    v[0].label = Some("別の名前".into());
    v[2].label = Some("追加".into());
    assert_eq!(base, approval_mac(&key, &v), "label は mac に影響しない");
    let mut v = sample();
    v[0].label = None;
    assert_eq!(base, approval_mac(&key, &v), "label の有無も影響しない");

    // ほかの欄は 1 つ変えるたびに別の値
    let mut v = sample();
    v[0].id = "z".into();
    assert_ne!(base, approval_mac(&key, &v), "id");
    let mut v = sample();
    v[1].kind = LaunchKind::Exe;
    assert_ne!(base, approval_mac(&key, &v), "kind");
    let mut v = sample();
    v[0].target = "C:\\Tools\\other.exe".into();
    assert_ne!(base, approval_mac(&key, &v), "target");
    let mut v = sample();
    v[0].args = vec!["--x".into(), "2".into()];
    assert_ne!(base, approval_mac(&key, &v), "args の値");
    let mut v = sample();
    v[0].args = vec!["1".into(), "--x".into()];
    assert_ne!(base, approval_mac(&key, &v), "args の順番");
    let mut v = sample();
    v[1].enabled = true;
    assert_ne!(base, approval_mac(&key, &v), "enabled");
}

// ---- 保存と承認 ----

#[test]
fn save_requires_confirmation() {
    // 初回: ファイルもストアも作られない
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    let r = save_approved(dir.path(), &store, sample(), false);
    assert!(matches!(r, Err(LaunchError::NotConfirmed)));
    assert!(!dir.path().join(FILE).exists());
    assert!(store.load().unwrap().is_none());

    // 既存の保存があっても、拒否されたら何も変わらない
    save_sample(dir.path(), &store);
    let file_before = std::fs::read(dir.path().join(FILE)).unwrap();
    let store_before = store.load().unwrap();
    let other = vec![item("z", LaunchKind::Url, "https://example.com/z", &[], true, None)];
    let r = save_approved(dir.path(), &store, other, false);
    assert!(matches!(r, Err(LaunchError::NotConfirmed)));
    assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), file_before);
    assert_eq!(store.load().unwrap(), store_before);
}

#[test]
fn startup_runs_only_when_approved() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    save_sample(dir.path(), &store);
    // enabled のものだけ、登録順
    assert_eq!(run_ids(load_for_startup(dir.path(), &store)), vec!["a", "c"]);
    // 保存した 3 項目すべてが UI 用には読める(承認は問わない)
    let all = ok_or_panic(load_items(dir.path()), "load_items");
    assert_eq!(all.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["a", "b", "c"]);
}

#[test]
fn tampered_file_needs_approval() {
    let edits: [(&str, fn(&mut Value)); 3] = [
        ("target", |v| v["items"][0]["target"] = json!("C:\\Evil\\evil.exe")),
        ("args", |v| v["items"][0]["args"] = json!(["--evil"])),
        ("enabled", |v| v["items"][1]["enabled"] = json!(true)),
    ];
    for (name, edit) in edits {
        let dir = tempfile::tempdir().unwrap();
        let store = MemStore::default();
        save_sample(dir.path(), &store);
        let mut v = read_json(dir.path());
        edit(&mut v);
        write_json(dir.path(), &v);
        assert!(needs_approval(load_for_startup(dir.path(), &store)), "{name} の書き換えは NeedsApproval");
    }

    // label だけなら Run のまま
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    save_sample(dir.path(), &store);
    let mut v = read_json(dir.path());
    v["items"][0]["label"] = json!("書き換えた表示名");
    write_json(dir.path(), &v);
    assert_eq!(run_ids(load_for_startup(dir.path(), &store)), vec!["a", "c"]);
}

#[test]
fn missing_store_or_corrupt_file_needs_approval() {
    // ストアが空
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    save_sample(dir.path(), &store);
    let empty = MemStore::default();
    assert!(needs_approval(load_for_startup(dir.path(), &empty)));

    // 壊れた JSON
    std::fs::write(dir.path().join(FILE), "{ not json").unwrap();
    assert!(needs_approval(load_for_startup(dir.path(), &store)));

    // version 2
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    save_sample(dir.path(), &store);
    let mut v = read_json(dir.path());
    v["version"] = json!(2);
    write_json(dir.path(), &v);
    assert!(needs_approval(load_for_startup(dir.path(), &store)));

    // mac は一致しているが validate に落ちる内容(.ps1)
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    save_sample(dir.path(), &store);
    let (key, _) = store.load().unwrap().unwrap();
    let bad = vec![item("a", LaunchKind::Exe, "C:\\Tools\\run.ps1", &[], true, None)];
    store.save(&key, &approval_mac(&key, &bad)).unwrap();
    write_json(
        dir.path(),
        &json!({ "version": 1, "items": [
            { "id": "a", "kind": "exe", "target": "C:\\Tools\\run.ps1", "args": [], "enabled": true }
        ]}),
    );
    assert!(needs_approval(load_for_startup(dir.path(), &store)));
}

#[test]
fn no_file_is_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    assert!(matches!(load_for_startup(dir.path(), &store), StartupDecision::Nothing));
    // ストアに値があってもファイルが無ければ Nothing
    store.save(&[1u8; 32], &[2u8; 32]).unwrap();
    assert!(matches!(load_for_startup(dir.path(), &store), StartupDecision::Nothing));
}

#[test]
fn errors_do_not_leak_targets_or_args() {
    const SECRETS: [&str; 4] = ["SECRETARG", "SECRETPASS", "SECRETEXE", "SECRETDIR"];
    let mut messages: Vec<String> = Vec::new();
    let mut push = |r: Result<(), LaunchError>| {
        if let Err(e) = r {
            messages.push(format!("{e}"));
        }
    };

    // validate に落ちる項目(拡張子・ユーザー情報・相対パス・上限超過)
    let mut bad_ext = item("a", LaunchKind::Exe, "C:\\SECRETEXE\\run.ps1", &["--token=SECRETARG"], true, None);
    push(validate_items(std::slice::from_ref(&bad_ext)));
    bad_ext.target = "SECRETEXE\\run.exe".into();
    push(validate_items(&[bad_ext]));
    push(validate_items(&[url("https://user:SECRETPASS@example.com/SECRETEXE")]));
    push(validate_items(&[item("a", LaunchKind::Exe, "C:\\SECRETEXE\\a.exe", &["SECRETARG"; 33], true, None)]));

    // confirmed なしの保存
    let dir = tempfile::tempdir().unwrap();
    let store = MemStore::default();
    push(save_approved(
        dir.path(),
        &store,
        vec![item("a", LaunchKind::Exe, "C:\\SECRETEXE\\a.exe", &["SECRETARG"], true, None)],
        false,
    ));

    // 書き込みに失敗する保存先(存在しないディレクトリ。作られて成功するなら検査対象外)
    let missing = dir.path().join("SECRETDIR").join("nested");
    push(save_approved(
        &missing,
        &store,
        vec![item("a", LaunchKind::Exe, "C:\\SECRETEXE\\a.exe", &["SECRETARG"], true, None)],
        true,
    ));

    // 壊れたファイルの読み込み
    let broken = tempfile::tempdir().unwrap();
    std::fs::write(broken.path().join(FILE), "{ \"items\": [ \"SECRETARG SECRETEXE SECRETPASS").unwrap();
    push(load_items(broken.path()).map(|_| ()));

    assert!(messages.len() >= 5, "拒否されるはずの入力が通っている: {messages:?}");
    for m in &messages {
        for s in SECRETS {
            assert!(!m.contains(s), "エラー文に {s} が含まれる: {m}");
        }
        assert!(!m.contains(dir.path().to_str().unwrap()), "エラー文に保存先のパスが含まれる: {m}");
    }
}
