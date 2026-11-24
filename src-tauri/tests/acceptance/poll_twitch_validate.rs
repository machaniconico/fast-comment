//! Twitch 公式 Polls 受け入れテスト: 検証(poll-pr2-twitch.md「検証」)。
use fast_comment_lib::sources::twitch_polls::validate_twitch;

fn opts(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("選択肢{i}")).collect()
}

fn ok(q: &str, o: &[String], d: Option<u32>) -> bool {
    validate_twitch(q, o, d).is_ok()
}

#[test]
fn validate_twitch_limits() {
    let o2 = opts(2);
    // 質問: trim 後 1〜60 文字
    assert!(ok(&"あ".repeat(60), &o2, Some(60)), "質問 60 文字");
    assert!(!ok(&"あ".repeat(61), &o2, Some(60)), "質問 61 文字");
    assert!(!ok("", &o2, Some(60)), "質問が空");
    assert!(!ok("   　", &o2, Some(60)), "質問が空白だけ");
    assert!(ok(&format!("  {}  ", "a".repeat(60)), &o2, Some(60)), "trim 後 60 文字なら前後の空白は数えない");
    assert!(ok("x", &o2, Some(60)), "質問 1 文字");
    // chars で数える(バイト数や UTF-16 ではない)
    assert!(ok(&"😀".repeat(60), &o2, Some(60)), "絵文字 60 個");
    assert!(!ok(&"😀".repeat(61), &o2, Some(60)), "絵文字 61 個");
    // 選択肢の個数: 2〜5
    assert!(!ok("q", &opts(1), Some(60)), "選択肢 1 個");
    assert!(ok("q", &opts(2), Some(60)), "選択肢 2 個");
    assert!(ok("q", &opts(5), Some(60)), "選択肢 5 個");
    assert!(!ok("q", &opts(6), Some(60)), "選択肢 6 個");
    assert!(!ok("q", &opts(0), Some(60)), "選択肢 0 個");
    // 選択肢の長さ: trim 後 1〜25 文字
    let long = |n: usize| vec!["あ".repeat(n), "b".to_string()];
    assert!(ok("q", &long(25), Some(60)), "選択肢 25 文字");
    assert!(!ok("q", &long(26), Some(60)), "選択肢 26 文字");
    assert!(ok("q", &["😀".repeat(25), "b".into()], Some(60)), "選択肢 絵文字 25 個");
    assert!(!ok("q", &["😀".repeat(26), "b".into()], Some(60)), "選択肢 絵文字 26 個");
    assert!(!ok("q", &["  ".to_string(), "b".into()], Some(60)), "選択肢が空白だけ");
    assert!(ok("q", &[format!(" {} ", "a".repeat(25)), "b".into()], Some(60)), "trim 後 25 文字");
    // 期間: 必須、15〜1800 秒
    assert!(!ok("q", &o2, None), "期間 None");
    assert!(!ok("q", &o2, Some(14)), "期間 14");
    assert!(ok("q", &o2, Some(15)), "期間 15");
    assert!(ok("q", &o2, Some(1800)), "期間 1800");
    assert!(!ok("q", &o2, Some(1801)), "期間 1801");
}
