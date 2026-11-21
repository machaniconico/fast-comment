// registered in main.rs by story OV-003 (also needs OV-002: MessageRoute and common.rs msg() carrying the `route` line)
//! 枠ごとの上書き 受け入れテスト 3.4: 枠の読み上げ ON/OFF と声の解決(`channel_allows_tts` / `stamp_channel_voice` / `effective_voice`)。
#![allow(dead_code)]

use crate::channel_overrides_common::*;
use crate::common::msg;
use fast_comment_lib::config::{ChannelTtsVoice, TtsBackendKind};
use fast_comment_lib::listeners::TtsVoiceOverride;
use fast_comment_lib::model::{ChatMessage, ListenerInfo};
use fast_comment_lib::tts::{channel_allows_tts, effective_voice, should_speak, stamp_channel_voice};
use serde_json::json;

const KEY: &str = "twitch:chan";

fn from(key: Option<&str>) -> ChatMessage {
    let mut m = msg("a");
    m.route.origin_key = key.map(str::to_owned);
    m
}

fn listener(muted: bool, voice: Option<TtsVoiceOverride>) -> ListenerInfo {
    ListenerInfo {
        comment_count: 1,
        gift_count: 0,
        first_seen_ms: 0,
        prev_seen_ms: None,
        is_first_ever: true,
        label: None,
        tts_muted: muted,
        tts_voice: voice,
    }
}

fn channel_voice(vv: Option<u32>, bouyomi: Option<i16>, web: Option<&str>) -> ChannelTtsVoice {
    ChannelTtsVoice { voicevox_speaker: vv, bouyomi_voice: bouyomi, web_speech_voice: web.map(str::to_owned) }
}

#[test]
fn channel_disabled_blocks_tts() {
    let cfg = cfg_with(vec![chan_with("twitch", "chan", json!({"ttsEnabled": false})), chan("twitch", "other")]);
    assert!(!channel_allows_tts(&cfg, &from(Some(KEY))));
    assert!(channel_allows_tts(&cfg, &from(Some("twitch:other"))), "上書きの無い別の行は読む");
}

#[test]
fn missing_origin_key_allows_tts() {
    let cfg = cfg_with(vec![chan_with("twitch", "chan", json!({"ttsEnabled": false}))]);
    // 手動・注入・テストのコメントには origin_key が無い。msg() の channel が "chan" でも見ない。
    assert!(channel_allows_tts(&cfg, &from(None)));
}

#[test]
fn unknown_origin_key_allows_tts() {
    let cfg = cfg_with(vec![chan_with("twitch", "chan", json!({"ttsEnabled": false}))]);
    assert!(channel_allows_tts(&cfg, &from(Some("twitch:removed-row"))));
}

#[test]
fn channel_allows_tts_ignores_global_backend() {
    for backend in [TtsBackendKind::Bouyomi, TtsBackendKind::Voicevox, TtsBackendKind::WebSpeech, TtsBackendKind::None] {
        let mut cfg = cfg_with(vec![
            chan_with("twitch", "chan", json!({"ttsVoice": {"voicevoxSpeaker": 1}})),
            chan_with("twitch", "off", json!({"ttsEnabled": false})),
        ]);
        cfg.tts.backend = backend;
        assert!(channel_allows_tts(&cfg, &from(Some(KEY))), "backend={backend:?} で枠が許可なのに false");
        assert!(!channel_allows_tts(&cfg, &from(Some("twitch:off"))), "backend={backend:?} で枠が禁止なのに true");
    }
}

#[test]
fn stamp_copies_row_voice_into_route() {
    let cfg = cfg_with(vec![chan_with(
        "twitch",
        "chan",
        json!({"ttsVoice": {"voicevoxSpeaker": 7, "bouyomiVoice": -3, "webSpeechVoice": "Kyoko"}}),
    )]);
    let mut m = from(Some(KEY));
    assert_eq!(m.route.channel_tts_voice, None, "前提");

    stamp_channel_voice(&cfg, &mut m);

    assert_eq!(m.route.channel_tts_voice, Some(channel_voice(Some(7), Some(-3), Some("Kyoko"))));
    assert_eq!(m.route.origin_key.as_deref(), Some(KEY), "origin_key は変わらない");
}

