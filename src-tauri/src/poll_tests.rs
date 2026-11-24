use super::*;
use crate::{api::TestCommentRequest, inject::build_test_messages};

fn request() -> PollStart {
    PollStart {
        question: " 質問 ".into(),
        options: vec![" Alpha ".into(), "Beta".into()],
        duration_secs: None,
        display: PollDisplay::BarH,
        show_percent: true,
        show_counts: true,
    }
}

fn start(poll: &Poll, request: PollStart) -> Result<PollSnapshot, PollError> {
    poll.start(request.validate()?)
}

fn comment(id: &str, text: &str) -> ChatMessage {
    let mut msg = build_test_messages(&TestCommentRequest {
        platform: "twitch".into(),
        name: String::new(),
        text: text.into(),
        kind: "normal".into(),
        amount: None,
        count: 1,
    })
    .unwrap()
    .remove(0);
    msg.author.id = id.into();
    msg.injected_test = false;
    msg
}

#[test]
fn matches_trimmed_numbers_or_full_labels() {
    let labels = request().validate().unwrap().labels;
    let unicode = PollStart {
        options: vec!["ς".into(), "b".into()],
        ..request()
    }
    .validate()
    .unwrap();
    assert_eq!(vote_index("Σ", &unicode.labels), Some(0));
    for (text, expected) in [
        (" １　", Some(0)),
        ("2", Some(1)),
        (" BeTA ", Some(1)),
        ("0", None),
        ("3", None),
        ("01", None),
        ("1!", None),
        ("+1", None),
        ("投票1", None),
        ("alphabet", None),
        ("", None),
    ] {
        assert_eq!(vote_index(text, &labels), expected, "{text:?}");
    }
    let ten = PollStart {
        options: (0..10).map(|index| format!("選択肢{index}")).collect(),
        ..request()
    }
    .validate()
    .unwrap();
    assert_eq!(vote_index("１０", &ten.labels), Some(9));
}

#[test]
fn validates_start_and_loads_broken_defaults() {
    for bad in [
        PollStart {
            question: " ".into(),
            ..request()
        },
        PollStart {
            question: "あ".repeat(101),
            ..request()
        },
        PollStart {
            options: vec!["a".into()],
            ..request()
        },
        PollStart {
            options: vec!["a".into(); 11],
            ..request()
        },
        PollStart {
            options: vec!["a".into(), " ".into()],
            ..request()
        },
        PollStart {
            options: vec!["a".into(), "あ".repeat(41)],
            ..request()
        },
        PollStart {
            options: vec!["a".into(), " A ".into()],
            ..request()
        },
        PollStart {
            duration_secs: Some(9),
            ..request()
        },
        PollStart {
            duration_secs: Some(3601),
            ..request()
        },
    ] {
        assert!(start(&Poll::default(), bad).is_err());
    }
    let saved = start(
        &Poll::default(),
        PollStart {
            question: "あ".repeat(100),
            options: (0..10).map(|n| format!("{n}{}", "あ".repeat(39))).collect(),
            duration_secs: Some(3600),
            ..request()
        },
    )
    .unwrap();
    assert_eq!(saved.options.len(), 10);

    for raw in [
        serde_json::json!({}),
        serde_json::json!({"poll":false}),
        serde_json::json!({"poll":{"display":"bad"}}),
    ] {
        let config: crate::config::AppConfig = serde_json::from_value(raw).unwrap();
        assert_eq!(config.poll, PollPreferences::default());
    }
}

