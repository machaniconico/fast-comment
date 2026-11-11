//! 音声パラメータの解決と自動加速(純関数)。

use crate::config::{TtsAutoSpeed, VoicevoxParams};
use crate::listeners::TtsVoiceOverride;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedVoiceParams {
    pub speed: Option<f32>,
    pub speed_factor: f32,
    pub max_speed: f32,
    pub pitch: Option<f32>,
    pub intonation: Option<f32>,
    pub volume: Option<f32>,
    pub pre: Option<f32>,
    pub post: Option<f32>,
}

/// `pending` は取り出し後の残件数、`chars` は整形後の文字数。
/// 上限はエンジンの基準速度が判明してから apply 側で適用する。
pub fn auto_speed_factor(cfg: &TtsAutoSpeed, pending: usize, chars: usize) -> f32 {
    if !cfg.enabled {
        return 1.0;
    }
    let queued = pending.saturating_sub(cfg.queue_threshold as usize) as f32;
    let extra_chars = if cfg.length_threshold == 0 {
        0.0
    } else {
        chars.saturating_sub(cfg.length_threshold as usize) as f32
    };
    1.0 + cfg.per_queued.max(0.0) * queued + cfg.per_ten_chars.max(0.0) * (extra_chars / 10.0)
}

pub fn resolve_voice_params(
    opt: &VoicevoxParams,
    listener: Option<&TtsVoiceOverride>,
    auto: &TtsAutoSpeed,
    factor: f32,
) -> ResolvedVoiceParams {
    ResolvedVoiceParams {
        speed: clamped(listener.and_then(|l| l.rate).or(opt.speed_scale), 0.5, 2.0),
        speed_factor: if auto.enabled { factor.max(1.0) } else { 1.0 },
        max_speed: if auto.enabled { auto.max_speed } else { 2.0 },
        pitch: clamped(
            listener
                .and_then(|l| l.pitch)
                .map(|p| (p - 1.0) * 0.15)
                .or(opt.pitch_scale),
            -0.15,
            0.15,
        ),
        intonation: clamped(opt.intonation_scale, 0.0, 2.0),
        volume: clamped(opt.volume_scale, 0.0, 2.0),
        pre: clamped(opt.pre_phoneme_length, 0.0, 1.5),
        post: clamped(opt.post_phoneme_length, 0.0, 1.5),
    }
}

fn clamped(value: Option<f32>, min: f32, max: f32) -> Option<f32> {
    value.filter(|v| v.is_finite()).map(|v| v.clamp(min, max))
}