#[test]
fn stamp_clears_voice_for_unknown_row() {
    let cfg = cfg_with(vec![chan_with("twitch", "chan", json!({"ttsVoice": {"voicevoxSpeaker": 7}}))]);
    for key in [Some("twitch:removed-row"), None] {
        let mut m = from(key);
        m.route.channel_tts_voice = Some(channel_voice(Some(1), None, None));
        stamp_channel_voice(&cfg, &mut m);
        assert_eq!(m.route.channel_tts_voice, None, "origin_key={key:?}");
    }
}

#[test]
fn effective_voice_listener_wins_over_channel_per_field() {
    let channel = channel_voice(Some(1), Some(2), Some("Chan"));
    let l = TtsVoiceOverride {
        voicevox_speaker: Some(10),
        bouyomi_voice: Some(20),
        web_speech_voice: Some("Listener".into()),
        ..Default::default()
    };
    let got = effective_voice(Some(&channel), Some(&l)).expect("声がある");
    assert_eq!(got.voicevox_speaker, Some(10));
    assert_eq!(got.bouyomi_voice, Some(20));
    assert_eq!(got.web_speech_voice.as_deref(), Some("Listener"));
}

#[test]
fn effective_voice_falls_back_field_by_field_to_channel() {
    let channel = channel_voice(Some(1), Some(2), Some("Chan"));
    let l = TtsVoiceOverride { voicevox_speaker: Some(10), ..Default::default() };
    let got = effective_voice(Some(&channel), Some(&l)).expect("声がある");
    assert_eq!(got.voicevox_speaker, Some(10), "listener にある項目は listener");
    assert_eq!(got.bouyomi_voice, Some(2), "listener に無い項目は channel(i16 → i32)");
    assert_eq!(got.web_speech_voice.as_deref(), Some("Chan"));

    // listener が無ければ channel だけ。
    let only_channel = effective_voice(Some(&channel), None).expect("声がある");
    assert_eq!(only_channel.voicevox_speaker, Some(1));
    assert_eq!(only_channel.bouyomi_voice, Some(2));
    assert_eq!(only_channel.web_speech_voice.as_deref(), Some("Chan"));
}

#[test]
fn effective_voice_rate_and_pitch_only_from_listener() {
    let channel = channel_voice(Some(1), None, None);

    let l = TtsVoiceOverride { rate: Some(1.5), pitch: Some(1.25), ..Default::default() };
    let with_listener = effective_voice(Some(&channel), Some(&l)).expect("声がある");
    assert_eq!(with_listener.rate, Some(1.5));
    assert_eq!(with_listener.pitch, Some(1.25));
    assert_eq!(with_listener.voicevox_speaker, Some(1), "声の項目は channel から補われる");

    let without = effective_voice(Some(&channel), None).expect("声がある");
    assert_eq!(without.rate, None, "channel に rate は無い");
    assert_eq!(without.pitch, None);
}

#[test]
fn effective_voice_none_when_both_absent() {
    assert_eq!(effective_voice(None, None), None);
    // 中身が全部 None なら「なし」と同じ(全体の声を使う)。
    let empty_channel = ChannelTtsVoice::default();
    let empty_listener = TtsVoiceOverride::default();
    assert_eq!(effective_voice(Some(&empty_channel), None), None);
    assert_eq!(effective_voice(None, Some(&empty_listener)), None);
    assert_eq!(effective_voice(Some(&empty_channel), Some(&empty_listener)), None);
}

#[test]
fn should_speak_order_unchanged_listener_mute_still_wins() {
    let cfg = cfg_with(vec![chan_with(
        "twitch",
        "chan",
        json!({"ttsVoice": {"voicevoxSpeaker": 7, "webSpeechVoice": "Kyoko"}}),
    )]);
    let opt = &cfg.tts.options;

    let mut unmuted = from(Some(KEY));
    unmuted.listener = Some(listener(false, None));
    stamp_channel_voice(&cfg, &mut unmuted);
    assert!(channel_allows_tts(&cfg, &unmuted));
    assert!(should_speak(&unmuted, opt), "前提: 枠が許可でミュートなしなら読む");

    let mut muted = unmuted.clone();
    muted.listener = Some(listener(true, Some(TtsVoiceOverride { voicevox_speaker: Some(1), ..Default::default() })));
    assert!(channel_allows_tts(&cfg, &muted), "枠は許可している");
    assert!(!should_speak(&muted, opt), "枠の上書きがあってもリスナーのミュートが優先する");
}
