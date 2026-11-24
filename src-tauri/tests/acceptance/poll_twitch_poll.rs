//! Twitch 公式 Polls 受け入れテスト: アンケートとの連携(poll-pr2-twitch.md「アンケートとの連携」)。
//! `Poll` / `PollStart` / `PollSource` などは受け入れテストから使えるよう pub であること(境界の前提)。
use crate::common::msg;
use fast_comment_lib::model::Fragment;
use fast_comment_lib::moderation::Verdict;
use fast_comment_lib::poll::{Poll, PollDisplay, PollSource, PollStart, PollStatus};

fn start_req(source: PollSource) -> PollStart {
    PollStart {
        question: "好きなのは?".into(),
        options: vec!["A".into(), "B".into(), "C".into()],
        duration_secs: Some(60),
        display: PollDisplay::BarH,
        show_percent: true,
        show_counts: true,
        source,
    }
}

fn new_poll() -> Poll {
    Poll::new(|_| Ok(()), |_| {})
}

fn begin(poll: &Poll, source: PollSource) {
    poll.start(start_req(source).validate().unwrap()).unwrap();
}

fn vote(text: &str) -> fast_comment_lib::model::ChatMessage {
    let mut m = msg("m1");
    m.fragments = vec![Fragment::text(text.to_string())];
    m
}

fn votes(poll: &Poll) -> Vec<u32> {
    poll.snapshot().options.iter().map(|o| o.votes).collect()
}

/// 現在の投票の id(スナップショットの `id`、UUID の文字列)。
fn find_poll_id(poll: &Poll) -> Option<String> {
    Some(poll.snapshot().id).filter(|id| !id.is_empty())
}

#[test]
fn twitch_poll_ignores_chat_votes() {
    let poll = new_poll();
    begin(&poll, PollSource::Twitch);
    assert!(!poll.record(&vote("1"), &Verdict::Show));
    assert_eq!(votes(&poll), vec![0, 0, 0]);
    assert_eq!(poll.snapshot().total_voters, 0);
}

#[test]
fn apply_external_overwrites_votes_and_closes() {
    let poll = new_poll();
    begin(&poll, PollSource::Twitch);
    let id = find_poll_id(&poll).expect("Twitch の投票の poll_id が見つからない");

    assert!(poll.apply_external(&id, &[5, 3, 2], false));
    let s = poll.snapshot();
    assert_eq!(votes(&poll), vec![5, 3, 2]);
    assert_eq!(s.total_voters, 10);
    assert_eq!(s.status, PollStatus::Open);
    assert_eq!(s.options.iter().map(|o| o.percent).collect::<Vec<_>>(), vec![50.0, 30.0, 20.0]);
    assert_eq!(s.options.iter().map(|o| o.leading).collect::<Vec<_>>(), vec![true, false, false]);

    // 上書き(足し込みではない)+ closed で閉じる
    assert!(poll.apply_external(&id, &[1, 1, 2], true));
    let s = poll.snapshot();
    assert_eq!(votes(&poll), vec![1, 1, 2]);
    assert_eq!(s.total_voters, 4);
    assert_eq!(s.status, PollStatus::Closed);
    assert_eq!(s.options.iter().map(|o| o.percent).collect::<Vec<_>>(), vec![25.0, 25.0, 50.0]);
    assert_eq!(s.options.iter().map(|o| o.leading).collect::<Vec<_>>(), vec![false, false, true]);
}

#[test]
fn apply_external_ignores_stale_or_chat_polls() {
    // 古い poll_id
    let poll = new_poll();
    begin(&poll, PollSource::Twitch);
    let id = find_poll_id(&poll).expect("Twitch の投票の poll_id が見つからない");
    assert!(poll.apply_external(&id, &[4, 0, 0], false));
    let before = poll.snapshot();
    for stale in [format!("{id}x"), String::new(), "00000000-0000-0000-0000-000000000000".to_owned()] {
        assert!(!poll.apply_external(&stale, &[9, 9, 9], true), "poll_id={stale}");
        assert_eq!(poll.snapshot(), before, "古い poll_id で状態が変わった");
    }

    // source=Chat の投票には適用しない
    let chat = new_poll();
    begin(&chat, PollSource::Chat);
    let before = chat.snapshot();
    let chat_id = chat.snapshot().id;
    assert!(!chat.apply_external(&chat_id, &[9, 9, 9], true), "Chat の投票に適用された");
    assert_eq!(chat.snapshot(), before, "Chat の投票の状態が変わった");
}

#[test]
fn old_start_body_without_source_is_chat() {
    let body = serde_json::json!({
        "question": "q", "options": ["a", "b"], "durationSecs": 30,
        "display": "bar-h", "showPercent": true, "showCounts": true
    });
    let start: PollStart = serde_json::from_value(body).expect("source の無い旧本文を読めること");
    let poll = new_poll();
    poll.start(start.validate().unwrap()).unwrap();
    assert_eq!(serde_json::to_value(poll.snapshot()).unwrap()["source"], "chat");
    // Chat なのでコメント投票は数える
    assert!(poll.record(&vote("1"), &Verdict::Show));
}
