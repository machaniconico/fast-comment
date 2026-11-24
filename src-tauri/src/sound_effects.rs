//! 種別ごとの効果音(仕様 SPEC §効果音)。
//!
//! 判定と間引きはここ(Rust)で行い、結果を `ChatMessage.sound` に載せる。UI は鳴らすだけ。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};

use crate::achievements::currency::{normalize_currency, AmountTotals, RateTable};
use crate::config::{AppConfig, ChannelOverrides};
use crate::model::{ChatMessage, MessageKind};
use crate::moderation::Verdict;

/// 内蔵プリセット(WebAudio の合成音。ファイルは同梱しない)。
pub const BUILTIN_SOUNDS: [&str; 4] = ["chime", "bell", "pop", "coin"];
pub const MAX_MIN_INTERVAL_MS: u32 = 5000;
pub const MAX_MIN_JPY: u32 = 10_000_000;
const DEFAULT_VOLUME: f32 = 0.6;
const DEFAULT_MIN_INTERVAL_MS: u32 = 300;

/// 種別ごとの規則。`min_jpy` は投げ銭系(superChat / bits / gift)だけが見る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundRule {
    pub enabled: bool,
    pub sound: String,
    pub min_jpy: u32,
}

impl SoundRule {
    fn new(enabled: bool, sound: &str) -> Self {
        Self { enabled, sound: format!("builtin:{sound}"), min_jpy: 0 }
    }

    /// 壊れた項目だけ既定値のまま残して、読めた項目を取り込む。
    fn merge(&mut self, value: Option<&serde_json::Value>) {
        let Some(obj) = value.and_then(serde_json::Value::as_object) else { return };
        if let Some(enabled) = obj.get("enabled").and_then(serde_json::Value::as_bool) {
            self.enabled = enabled;
        }
        if let Some(sound) = obj.get("sound").and_then(serde_json::Value::as_str) {
            if is_valid_sound_ref(sound) {
                self.sound = sound.to_owned();
            }
        }
        if let Some(min) = obj.get("minJpy").and_then(serde_json::Value::as_u64) {
            self.min_jpy = min.min(u64::from(MAX_MIN_JPY)) as u32;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundEffectsConfig {
    pub enabled: bool,
    pub volume: f32,
    pub min_interval_ms: u32,
    pub super_chat: SoundRule,
    pub bits: SoundRule,
    pub gift: SoundRule,
    pub membership: SoundRule,
    pub first_comment: SoundRule,
}

impl Default for SoundEffectsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            volume: DEFAULT_VOLUME,
            min_interval_ms: DEFAULT_MIN_INTERVAL_MS,
            super_chat: SoundRule::new(true, "coin"),
            bits: SoundRule::new(true, "pop"),
            gift: SoundRule::new(true, "bell"),
            membership: SoundRule::new(true, "chime"),
            first_comment: SoundRule::new(false, "pop"),
        }
    }
}

/// 読み込みは寛容: 無い・型違いの項目は既定値、範囲外は丸める。
impl<'de> Deserialize<'de> for SoundEffectsConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mut cfg = Self::default();
        if let Some(obj) = value.as_object() {
            if let Some(enabled) = obj.get("enabled").and_then(serde_json::Value::as_bool) {
                cfg.enabled = enabled;
            }
            if let Some(volume) = obj.get("volume").and_then(serde_json::Value::as_f64) {
                cfg.volume = volume as f32;
            }
            if let Some(ms) = obj.get("minIntervalMs").and_then(serde_json::Value::as_u64) {
                cfg.min_interval_ms = ms.min(u64::from(MAX_MIN_INTERVAL_MS)) as u32;
            }
            cfg.super_chat.merge(obj.get("superChat"));
            cfg.bits.merge(obj.get("bits"));
            cfg.gift.merge(obj.get("gift"));
            cfg.membership.merge(obj.get("membership"));
            cfg.first_comment.merge(obj.get("firstComment"));
        }
        cfg.normalize();
        Ok(cfg)
    }
}

