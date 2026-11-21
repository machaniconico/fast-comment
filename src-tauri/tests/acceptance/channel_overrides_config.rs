// registered in main.rs by story OV-001
//! 枠ごとの上書き 受け入れテスト 3.1: 永続形式・正規化・channel_key・行の編集(`set_channel_overrides_in` / `replace_channel_in`)。
#![allow(dead_code)]

use crate::channel_overrides_common::*;
use fast_comment_lib::config::{
    channel_key, replace_channel_in, set_channel_overrides_in, AppConfig, ChannelEditError, ChannelOverrides,
    ChannelTtsVoice,
};
use serde_json::{json, Value};

fn parse(v: Value) -> AppConfig {
    serde_json::from_value(v).expect("設定として読める")
}

fn all_platform_channels() -> Value {
    json!([
        {"platform": "twitch", "identifier": "alice", "enabled": true},
        {"platform": "youtube", "identifier": "vid123", "enabled": true},
        {"platform": "x", "identifier": "1AbCd", "enabled": false},
        {"platform": "niconico", "identifier": "lv1", "enabled": true},
        {"platform": "twitcasting", "identifier": "bob", "enabled": true, "ownStream": true}
    ])
}

fn write_raw(dir: &std::path::Path, text: &str) {
    std::fs::write(AppConfig::path_in(dir), text).unwrap();
}

#[test]
fn old_config_without_overrides_loads_with_empty_overrides() {
    let cfg = parse(json!({"channels": all_platform_channels()}));
    assert_eq!(cfg.channels.len(), 5);
    for (i, ch) in cfg.channels.iter().enumerate() {
        assert!(ch.overrides.is_empty(), "{i} 行目の overrides が空でない: {:?}", ch.overrides);
        assert_eq!(ch.overrides, ChannelOverrides::default());
    }
}

#[test]
fn empty_overrides_are_omitted_from_saved_json() {
    let mut channels = all_platform_channels();
    // 明示的な空の overrides を持つ行も、保存では消える。
    channels[0]["overrides"] = json!({});
    channels[1]["overrides"] = json!({"ttsVoice": {}});
    let cfg = parse(json!({"channels": channels}));
    let dir = tempfile::tempdir().unwrap();
    cfg.save(dir.path()).unwrap();

    let disk = disk_json(dir.path());
    let rows = disk["channels"].as_array().expect("channels は配列");
    assert_eq!(rows.len(), 5);
    for (i, row) in rows.iter().enumerate() {
        assert!(row.get("overrides").is_none(), "{i} 行目に overrides キーが残っている: {row}");
    }
}

#[test]
fn config_without_overrides_roundtrips_byte_identical() {
    // 旧形式の保存結果(overrides キー無し)を作る。
    let dir = tempfile::tempdir().unwrap();
    parse(json!({"channels": all_platform_channels()})).save(dir.path()).unwrap();
    let before = std::fs::read(AppConfig::path_in(dir.path())).unwrap();
    assert!(!String::from_utf8_lossy(&before).contains("overrides"), "前提: 旧形式に overrides は無い");

    let loaded = AppConfig::load(dir.path()).unwrap();
    loaded.save(dir.path()).unwrap();

    assert_eq!(std::fs::read(AppConfig::path_in(dir.path())).unwrap(), before, "load → save でバイトが変わった");
}

#[test]
fn overrides_roundtrip_camel_case() {
    let input = json!({"channels": [{
        "platform": "twitch", "identifier": "alice", "enabled": true,
        "overrides": {
            "ttsEnabled": false,
            "ttsVoice": {"voicevoxSpeaker": 3, "bouyomiVoice": -5, "webSpeechVoice": "Haruka"}
        }
    }]});
    let cfg = parse(input);
    let dir = tempfile::tempdir().unwrap();
    cfg.save(dir.path()).unwrap();

    let disk = disk_json(dir.path());
    assert_eq!(
        disk["channels"][0]["overrides"],
        json!({"ttsEnabled": false, "ttsVoice": {"voicevoxSpeaker": 3, "bouyomiVoice": -5, "webSpeechVoice": "Haruka"}})
    );
    let back = AppConfig::load(dir.path()).unwrap();
    assert_eq!(back.channels[0].overrides.tts_enabled, Some(false));
    assert_eq!(
        back.channels[0].overrides.tts_voice,
        Some(ChannelTtsVoice {
            voicevox_speaker: Some(3),
            bouyomi_voice: Some(-5),
            web_speech_voice: Some("Haruka".to_string()),
        })
    );
}

