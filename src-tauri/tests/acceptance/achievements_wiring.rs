// registered in main.rs by story ACH-003
//! 配信イベントの配布(acceptance.md §3.5、D10)。時刻は引数で与え、待ちは上限つきの受信だけ。
use crate::achievements_common::*;
use fast_comment_lib::achievements::engine::AchievementEngine;
use fast_comment_lib::achievements::model::{Metric, Window};
use fast_comment_lib::live_fanout::spawn_live_fanout;
use fast_comment_lib::model::{LiveEvent, Platform};
use std::time::Duration;
use tokio::sync::mpsc;

const IDENT: &str = "https://twitcasting.tv/Alice";

fn four_events() -> Vec<LiveEvent> {
    vec![
        started(Platform::Twitcasting, IDENT, "42"),
        ended(Platform::Twitcasting, IDENT),
        started(Platform::Twitcasting, IDENT, "43"),
        ended(Platform::Twitcasting, IDENT),
    ]
}

async fn drain(rx: &mut mpsc::Receiver<LiveEvent>, n: usize) -> Vec<LiveEvent> {
    let mut out = vec![];
    for i in 0..n {
        match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            Ok(Some(e)) => out.push(e),
            other => panic!("{} 件目を受け取れなかった ({:?})", i + 1, other.map(|o| o.is_some())),
        }
    }
    out
}

#[tokio::test(flavor = "current_thread")]
async fn live_events_reach_autopost_and_achievements_in_order() {
    let (tx, rx) = mpsc::channel(16);
    let (autopost_tx, mut autopost_rx) = mpsc::channel(16);
    let (engine_tx, mut engine_rx) = mpsc::channel(16);
    let handle = spawn_live_fanout(rx, vec![autopost_tx, engine_tx]);
    for ev in four_events() {
        tx.send(ev).await.unwrap();
    }
    drop(tx);

    let for_autopost = drain(&mut autopost_rx, 4).await;
    let for_engine = drain(&mut engine_rx, 4).await;
    assert_eq!(for_autopost, four_events(), "autopost は 4 件を送信順に受け取る");
    assert_eq!(for_engine, four_events(), "エンジン側も同じ 4 件を同じ順で受け取る");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("入力が閉じたら fanout は終わる")
        .expect("panic しない");

    // エンジン側に流す。コメントは各配信の間に 1 件ずつ。
    let dir = tempfile::tempdir().unwrap();
    let engine = AchievementEngine::open(
        dir.path(),
        mk_defs(vec![
            streamer_count("streams2", Window::Lifetime, Metric::Streams, 2.0),
            streamer_count("pc1", Window::PerStream, Metric::Comments, 1.0),
        ]),
        T0,
    );
    let mut unlocks = vec![];
    let comment = |id: &str| chat(id, Platform::Twitcasting, "alice", "tc:bob");
    unlocks.extend(engine.on_live_event(&for_engine[0], T0));
    unlocks.extend(engine.observe(&comment("c1"), T0 + 500));
    unlocks.extend(engine.on_live_event(&for_engine[1], T0 + 1_000));
    unlocks.extend(engine.on_live_event(&for_engine[2], T0 + 2_000));
    unlocks.extend(engine.observe(&comment("c2"), T0 + 2_500));
    unlocks.extend(engine.on_live_event(&for_engine[3], T0 + 3_000));

    assert_eq!(ids(&unlocks), vec!["pc1", "streams2"], "Streams>=2 は 1 回、PerStream の comments>=1 も 1 回だけ");
    assert_eq!(engine.current_epoch(), Some(format!("e:{T0}")), "30 分以内なので epoch は 1 つのまま");
}

#[tokio::test(flavor = "current_thread")]
async fn live_fanout_survives_one_closed_output() {
    let (tx, rx) = mpsc::channel(16);
    let (dead_tx, dead_rx) = mpsc::channel(16);
    let (live_tx, mut live_rx) = mpsc::channel(16);
    let _handle = spawn_live_fanout(rx, vec![dead_tx, live_tx]);
    drop(dead_rx);
    for ev in four_events() {
        tx.send(ev).await.unwrap();
    }
    let got = drain(&mut live_rx, 4).await;
    assert_eq!(got, four_events(), "片方が閉じても、もう片方には届き続ける");
}