impl SoundEffectsConfig {
    pub fn normalize(&mut self) {
        let defaults = Self::default();
        self.volume = if self.volume.is_nan() { defaults.volume } else { self.volume.clamp(0.0, 1.0) };
        self.min_interval_ms = self.min_interval_ms.min(MAX_MIN_INTERVAL_MS);
        for (rule, default) in [
            (&mut self.super_chat, defaults.super_chat),
            (&mut self.bits, defaults.bits),
            (&mut self.gift, defaults.gift),
            (&mut self.membership, defaults.membership),
            (&mut self.first_comment, defaults.first_comment),
        ] {
            if !is_valid_sound_ref(&rule.sound) {
                rule.sound = default.sound;
            }
            rule.min_jpy = rule.min_jpy.min(MAX_MIN_JPY);
        }
    }

    /// 保管庫に無い `custom:<id>` を使う規則を、その規則の既定の内蔵音にする。変えたら true。
    /// 削除済みの音源が設定に残っても、保存時と実行時にここで既定音へ寄せる。
    pub fn resolve_missing(&mut self, exists: impl Fn(&str) -> bool) -> bool {
        let defaults = Self::default();
        let mut changed = false;
        for (rule, default) in [
            (&mut self.super_chat, defaults.super_chat),
            (&mut self.bits, defaults.bits),
            (&mut self.gift, defaults.gift),
            (&mut self.membership, defaults.membership),
            (&mut self.first_comment, defaults.first_comment),
        ] {
            if let Some(id) = rule.sound.strip_prefix("custom:") {
                if !exists(id) {
                    rule.sound = default.sound;
                    changed = true;
                }
            }
        }
        changed
    }
}

/// 鳴らす音の指定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundCue(pub String);

/// カスタム音源の ID(UUID の小文字ハイフン形式)。パスに使う前に必ずこれを通す。
pub fn is_valid_sound_id(id: &str) -> bool {
    id.len() == 36
        && uuid::Uuid::parse_str(id).is_ok()
        && id.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'-'))
}

/// `builtin:<名前>` か `custom:<UUID>` だけを受け付ける。
pub fn is_valid_sound_ref(sound: &str) -> bool {
    match sound.split_once(':') {
        Some(("builtin", name)) => BUILTIN_SOUNDS.contains(&name),
        Some(("custom", id)) => is_valid_sound_id(id),
        _ => false,
    }
}

/// 円換算(ミリ円)。レート表に無い通貨・壊れたレートは None。
fn jpy_milli(msg: &ChatMessage, rates: &RateTable, symbols: &BTreeMap<String, String>) -> Option<i64> {
    let amount = msg.amount.as_ref()?;
    let code = normalize_currency(&amount.currency, symbols);
    if !rates.get(&code).is_some_and(|r| r.is_finite() && *r > 0.0) {
        return None;
    }
    let mut totals = AmountTotals::default();
    totals.add(&amount.currency, amount.value);
    Some(totals.jpy_milli(rates, symbols))
}

fn money_passes(
    rule: &SoundRule,
    msg: &ChatMessage,
    rates: &RateTable,
    symbols: &BTreeMap<String, String>,
) -> bool {
    if !rule.enabled {
        return false;
    }
    match jpy_milli(msg, rates, symbols) {
        Some(milli) => milli >= i64::from(rule.min_jpy) * 1000,
        // Bits はレートが無ければ最低金額の対象外。他は換算できなければ最低金額 0 の時だけ鳴らす。
        None => msg.kind == MessageKind::Bits || rule.min_jpy == 0,
    }
}

/// 1 件に鳴らす音を 1 つ選ぶ。優先順位は 投げ銭 > メンバー > 初コメ。
/// 上位の規則が OFF・金額未満なら次の候補へ落とす。
pub fn select_sound(
    msg: &ChatMessage,
    cfg: &SoundEffectsConfig,
    rates: &RateTable,
    symbol_overrides: &BTreeMap<String, String>,
    channel: Option<&ChannelOverrides>,
) -> Option<SoundCue> {
    if !cfg.enabled
        || msg.kind == MessageKind::System
        || msg.injected_test
        || msg.route.uncounted
        || channel.and_then(|o| o.sound_enabled) == Some(false)
    {
        return None;
    }
    let donation = match msg.kind {
        MessageKind::SuperChat => Some(&cfg.super_chat),
        MessageKind::Bits => Some(&cfg.bits),
        MessageKind::Gift => Some(&cfg.gift),
        _ => None,
    };
    if let Some(rule) = donation.filter(|r| money_passes(r, msg, rates, symbol_overrides)) {
        return Some(SoundCue(rule.sound.clone()));
    }
    if msg.kind == MessageKind::Membership && cfg.membership.enabled {
        return Some(SoundCue(cfg.membership.sound.clone()));
    }
    if cfg.first_comment.enabled && msg.listener.as_ref().is_some_and(|l| l.is_first_ever) {
        return Some(SoundCue(cfg.first_comment.sound.clone()));
    }
    None
}