#[test]
fn unknown_keys_inside_overrides_are_ignored() {
    let cfg = parse(json!({"channels": [{
        "platform": "twitch", "identifier": "alice", "enabled": true,
        "overrides": {
            "ttsEnabled": false, "futureFlag": {"a": 1},
            "ttsVoice": {"voicevoxSpeaker": 4, "futureKnob": true}
        }
    }]}));
    let ov = &cfg.channels[0].overrides;
    assert_eq!(ov.tts_enabled, Some(false));
    assert_eq!(ov.tts_voice.as_ref().and_then(|v| v.voicevox_speaker), Some(4));
}

#[test]
fn non_object_overrides_value_fails_load_with_path() {
    for bad in ["5", "\"MARKER_VALUE\"", "true"] {
        let dir = tempfile::tempdir().unwrap();
        write_raw(
            dir.path(),
            &format!(r#"{{"channels":[{{"platform":"twitch","identifier":"alice","overrides":{bad}}}]}}"#),
        );
        let err = AppConfig::load(dir.path()).expect_err(&format!("overrides={bad} は読み込めてはいけない"));
        let text = err.to_string();
        assert!(text.contains("channels"), "項目の位置(channels)が無い: {text}");
        assert!(!text.contains("MARKER_VALUE"), "入力値がエラー文に出ている: {text}");
    }
}

#[test]
fn normalize_turns_tts_enabled_true_into_none() {
    let mut ov = ChannelOverrides { tts_enabled: Some(true), tts_voice: None };
    ov.normalize();
    assert_eq!(ov.tts_enabled, None);
    assert!(ov.is_empty());

    let mut off_ov = off();
    off_ov.normalize();
    assert_eq!(off_ov.tts_enabled, Some(false), "false は残る(狭める方向)");
}

#[test]
fn normalize_trims_web_speech_voice_and_drops_empty() {
    let cases: [(&str, Option<&str>); 4] =
        [("  Haruka \t", Some("Haruka")), ("Haruka", Some("Haruka")), ("   ", None), ("", None)];
    for (input, expected) in cases {
        // 他の項目を 1 つ持たせて、声が丸ごと消える効果と区別する。
        let mut ov = voice_ov(Some(1), None, Some(input));
        ov.normalize();
        let voice = ov.tts_voice.expect("voicevox_speaker が残るので tts_voice は残る");
        assert_eq!(voice.web_speech_voice.as_deref(), expected, "input={input:?}");
        assert_eq!(voice.voicevox_speaker, Some(1));
    }
}

#[test]
fn normalize_collapses_all_none_voice_to_none() {
    for mut ov in [
        ChannelOverrides { tts_enabled: None, tts_voice: Some(ChannelTtsVoice::default()) },
        voice_ov(None, None, Some("   ")),
    ] {
        ov.normalize();
        assert_eq!(ov.tts_voice, None);
        assert!(ov.is_empty());
    }

    // 0 は有効な値(声が全部 None ではない)。
    let mut zero = voice_ov(Some(0), Some(0), None);
    zero.normalize();
    assert!(zero.tts_voice.is_some(), "Some(0) は残る");
}

#[test]
fn channel_key_matches_platform_colon_identifier_for_all_platforms() {
    let cases = [
        ("twitch", "alice", "twitch:alice"),
        ("youtube", "https://www.youtube.com/watch?v=abc", "youtube:https://www.youtube.com/watch?v=abc"),
        ("x", "1AbCd", "x:1AbCd"),
        ("niconico", "lv123", "niconico:lv123"),
        ("twitcasting", "bob_1", "twitcasting:bob_1"),
    ];
    for (platform, ident, expected) in cases {
        assert_eq!(channel_key(&chan(platform, ident)), expected);
    }
}

#[test]
fn channel_overrides_returns_none_for_unknown_key() {
    let cfg = cfg_with(vec![chan_with("twitch", "alice", json!({"ttsEnabled": false}))]);
    assert_eq!(cfg.channel_overrides("twitch:nobody"), None);
    assert_eq!(cfg.channel_overrides(""), None);
    assert_eq!(cfg.channel_overrides("twitch:alice"), Some(&off()), "存在する行は Some");
}

#[test]
fn set_channel_overrides_in_replaces_only_target_row() {
    let mut cfg = cfg_with(vec![
        chan_with("twitch", "alice", json!({"ttsEnabled": false})),
        chan_with("youtube", "vid", json!({"ttsEnabled": false, "ttsVoice": {"voicevoxSpeaker": 2}})),
        chan("x", "zzz"),
    ]);
    let before = cfg.clone();

    // 対象行は丸ごと置き換わる(古い ttsEnabled=false は引き継がれない)。未正規化の入力は正規化されて入る。
    let new = ChannelOverrides {
        tts_enabled: Some(true),
        tts_voice: Some(ChannelTtsVoice { web_speech_voice: Some(" Kyoko ".into()), ..Default::default() }),
    };
    set_channel_overrides_in(&mut cfg, "youtube:vid", new).expect("既存キーは置き換えられる");

    let mut expected = before.clone();
    expected.channels[1].overrides = voice_ov(None, None, Some("Kyoko"));
    assert_eq!(cfg, expected, "対象行の overrides 以外は何も変わらない");
}

#[test]
fn set_channel_overrides_in_unknown_key_is_not_found() {
    let mut cfg = cfg_with(vec![chan("twitch", "alice")]);
    let before = cfg.clone();
    let r = set_channel_overrides_in(&mut cfg, "twitch:nobody", off());
    assert_eq!(r, Err(ChannelEditError::NotFound));
    assert_eq!(cfg, before);
}

#[test]
fn channel_edit_error_display_has_no_identifier() {
    let secret = "SECRET_ID_123";
    let mut cfg = cfg_with(vec![chan("twitch", "alice"), chan("twitch", "bob")]);

    let not_found = set_channel_overrides_in(&mut cfg, &format!("twitch:{secret}"), off()).unwrap_err();
    let dup = replace_channel_in(&mut cfg, "twitch:alice", chan("twitch", "bob")).unwrap_err();
    assert_eq!(not_found, ChannelEditError::NotFound);
    assert_eq!(dup, ChannelEditError::Duplicate);

    for err in [not_found, dup] {
        let shown = err.to_string();
        assert!(!shown.trim().is_empty(), "Display が空");
        for leak in [secret, "alice", "bob", "twitch:"] {
            assert!(!shown.contains(leak), "Display に {leak:?} が含まれている: {shown}");
        }
    }
}

#[test]
fn replace_channel_in_inherits_overrides_when_new_is_empty() {
    let old_ov = json!({"ttsEnabled": false, "ttsVoice": {"voicevoxSpeaker": 8}});
    let mut cfg = cfg_with(vec![chan_with("twitch", "old", old_ov)]);
    let inherited = cfg.channels[0].overrides.clone();

    replace_channel_in(&mut cfg, "twitch:old", chan("twitch", "new")).expect("置き換えられる");

    assert_eq!(cfg.channels.len(), 1);
    assert_eq!(channel_key(&cfg.channels[0]), "twitch:new");
    assert_eq!(cfg.channels[0].overrides, inherited);
    assert!(!inherited.is_empty(), "前提: 引き継ぐ値は空でない");
}

#[test]
fn replace_channel_in_keeps_new_overrides_when_given() {
    let mut cfg = cfg_with(vec![chan_with("twitch", "old", json!({"ttsEnabled": false}))]);
    let new = chan_with("youtube", "vid", json!({"ttsVoice": {"bouyomiVoice": 2}}));
    let new_ov = new.overrides.clone();

    replace_channel_in(&mut cfg, "twitch:old", new).unwrap();

    assert_eq!(cfg.channels[0].overrides, new_ov);
    assert_eq!(cfg.channels[0].overrides.tts_enabled, None, "古い ttsEnabled は混ざらない");
}

#[test]
fn replace_channel_in_keeps_position_and_other_rows() {
    let mut old_mid = chan_with("youtube", "mid", json!({"ttsEnabled": false}));
    old_mid.own_stream = true;
    let mut cfg = cfg_with(vec![chan("twitch", "first"), old_mid, chan_with("x", "last", json!({"ttsEnabled": false}))]);
    let before = cfg.clone();

    replace_channel_in(&mut cfg, "youtube:mid", chan("niconico", "lv9")).unwrap();

    assert_eq!(cfg.channels.len(), 3);
    assert_eq!(cfg.channels[0], before.channels[0]);
    assert_eq!(cfg.channels[2], before.channels[2]);
    assert_eq!(channel_key(&cfg.channels[1]), "niconico:lv9", "同じ位置に入る");
    assert!(!cfg.channels[1].own_stream, "own_stream は引き継がない(決定 D4)");
    let mut expected = before.clone();
    expected.channels[1] = cfg.channels[1].clone();
    assert_eq!(cfg, expected, "チャンネル以外のフィールドも変わらない");
}

#[test]
fn replace_channel_in_rejects_duplicate_key() {
    let mut cfg = cfg_with(vec![chan("twitch", "a"), chan("twitch", "b"), chan("twitch", "c")]);
    let before = cfg.clone();

    let r = replace_channel_in(&mut cfg, "twitch:a", chan("twitch", "c"));
    assert_eq!(r, Err(ChannelEditError::Duplicate));
    assert_eq!(cfg, before, "失敗したら何も変わらない");

    // 自分自身と同じキーへの置き換えは重複ではない。
    let mut same = chan("twitch", "b");
    same.enabled = false;
    replace_channel_in(&mut cfg, "twitch:b", same).expect("old_key と同じキーは重複ではない");
    assert!(!cfg.channels[1].enabled);
}

#[test]
fn replace_channel_in_unknown_old_key_is_not_found() {
    // 契約 §1.1 にあるが §3.1 に名前が無い。実装者の取りこぼし防止に追加。
    let mut cfg = cfg_with(vec![chan("twitch", "a")]);
    let before = cfg.clone();
    assert_eq!(replace_channel_in(&mut cfg, "twitch:zz", chan("twitch", "n")), Err(ChannelEditError::NotFound));
    assert_eq!(cfg, before);
}

#[test]
fn update_config_path_normalizes_channel_overrides() {
    // `from_config_update` は pub(crate) で統合テストから呼べない。同じ入口の serde 読み込み → normalize_all で確かめる。
    let mut cfg = parse(json!({"channels": [{
        "platform": "twitch", "identifier": "alice", "enabled": true,
        "overrides": {"ttsEnabled": true, "ttsVoice": {"webSpeechVoice": "  Haruka  "}}
    }, {
        "platform": "youtube", "identifier": "v", "enabled": true,
        "overrides": {"ttsVoice": {"webSpeechVoice": "   "}}
    }]}));
    cfg.normalize_all();

    assert_eq!(cfg.channels[0].overrides.tts_enabled, None);
    assert_eq!(
        cfg.channels[0].overrides.tts_voice.as_ref().and_then(|v| v.web_speech_voice.as_deref()),
        Some("Haruka")
    );
    assert!(cfg.channels[1].overrides.is_empty(), "空白だけの声は行ごと空になる");
}