#[test]
fn counts_only_visible_eligible_messages_and_replaces_votes() {
    let poll = Poll::default();
    let mut msg = comment("a", "１");
    assert!(!poll.record(&msg, &Verdict::Show));
    let snapshot = start(&poll, request()).unwrap();
    assert_eq!(snapshot.question, "質問");
    assert_eq!(snapshot.options[0].label, "Alpha");
    assert_eq!(snapshot.options[0].percent, 0.0);
    assert!(matches!(
        start(&poll, request()),
        Err(PollError::AlreadyOpen)
    ));
    assert!(!poll.record(&msg, &Verdict::Hide));
    msg.kind = MessageKind::System;
    assert!(!poll.record(&msg, &Verdict::Show));
    msg.kind = MessageKind::Normal;
    msg.route.uncounted = true;
    assert!(!poll.record(&msg, &Verdict::Show));
    msg.route.uncounted = false;
    assert!(!poll.record(&comment("", "1"), &Verdict::Show));
    msg.injected_test = true;
    assert!(poll.record(&msg, &Verdict::Show));
    assert!(!poll.record(&msg, &Verdict::Show));
    msg.platform = Platform::Youtube;
    assert!(poll.record(&msg, &Verdict::Highlight));
    assert!(poll.record(&comment("a", "Beta"), &Verdict::Show));
    let closed = poll.stop();
    assert_eq!(closed.status, PollStatus::Closed);
    assert_eq!(closed.total_voters, 2);
    assert!(closed
        .options
        .iter()
        .all(|o| o.votes == 1 && o.percent == 50.0 && o.leading));
    assert!(!poll.record(&comment("b", "2"), &Verdict::Show));
    assert_eq!(poll.reset(), PollSnapshot::default());
}

#[test]
fn stops_accepting_new_voters_at_the_limit() {
    let poll = Poll::default();
    start(&poll, request()).unwrap();
    let mut msg = comment("", "1");
    for id in 0..MAX_VOTERS {
        msg.author.id = id.to_string();
        assert!(poll.record(&msg, &Verdict::Show));
    }
    assert!(!poll.record(&comment("overflow", "2"), &Verdict::Show));
    assert!(poll.record(&comment("0", "2"), &Verdict::Show));
    let snapshot = poll.stop();
    assert_eq!(snapshot.total_voters as usize, MAX_VOTERS);
    assert_eq!(snapshot.options[1].votes, 1);
}

#[tokio::test(start_paused = true)]
async fn batches_votes_and_closes_once_at_the_monotonic_deadline() {
    let poll = Arc::new(Poll::default());
    let mut rx = poll.sender().subscribe();
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(poll.clone().run(cancel.clone()));
    start(
        &poll,
        PollStart {
            duration_secs: Some(10),
            ..request()
        },
    )
    .unwrap();
    let started = rx.borrow_and_update().clone();
    assert_eq!(started.ends_at_ms, Some(started.started_at_ms + 10_000));
    for voter in ["a", "b"] {
        assert!(poll.record(&comment(voter, "1"), &Verdict::Show));
    }
    tokio::time::advance(Duration::from_millis(99)).await;
    assert!(!rx.has_changed().unwrap());
    rx.changed().await.unwrap();
    assert_eq!(rx.borrow_and_update().total_voters, 2);
    tokio::time::advance(Duration::from_millis(9_899)).await;
    assert!(!rx.has_changed().unwrap());
    rx.changed().await.unwrap();
    assert_eq!(rx.borrow_and_update().status, PollStatus::Closed);
    assert_eq!(rx.borrow().options[0].percent, 100.0);
    poll.stop();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!rx.has_changed().unwrap());
    cancel.cancel();
    worker.await.unwrap();
}

#[test]
fn saves_display_changes_and_leaves_state_intact_on_save_failure() {
    let broken = Poll::new(|_| Err(PollError::Save), |_| {});
    assert!(matches!(start(&broken, request()), Err(PollError::Save)));
    assert_eq!(broken.snapshot().status, PollStatus::Idle);

    let applied = Arc::new(Mutex::new(None));
    let captured = applied.clone();
    let poll = Poll::new(
        move |change| {
            if let PreferenceChange::Display(settings) = change {
                *captured.lock().unwrap() = Some(settings);
            }
            Ok(())
        },
        |_| {},
    );
    start(&poll, request()).unwrap();
    poll.record(&comment("a", "2"), &Verdict::Show);
    let settings = PollDisplaySettings {
        display: PollDisplay::Donut,
        show_percent: false,
        show_counts: true,
    };
    let snapshot = poll.set_display(settings).unwrap();
    assert_eq!(*applied.lock().unwrap(), Some(settings));
    assert_eq!(snapshot.settings, settings);
    assert_eq!(snapshot.total_voters, 1);
    poll.stop();
    assert_eq!(
        poll.set_display(PollDisplaySettings::default())
            .unwrap()
            .status,
        PollStatus::Closed
    );
}