/// アプリ全体の最小間隔。間隔内の音は捨てる(溜めない)。
#[derive(Debug, Default)]
pub struct SoundGate {
    last: Option<Instant>,
}

impl SoundGate {
    pub fn admit(&mut self, now: Instant, min_interval_ms: u32) -> bool {
        let interval = Duration::from_millis(u64::from(min_interval_ms));
        if self.last.is_some_and(|last| now.saturating_duration_since(last) < interval) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// パイプライン用: 非表示判定の物は鳴らさず、選んだ音が間引きを通れば `msg.sound` に入れる。
pub fn stamp_sound(
    msg: &mut ChatMessage,
    verdict: &Verdict,
    cfg: &AppConfig,
    defs: (&RateTable, &BTreeMap<String, String>),
    custom_exists: impl Fn(&str) -> bool,
    gate: &mut SoundGate,
    now: Instant,
) {
    if matches!(verdict, Verdict::Hide) {
        return;
    }
    let overrides = crate::tts::origin_overrides(cfg, msg);
    // 削除済みのカスタム音源を指す規則は既定の内蔵音として扱う。
    let mut resolved = cfg.sound_effects.clone();
    resolved.resolve_missing(custom_exists);
    let Some(cue) = select_sound(msg, &resolved, defs.0, defs.1, overrides) else { return };
    if gate.admit(now, cfg.sound_effects.min_interval_ms) {
        msg.sound = Some(cue.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Amount, Author, Fragment, ListenerInfo, Platform, Roles};
    use std::time::Duration;

    const UUID: &str = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";

    fn msg(kind: MessageKind) -> ChatMessage {
        ChatMessage {
            listener: None,
            native_id: false,
            id: "m1".into(),
            platform: Platform::Youtube,
            channel: "c".into(),
            author: Author {
                nickname: None,
                avatar_url: None,
                id: "u".into(),
                name: "n".into(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::text("hi")],
            kind,
            amount: None,
            timestamp_ms: 0,
            raw: None,
            skip_tts: false,
            injected_test: false,
            route: Default::default(),
            sound: None,
        }
    }

    fn money(kind: MessageKind, value: f64, currency: &str) -> ChatMessage {
        let mut m = msg(kind);
        m.amount = Some(Amount { value, currency: currency.into(), raw_text: String::new() });
        m
    }

    fn first_ever(mut m: ChatMessage) -> ChatMessage {
        m.listener = Some(ListenerInfo {
            comment_count: 1,
            gift_count: 0,
            first_seen_ms: 0,
            prev_seen_ms: None,
            is_first_ever: true,
            label: None,
            tts_muted: false,
            tts_voice: None,
        });
        m
    }

    fn on() -> SoundEffectsConfig {
        SoundEffectsConfig { enabled: true, ..Default::default() }
    }

    fn rates() -> RateTable {
        BTreeMap::from([("JPY".to_owned(), 1.0), ("USD".to_owned(), 150.0)])
    }

    fn pick(m: &ChatMessage, cfg: &SoundEffectsConfig) -> Option<String> {
        select_sound(m, cfg, &rates(), &BTreeMap::new(), None).map(|c| c.0)
    }

    #[test]
    fn disabled_by_default_and_when_master_off() {
        let m = money(MessageKind::SuperChat, 500.0, "JPY");
        assert!(!SoundEffectsConfig::default().enabled);
        assert_eq!(pick(&m, &SoundEffectsConfig::default()), None);
    }

    #[test]
    fn each_kind_picks_its_rule_sound() {
        let mut cfg = on();
        cfg.super_chat.sound = "builtin:bell".into();
        cfg.membership.sound = "builtin:coin".into();
        cfg.first_comment.enabled = true;
        cfg.first_comment.sound = format!("custom:{UUID}");
        assert_eq!(pick(&money(MessageKind::SuperChat, 1.0, "JPY"), &cfg).as_deref(), Some("builtin:bell"));
        assert_eq!(pick(&msg(MessageKind::Membership), &cfg).as_deref(), Some("builtin:coin"));
        assert_eq!(pick(&first_ever(msg(MessageKind::Normal)), &cfg), Some(format!("custom:{UUID}")));
        assert_eq!(pick(&msg(MessageKind::Normal), &cfg), None);
    }

    #[test]
    fn threshold_is_inclusive_and_converts_with_rates() {
        let mut cfg = on();
        cfg.super_chat.min_jpy = 1500;
        assert!(pick(&money(MessageKind::SuperChat, 1499.0, "JPY"), &cfg).is_none());
        assert!(pick(&money(MessageKind::SuperChat, 1500.0, "JPY"), &cfg).is_some());
        assert!(pick(&money(MessageKind::SuperChat, 9.99, "USD"), &cfg).is_none());
        assert!(pick(&money(MessageKind::SuperChat, 10.0, "USD"), &cfg).is_some());
    }

    #[test]
    fn symbol_overrides_apply_to_conversion() {
        let mut cfg = on();
        cfg.super_chat.min_jpy = 1500;
        let sym = BTreeMap::from([("ZZ$".to_owned(), "USD".to_owned())]);
        let m = money(MessageKind::SuperChat, 10.0, "ZZ$");
        assert!(select_sound(&m, &cfg, &rates(), &sym, None).is_some());
        assert!(select_sound(&m, &cfg, &rates(), &BTreeMap::new(), None).is_none());
    }

    #[test]
    fn unconvertible_currency_rings_only_when_min_is_zero() {
        let mut cfg = on();
        let m = money(MessageKind::SuperChat, 100.0, "XYZ");
        assert!(pick(&m, &cfg).is_some());
        cfg.super_chat.min_jpy = 1;
        assert!(pick(&m, &cfg).is_none());
        let mut no_amount = msg(MessageKind::SuperChat);
        no_amount.amount = None;
        assert!(pick(&no_amount, &cfg).is_none());
    }

    #[test]
    fn bits_without_rate_ignore_min_jpy_but_use_a_rate_when_defined() {
        let mut cfg = on();
        cfg.bits.min_jpy = 1000;
        let m = money(MessageKind::Bits, 100.0, "BITS");
        assert!(pick(&m, &cfg).is_some());
        let mut with_rate = rates();
        with_rate.insert("BITS".to_owned(), 1.0);
        assert!(select_sound(&m, &cfg, &with_rate, &BTreeMap::new(), None).is_none());
        let big = money(MessageKind::Bits, 1000.0, "BITS");
        assert!(select_sound(&big, &cfg, &with_rate, &BTreeMap::new(), None).is_some());
    }

    #[test]
    fn priority_is_donation_then_membership_then_first_comment() {
        let mut cfg = on();
        cfg.first_comment.enabled = true;
        cfg.super_chat.sound = "builtin:bell".into();
        cfg.membership.sound = "builtin:coin".into();
        cfg.first_comment.sound = "builtin:pop".into();
        let sc = first_ever(money(MessageKind::SuperChat, 100.0, "JPY"));
        assert_eq!(pick(&sc, &cfg).as_deref(), Some("builtin:bell"));
        let ms = first_ever(msg(MessageKind::Membership));
        assert_eq!(pick(&ms, &cfg).as_deref(), Some("builtin:coin"));
    }

    #[test]
    fn falls_through_when_higher_rule_is_off_or_below_threshold() {
        let mut cfg = on();
        cfg.first_comment.enabled = true;
        cfg.first_comment.sound = "builtin:pop".into();
        cfg.super_chat.min_jpy = 1000;
        let sc = first_ever(money(MessageKind::SuperChat, 100.0, "JPY"));
        assert_eq!(pick(&sc, &cfg).as_deref(), Some("builtin:pop"));
        cfg.first_comment.enabled = false;
        assert_eq!(pick(&sc, &cfg), None);
    }

    #[test]
    fn excluded_messages_never_ring() {
        let cfg = on();
        let mut injected = money(MessageKind::SuperChat, 100.0, "JPY");
        injected.injected_test = true;
        assert!(pick(&injected, &cfg).is_none());
        let mut backfill = money(MessageKind::SuperChat, 100.0, "JPY");
        backfill.route.uncounted = true;
        assert!(pick(&backfill, &cfg).is_none());
        let mut system = first_ever(msg(MessageKind::System));
        system.amount = Some(Amount { value: 1.0, currency: "JPY".into(), raw_text: String::new() });
        let mut cfg_first = on();
        cfg_first.first_comment.enabled = true;
        assert!(pick(&system, &cfg_first).is_none());
    }

    #[test]
    fn channel_override_false_silences_but_true_does_not() {
        let cfg = on();
        let m = money(MessageKind::SuperChat, 100.0, "JPY");
        let off = ChannelOverrides { sound_enabled: Some(false), ..Default::default() };
        let on_ = ChannelOverrides { sound_enabled: Some(true), ..Default::default() };
        assert!(select_sound(&m, &cfg, &rates(), &BTreeMap::new(), Some(&off)).is_none());
        assert!(select_sound(&m, &cfg, &rates(), &BTreeMap::new(), Some(&on_)).is_some());
    }

    #[test]
    fn gate_drops_within_interval_and_does_not_queue() {
        let t0 = Instant::now();
        let mut gate = SoundGate::default();
        assert!(gate.admit(t0, 300));
        assert!(!gate.admit(t0 + Duration::from_millis(299), 300));
        // 捨てた音は間隔の起点を動かさない。
        assert!(gate.admit(t0 + Duration::from_millis(300), 300));
        assert!(gate.admit(t0 + Duration::from_millis(300), 0));
        assert!(gate.admit(t0 + Duration::from_millis(300), 0));
    }

    fn app_cfg() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.sound_effects = on();
        cfg
    }

    #[test]
    fn stamp_sets_sound_throttles_and_skips_hidden() {
        let cfg = app_cfg();
        let (r, s) = (rates(), BTreeMap::new());
        let t0 = Instant::now();
        let mut gate = SoundGate::default();

        let mut hidden = money(MessageKind::SuperChat, 100.0, "JPY");
        stamp_sound(&mut hidden, &Verdict::Hide, &cfg, (&r, &s), |_| true, &mut gate, t0);
        assert_eq!(hidden.sound, None);

        let mut a = money(MessageKind::SuperChat, 100.0, "JPY");
        stamp_sound(&mut a, &Verdict::Show, &cfg, (&r, &s), |_| true, &mut gate, t0);
        assert!(a.sound.is_some());

        let mut b = money(MessageKind::SuperChat, 100.0, "JPY");
        stamp_sound(&mut b, &Verdict::Highlight, &cfg, (&r, &s), |_| true, &mut gate, t0 + Duration::from_millis(10));
        assert_eq!(b.sound, None);

        // 鳴らない種別は間隔を消費しない。
        let mut normal = msg(MessageKind::Normal);
        stamp_sound(&mut normal, &Verdict::Show, &cfg, (&r, &s), |_| true, &mut gate, t0 + Duration::from_millis(400));
        let mut c = money(MessageKind::SuperChat, 100.0, "JPY");
        stamp_sound(&mut c, &Verdict::Show, &cfg, (&r, &s), |_| true, &mut gate, t0 + Duration::from_millis(400));
        assert!(c.sound.is_some());
    }

    #[test]
    fn deleted_custom_sound_is_resolved_to_the_default_on_save_and_at_runtime() {
        let live = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
        let gone = "1b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
        let d = SoundEffectsConfig::default();
        let mut cfg = on();
        cfg.super_chat.sound = format!("custom:{gone}");
        cfg.gift.sound = format!("custom:{live}");
        // 保存時
        let mut saved = cfg.clone();
        assert!(saved.resolve_missing(|id| id == live));
        assert_eq!(saved.super_chat.sound, d.super_chat.sound);
        assert_eq!(saved.gift.sound, format!("custom:{live}"));
        assert!(!saved.resolve_missing(|id| id == live));
        // 実行時: 設定は削除済みの id のままでも既定音で鳴る。
        let mut app = AppConfig::default();
        app.sound_effects = cfg;
        let (r, s) = (rates(), BTreeMap::new());
        let mut sc = money(MessageKind::SuperChat, 100.0, "JPY");
        stamp_sound(&mut sc, &Verdict::Show, &app, (&r, &s), |id| id == live, &mut SoundGate::default(), Instant::now());
        assert_eq!(sc.sound, Some(d.super_chat.sound));
        let mut gift = money(MessageKind::Gift, 100.0, "JPY");
        stamp_sound(&mut gift, &Verdict::Show, &app, (&r, &s), |id| id == live, &mut SoundGate::default(), Instant::now());
        assert_eq!(gift.sound, Some(format!("custom:{live}")));
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let c: SoundEffectsConfig = serde_json::from_value(serde_json::json!({
            "volume": 7, "minIntervalMs": 6000, "superChat": {"minJpy": 99999999999u64}
        }))
        .unwrap();
        assert_eq!(c.volume, 1.0);
        assert_eq!(c.min_interval_ms, MAX_MIN_INTERVAL_MS);
        assert_eq!(c.super_chat.min_jpy, MAX_MIN_JPY);
        let neg: SoundEffectsConfig = serde_json::from_value(serde_json::json!({"volume": -2})).unwrap();
        assert_eq!(neg.volume, 0.0);
    }

    #[test]
    fn stamp_uses_channel_override_from_origin_key() {
        use crate::config::{ChannelConfig, ChannelPlatform};
        let mut cfg = app_cfg();
        let ch = ChannelConfig {
            platform: ChannelPlatform::Youtube,
            identifier: "abc".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides { sound_enabled: Some(false), ..Default::default() },
        };
        let key = crate::config::channel_key(&ch);
        cfg.channels.push(ch);
        let (r, s) = (rates(), BTreeMap::new());
        let mut m = money(MessageKind::SuperChat, 100.0, "JPY");
        m.route.origin_key = Some(key);
        stamp_sound(&mut m, &Verdict::Show, &cfg, (&r, &s), |_| true, &mut SoundGate::default(), Instant::now());
        assert_eq!(m.sound, None);
    }

    #[test]
    fn missing_or_broken_config_falls_back_to_defaults() {
        let from = |v: serde_json::Value| serde_json::from_value::<SoundEffectsConfig>(v).unwrap();
        assert_eq!(from(serde_json::json!({})), SoundEffectsConfig::default());
        let broken = from(serde_json::json!({
            "enabled": "yes", "volume": "loud", "minIntervalMs": -5,
            "superChat": 7, "bits": {"enabled": null, "sound": 3, "minJpy": "x"},
            "gift": {"sound": "../../etc/passwd"}, "membership": {"sound": "custom:not-a-uuid"}
        }));
        assert_eq!(broken, SoundEffectsConfig::default());
        let partial = from(serde_json::json!({"enabled": true, "superChat": {"minJpy": 500}}));
        assert!(partial.enabled);
        assert_eq!(partial.super_chat.min_jpy, 500);
        assert_eq!(partial.super_chat.sound, SoundEffectsConfig::default().super_chat.sound);
    }

    #[test]
    fn normalize_clamps_numbers_and_rejects_bad_refs() {
        let mut cfg = on();
        cfg.volume = 7.0;
        cfg.min_interval_ms = 999_999;
        cfg.super_chat.min_jpy = u32::MAX;
        cfg.bits.sound = "builtin:nope".into();
        cfg.gift.sound = "custom:../x".into();
        cfg.normalize();
        assert_eq!(cfg.volume, 1.0);
        assert_eq!(cfg.min_interval_ms, MAX_MIN_INTERVAL_MS);
        assert_eq!(cfg.super_chat.min_jpy, MAX_MIN_JPY);
        let d = SoundEffectsConfig::default();
        assert_eq!(cfg.bits.sound, d.bits.sound);
        assert_eq!(cfg.gift.sound, d.gift.sound);
        cfg.volume = f32::NAN;
        cfg.normalize();
        assert_eq!(cfg.volume, d.volume);
        cfg.volume = -1.0;
        cfg.normalize();
        assert_eq!(cfg.volume, 0.0);
    }

    #[test]
    fn sound_ref_validation() {
        assert!(is_valid_sound_ref("builtin:chime"));
        assert!(is_valid_sound_ref(&format!("custom:{UUID}")));
        for bad in ["", "chime", "builtin:", "builtin:Chime", "custom:", "custom:abc", "custom:../a", "file:///x", &format!("custom:{UUID}/..")] {
            assert!(!is_valid_sound_ref(bad), "{bad}");
        }
    }

    #[test]
    fn old_config_json_without_the_field_still_loads() {
        let cfg: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.sound_effects, SoundEffectsConfig::default());
    }
}
