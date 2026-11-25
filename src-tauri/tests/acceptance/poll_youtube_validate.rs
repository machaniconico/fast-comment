//! YouTube 公式投票 受け入れテスト: 検証(poll-pr3-youtube.md「検証」)。
use fast_comment_lib::poll::{PollDisplay, PollSource, PollStart};
use fast_comment_lib::sources::youtube_polls::validate_youtube;

fn opts(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("選択肢{i}")).collect()
}

fn ok(q: &str, o: &[String]) -> bool {
    validate_youtube(q, o).is_ok()
}

#[test]
fn validate_youtube_limits() {
    let o2 = opts(2);
    // 質問: trim 後 1〜100 文字
    assert!(ok("x", &o2), "質問 1 文字");
    assert!(ok(&"あ".repeat(100), &o2), "質問 100 文字");
    assert!(!ok(&"あ".repeat(101), &o2), "質問 101 文字");
    assert!(!ok("", &o2), "質問が空(0 文字)");
    assert!(!ok("   　", &o2), "質問が空白だけ");
    assert!(ok(&format!("  {}  ", "a".repeat(100)), &o2), "trim 後 100 文字なら前後の空白は数えない");
    // chars で数える(バイト数や UTF-16 ではない)
    assert!(ok(&"😀".repeat(100), &o2), "絵文字 100 個");
    assert!(!ok(&"😀".repeat(101), &o2), "絵文字 101 個");
    // 選択肢の個数: 2〜4
    assert!(!ok("q", &opts(0)), "選択肢 0 個");
    assert!(!ok("q", &opts(1)), "選択肢 1 個");
    assert!(ok("q", &opts(2)), "選択肢 2 個");
    assert!(ok("q", &opts(4)), "選択肢 4 個");
    assert!(!ok("q", &opts(5)), "選択肢 5 個");
    // 選択肢の長さ: trim 後 1〜40 文字
    let long = |n: usize| vec!["あ".repeat(n), "b".to_string()];
    assert!(ok("q", &long(1)), "選択肢 1 文字");
    assert!(ok("q", &long(40)), "選択肢 40 文字");
    assert!(!ok("q", &long(41)), "選択肢 41 文字");
    assert!(!ok("q", &long(0)), "選択肢 0 文字");
    assert!(!ok("q", &["  ".to_string(), "b".into()]), "選択肢が空白だけ");
    assert!(ok("q", &["😀".repeat(40), "b".into()]), "選択肢 絵文字 40 個");
    assert!(!ok("q", &["😀".repeat(41), "b".into()]), "選択肢 絵文字 41 個");
    assert!(ok("q", &[format!(" {} ", "a".repeat(40)), "b".into()]), "trim 後 40 文字");
    // trim の後の重複は拒否する
    assert!(!ok("q", &["A".to_string(), "A".into()]), "完全一致の重複");
    assert!(!ok("q", &["A".to_string(), "  A ".into()]), "trim 後に一致する重複");
}

fn youtube_start(duration_secs: Option<u32>) -> PollStart {
    PollStart {
        question: "好きなのは?".into(),
        options: vec!["A".into(), "B".into(), "C".into()],
        duration_secs,
        display: PollDisplay::BarH,
        show_percent: true,
        show_counts: true,
        source: PollSource::Youtube,
    }
}

#[test]
fn youtube_start_allows_no_duration_and_checks_range() {
    assert!(youtube_start(None).validate().is_ok(), "期間 None");
    assert!(youtube_start(Some(9)).validate().is_err(), "期間 9");
    assert!(youtube_start(Some(10)).validate().is_ok(), "期間 10");
    assert!(youtube_start(Some(3600)).validate().is_ok(), "期間 3600");
    assert!(youtube_start(Some(3601)).validate().is_err(), "期間 3601");
}

#[test]
fn youtube_start_uses_youtube_limits_for_text() {
    let mut s = youtube_start(None);
    s.question = "あ".repeat(100);
    s.options = (0..4).map(|i| format!("選択肢{i}")).collect();
    assert!(s.validate().is_ok(), "質問 100 文字・選択肢 4 個は通る(Chat・Twitch の上限ではない)");
    let mut s = youtube_start(None);
    s.options = (0..5).map(|i| format!("選択肢{i}")).collect();
    assert!(s.validate().is_err(), "選択肢 5 個は拒否");
    let mut s = youtube_start(None);
    s.question = "あ".repeat(101);
    assert!(s.validate().is_err(), "質問 101 文字は拒否");
}
