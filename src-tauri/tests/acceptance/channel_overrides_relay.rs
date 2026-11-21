// registered in main.rs by story OV-002 (needs MessageRoute and common.rs msg() carrying the `route` line)
//! 枠ごとの上書き 受け入れテスト 3.3: `spawn_origin_relay`(発生元キーを付けて upstream へ流す)。
//! テストは `current_thread` で動かす。spawn したタスクは await するまで走らないので、「戻る前に購読済み」も検査できる。
#![allow(dead_code)]

use crate::channel_overrides_common::*;
use crate::common::msg;
use fast_comment_lib::config::ChannelTtsVoice;
use fast_comment_lib::model::ChatMessage;
use fast_comment_lib::sources::origin_relay::spawn_origin_relay;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const KEY: &str = "twitch:chan";

fn upstream() -> (broadcast::Sender<ChatMessage>, broadcast::Receiver<ChatMessage>) {
    let (tx, rx) = broadcast::channel(8192);
    (tx, rx)
}

#[tokio::test(flavor = "current_thread")]
async fn relay_sets_origin_key_and_preserves_other_fields() {
    let (up, mut up_rx) = upstream();
    let tx = spawn_origin_relay(KEY.to_string(), up.clone(), CancellationToken::new());

    let mut sent = msg("a");
    sent.skip_tts = true;
    sent.injected_test = true;
    sent.route.channel_tts_voice = Some(ChannelTtsVoice { voicevox_speaker: Some(3), ..Default::default() });
    tx.send(sent.clone()).unwrap();

    let got = recv_within(&mut up_rx, "relay の出力").await;
    let mut expected = sent;
    expected.route.origin_key = Some(KEY.to_string());
    assert_eq!(got, expected, "origin_key 以外は何も変わらない");
}

#[tokio::test(flavor = "current_thread")]
async fn relay_overwrites_preexisting_origin_key() {
    let (up, mut up_rx) = upstream();
    let tx = spawn_origin_relay(KEY.to_string(), up.clone(), CancellationToken::new());

    let mut sent = msg("a");
    sent.route.origin_key = Some("youtube:spoofed".to_string());
    tx.send(sent).unwrap();

    let got = recv_within(&mut up_rx, "relay の出力").await;
    assert_eq!(got.route.origin_key.as_deref(), Some(KEY));
}

#[tokio::test(flavor = "current_thread")]
async fn relay_preserves_order_of_100_messages() {
    let (up, mut up_rx) = upstream();
    let tx = spawn_origin_relay(KEY.to_string(), up.clone(), CancellationToken::new());

    for i in 0..100 {
        tx.send(msg(&format!("m{i}"))).unwrap();
    }

    for i in 0..100 {
        let got = recv_within(&mut up_rx, &format!("{i} 件目")).await;
        assert_eq!(got.id, format!("m{i}"), "順序が入れ替わった");
        assert_eq!(got.route.origin_key.as_deref(), Some(KEY));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn relay_stops_on_cancel() {
    let (up, mut up_rx) = upstream();
    let cancel = CancellationToken::new();
    // up はここで move する。relay が止まれば upstream の送り手が 0 になり、up_rx は Closed になる。
    let tx = spawn_origin_relay(KEY.to_string(), up, cancel.clone());

    cancel.cancel();

    let r = tokio::time::timeout(std::time::Duration::from_secs(2), up_rx.recv()).await.expect("止まらない");
    assert!(matches!(r, Err(broadcast::error::RecvError::Closed)), "cancel しても relay が upstream を握っている: {r:?}");
    wait_until_no_receivers(&tx).await;
}

#[tokio::test(flavor = "current_thread")]
async fn relay_stops_when_all_senders_dropped() {
    let (up, mut up_rx) = upstream();
    let tx = spawn_origin_relay(KEY.to_string(), up, CancellationToken::new());
    let tx2 = tx.clone();

    drop(tx);
    // 複製が 1 つ残っている間は止まらない。
    tx2.send(msg("still")).unwrap();
    assert_eq!(recv_within(&mut up_rx, "複製が残っている間").await.id, "still");

    drop(tx2);

    let r = tokio::time::timeout(std::time::Duration::from_secs(2), up_rx.recv()).await.expect("止まらない");
    assert!(matches!(r, Err(broadcast::error::RecvError::Closed)), "送り手が全部 drop されたのに relay が残っている: {r:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn message_sent_immediately_after_spawn_is_not_lost() {
    let (up, mut up_rx) = upstream();
    let tx = spawn_origin_relay(KEY.to_string(), up.clone(), CancellationToken::new());

    // spawn したタスクにはまだ制御が移っていない。購読が戻る前に済んでいなければ、ここで受信者 0 になる。
    tx.send(msg("first")).expect("戻った直後でも受信者がいる(購読済み)");

    let got = recv_within(&mut up_rx, "戻った直後に送った分").await;
    assert_eq!(got.id, "first");
}

#[test]
fn route_is_not_serialized() {
    let mut m = msg("a");
    m.route.origin_key = Some("twitch:SECRET_ROUTE_KEY".to_string());
    m.route.channel_tts_voice = Some(ChannelTtsVoice { web_speech_voice: Some("SECRET_VOICE".into()), ..Default::default() });

    let value = serde_json::to_value(&m).unwrap();
    let obj = value.as_object().expect("オブジェクト");
    assert!(!obj.contains_key("route"), "route が出ている");
    assert!(!obj.contains_key("originKey"), "originKey が出ている");

    let text = serde_json::to_string(&m).unwrap();
    for leak in ["route", "originKey", "origin_key", "channelTtsVoice", "SECRET_ROUTE_KEY", "SECRET_VOICE"] {
        assert!(!text.contains(leak), "{leak} がシリアライズに含まれている: {text}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn relay_logs_never_contain_origin_key() {
    let (logs, _guard) = capture_logs();
    let key = "twitch:SECRET_ORIGIN_KEY_MARK";
    let (up, mut up_rx) = upstream();
    let cancel = CancellationToken::new();
    let tx = spawn_origin_relay(key.to_string(), up.clone(), cancel.clone());

    // relay が走る前に容量(4096)を超えて送り、Lagged を起こす(current_thread なので送信中は relay が動かない)。
    for i in 0..5000 {
        let _ = tx.send(msg(&format!("bulk{i}")));
    }
    // Lagged の後も読み続けること。
    tokio::task::yield_now().await;
    tx.send(msg("after-lag")).unwrap();
    loop {
        let got = recv_within(&mut up_rx, "Lagged の後の 1 件").await;
        if got.id == "after-lag" {
            assert_eq!(got.route.origin_key.as_deref(), Some(key));
            break;
        }
    }

    cancel.cancel();
    drop(tx);
    drop(up);
    drop(up_rx);
    tokio::task::yield_now().await;

    let text = logs.text();
    assert!(!text.contains("SECRET_ORIGIN_KEY_MARK"), "ログに origin_key が出ている: {text}");
    assert!(!text.contains(key), "ログに origin_key が出ている: {text}");
}

async fn wait_until_no_receivers(tx: &broadcast::Sender<ChatMessage>) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while tx.receiver_count() > 0 {
        assert!(std::time::Instant::now() < end, "relay が返した Sender の購読を手放さない");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}
