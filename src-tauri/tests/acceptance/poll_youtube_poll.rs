//! YouTube 公式投票 受け入れテスト: アンケートとの連携(poll-pr3-youtube.md「連携」)。
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

fn begin(poll: &Poll, source: PollSource) -> String {
    poll.start(start_req(source).validate().unwrap()).unwrap();
    poll.snapshot().id
}

fn vote(text: &str) -> fast_comment_lib::model::ChatMessage {
    let mut m = msg("m1");
    m.fragments = vec![Fragment::text(text.to_string())];
    m
}

fn votes(poll: &Poll) -> Vec<u32> {
    poll.snapshot().options.iter().map(|o| o.votes).collect()
}

#[test]
fn youtube_poll_ignores_chat_votes() {
    let poll = new_poll();
    begin(&poll, PollSource::Youtube);
    assert_eq!(serde_json::to_value(poll.snapshot()).unwrap()["source"], "youtube");
    assert!(!poll.record(&vote("1"), &Verdict::Show));
    assert_eq!(votes(&poll), vec![0, 0, 0]);
    assert_eq!(poll.snapshot().total_voters, 0);
}

#[test]
fn apply_external_and_remote_timing_work_for_youtube_but_not_chat() {
    // Youtube: 票の上書きと closed
    let poll = new_poll();
    let id = begin(&poll, PollSource::Youtube);
    assert!(poll.apply_external(&id, &[5, 3, 2], false));
    let s = poll.snapshot();
    assert_eq!(votes(&poll), vec![5, 3, 2]);
    assert_eq!(s.total_voters, 10);
    assert_eq!(s.status, PollStatus::Open);
    assert!(poll.apply_external(&id, &[1, 1, 2], true));
    assert_eq!(votes(&poll), vec![1, 1, 2]);
    assert_eq!(poll.snapshot().status, PollStatus::Closed);

    // Youtube: id の照合は今のまま(古い id・空・別 id は無視)
    let poll = new_poll();
    let id = begin(&poll, PollSource::Youtube);
    let before = poll.snapshot();
    for stale in [format!("{id}x"), String::new(), "00000000-0000-0000-0000-000000000000".to_owned()] {
        assert!(!poll.apply_external(&stale, &[9, 9, 9], true), "poll_id={stale}");
        assert_eq!(poll.snapshot(), before, "古い poll_id で状態が変わった");
    }

    // Youtube: set_remote_timing が開始・終了時刻を書き換える
    let poll = new_poll();
    let id = begin(&poll, PollSource::Youtube);
    poll.set_remote_timing(&id, 1_000, 61_000);
    let s = poll.snapshot();
    assert_eq!(s.started_at_ms, 1_000);
    assert_eq!(s.ends_at_ms, Some(61_000));

    // Chat: どちらも働かない
    let chat = new_poll();
    let chat_id = begin(&chat, PollSource::Chat);
    let before = chat.snapshot();
    assert!(!chat.apply_external(&chat_id, &[9, 9, 9], true), "Chat の投票に適用された");
    assert_eq!(chat.snapshot(), before, "Chat の投票の状態が変わった");
    chat.set_remote_timing(&chat_id, 1_000, 61_000);
    assert_eq!(chat.snapshot(), before, "Chat の投票の時刻が変わった");
}

#[test]
fn twitch_behaviour_is_unchanged() {
    let poll = new_poll();
    let id = begin(&poll, PollSource::Twitch);
    // コメント投票は数えない
    assert!(!poll.record(&vote("1"), &Verdict::Show));
    assert_eq!(votes(&poll), vec![0, 0, 0]);
    // apply_external は今までどおり上書きして閉じる
    assert!(poll.apply_external(&id, &[5, 3, 2], false));
    let s = poll.snapshot();
    assert_eq!(votes(&poll), vec![5, 3, 2]);
    assert_eq!(s.total_voters, 10);
    assert_eq!(s.status, PollStatus::Open);
    assert!(!poll.apply_external(&format!("{id}x"), &[9, 9, 9], true), "古い id は無視");
    assert_eq!(votes(&poll), vec![5, 3, 2]);
    // set_remote_timing も今までどおり働く
    poll.set_remote_timing(&id, 1_000, 61_000);
    assert_eq!(poll.snapshot().ends_at_ms, Some(61_000));
    assert!(poll.apply_external(&id, &[1, 1, 2], true));
    assert_eq!(poll.snapshot().status, PollStatus::Closed);
    assert_eq!(votes(&poll), vec![1, 1, 2]);
}