pub fn apply_to_audio_query(query: &mut Value, p: &ResolvedVoiceParams) {
    let Some(object) = query.as_object_mut() else {
        return;
    };
    // 未設定かつ加速なしならエンジン値をそのまま残す。
    if p.speed.is_some() || p.speed_factor > 1.0 {
        let base = p
            .speed
            .or_else(|| {
                object
                    .get("speedScale")
                    .and_then(Value::as_f64)
                    .map(|v| v as f32)
            })
            .unwrap_or(1.0);
        let speed = (base * p.speed_factor)
            .min(p.max_speed.max(base))
            .clamp(0.5, 2.0);
        object.insert("speedScale".into(), Value::from(speed));
    }
    for (key, value) in [
        ("pitchScale", p.pitch),
        ("intonationScale", p.intonation),
        ("volumeScale", p.volume),
        ("prePhonemeLength", p.pre),
        ("postPhonemeLength", p.post),
    ] {
        if let Some(value) = value {
            object.insert(key.into(), Value::from(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn approx(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
    }

    fn apply(opt: &VoicevoxParams, auto: &TtsAutoSpeed, factor: f32, query: &mut Value) {
        apply_to_audio_query(query, &resolve_voice_params(opt, None, auto, factor));
    }

    #[test]
    fn listener_rate_and_pitch_map_to_voicevox_scales() {
        for (pitch, expected) in [(0.0, -0.15), (1.0, 0.0), (2.0, 0.15)] {
            let listener = TtsVoiceOverride {
                rate: Some(1.5),
                pitch: Some(pitch),
                ..Default::default()
            };
            let p = resolve_voice_params(
                &VoicevoxParams::default(),
                Some(&listener),
                &TtsAutoSpeed::default(),
                1.0,
            );
            assert_eq!(p.speed, Some(1.5));
            approx(p.pitch.unwrap() as f64, expected);
        }
    }

    #[test]
    fn listener_overrides_global_params() {
        let opt = VoicevoxParams {
            speed_scale: Some(0.8),
            pitch_scale: Some(-0.1),
            volume_scale: Some(0.7),
            ..Default::default()
        };
        let listener = TtsVoiceOverride {
            rate: Some(1.5),
            pitch: Some(1.5),
            ..Default::default()
        };
        let p = resolve_voice_params(&opt, Some(&listener), &TtsAutoSpeed::default(), 1.0);
        assert_eq!(p.speed, Some(1.5));
        approx(p.pitch.unwrap() as f64, 0.075);
        assert_eq!(p.volume, Some(0.7));
        let partial = TtsVoiceOverride {
            rate: Some(1.2),
            ..Default::default()
        };
        assert_eq!(
            resolve_voice_params(&opt, Some(&partial), &TtsAutoSpeed::default(), 1.0).pitch,
            opt.pitch_scale
        );
    }

    #[test]
    fn unset_params_leave_audio_query_untouched() {
        let mut query = json!({"speedScale": 1.123456789, "pitchScale": 0.03, "accent_phrases": [{"moras": []}], "custom": true});
        let original = query.clone();
        apply(
            &VoicevoxParams::default(),
            &TtsAutoSpeed::default(),
            1.0,
            &mut query,
        );
        assert_eq!(query, original);
        let mut empty = json!({});
        apply(
            &VoicevoxParams::default(),
            &TtsAutoSpeed::default(),
            1.0,
            &mut empty,
        );
        assert_eq!(empty, json!({}));
    }

    #[test]
    fn apply_overwrites_only_set_keys_and_preserves_others() {
        let mut query = json!({"speedScale": 0.9, "pitchScale": 0.03, "volumeScale": 1.7, "accent_phrases": [{"moras": []}], "custom": 42});
        let opt = VoicevoxParams {
            speed_scale: Some(1.2),
            intonation_scale: Some(0.8),
            pre_phoneme_length: Some(0.2),
            post_phoneme_length: Some(0.4),
            ..Default::default()
        };
        apply(&opt, &TtsAutoSpeed::default(), 1.0, &mut query);
        approx(query["speedScale"].as_f64().unwrap(), 1.2);
        approx(query["intonationScale"].as_f64().unwrap(), 0.8);
        approx(query["prePhonemeLength"].as_f64().unwrap(), 0.2);
        approx(query["postPhonemeLength"].as_f64().unwrap(), 0.4);
        assert_eq!(query["pitchScale"], json!(0.03));
        assert_eq!(query["volumeScale"], json!(1.7));
        assert_eq!(query["accent_phrases"], json!([{"moras": []}]));
        assert_eq!(query["custom"], json!(42));
    }

    #[test]
    fn apply_is_noop_for_non_object_query() {
        let p = resolve_voice_params(
            &VoicevoxParams {
                speed_scale: Some(1.5),
                ..Default::default()
            },
            None,
            &TtsAutoSpeed::default(),
            1.0,
        );
        for mut query in [
            Value::Null,
            json!([]),
            json!("query"),
            json!(42),
            json!(true),
        ] {
            let original = query.clone();
            apply_to_audio_query(&mut query, &p);
            assert_eq!(query, original);
        }
    }

    #[test]
    fn auto_speed_disabled_is_identity() {
        let auto = TtsAutoSpeed::default();
        assert_eq!(auto_speed_factor(&auto, usize::MAX, usize::MAX), 1.0);
        let mut query = json!({"speedScale": 1.8});
        apply(&VoicevoxParams::default(), &auto, 3.0, &mut query);
        assert_eq!(query, json!({"speedScale": 1.8}));
    }

    #[test]
    fn auto_speed_grows_with_pending_and_length_and_caps_at_max_speed() {
        let mut auto = TtsAutoSpeed {
            enabled: true,
            ..Default::default()
        };
        approx(auto_speed_factor(&auto, 2, 40) as f64, 1.0);
        approx(auto_speed_factor(&auto, 4, 45) as f64, 1.225);
        approx(auto_speed_factor(&auto, 5, 60) as f64, 1.4);
        let mut query = json!({"speedScale": 1.2});
        apply(
            &VoicevoxParams::default(),
            &auto,
            auto_speed_factor(&auto, 20, 200),
            &mut query,
        );
        approx(query["speedScale"].as_f64().unwrap(), 1.6);
        auto.length_threshold = 0;
        assert_eq!(auto_speed_factor(&auto, 2, 1000), 1.0);
    }

    #[test]
    fn auto_speed_never_slows_below_base() {
        let auto = TtsAutoSpeed {
            enabled: true,
            max_speed: 0.7,
            ..Default::default()
        };
        for base in [0.5, 1.2, 1.8] {
            for factor in [0.5, 1.0, 4.0] {
                let opt = VoicevoxParams {
                    speed_scale: Some(base),
                    ..Default::default()
                };
                let mut query = json!({"speedScale": 0.6});
                apply(&opt, &auto, factor, &mut query);
                assert!(query["speedScale"].as_f64().unwrap() >= base as f64);
            }
        }
        assert_eq!(auto_speed_factor(&auto, 0, 0), 1.0);
    }

    #[test]
    fn final_speed_is_clamped_to_engine_range() {
        let auto = TtsAutoSpeed {
            enabled: true,
            max_speed: 10.0,
            ..Default::default()
        };
        for (base, factor, expected) in [(0.1, 1.1, 0.5), (1.5, 10.0, 2.0), (4.0, 1.1, 2.0)] {
            let mut query = json!({"speedScale": base});
            apply(&VoicevoxParams::default(), &auto, factor, &mut query);
            approx(query["speedScale"].as_f64().unwrap(), expected);
        }
        let opt = VoicevoxParams {
            speed_scale: Some(0.1),
            pitch_scale: Some(1.0),
            intonation_scale: Some(3.0),
            volume_scale: Some(-1.0),
            pre_phoneme_length: Some(3.0),
            post_phoneme_length: Some(-1.0),
        };
        let p = resolve_voice_params(&opt, None, &auto, 1.0);
        assert_eq!(
            (p.speed, p.pitch, p.intonation, p.volume, p.pre, p.post),
            (
                Some(0.5),
                Some(0.15),
                Some(2.0),
                Some(0.0),
                Some(1.5),
                Some(0.0)
            )
        );
    }
}
