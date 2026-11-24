//! キーワードエフェクト: コメント本文に規則のキーワードが含まれたら、アプリ内と OBS(`effects`
//! テンプレート)へ演出イベントを流す。判定とクールダウンはここ(Rust)だけで行い、UI は描画のみ。

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::config::{EffectRule, EffectTarget, EffectsConfig};
use crate::model::{ChatMessage, MessageKind};
use crate::webhook::rules::{keyword_regex, text_matches};

/// 発火した演出 1 件。`target` はシリアライズしない(配信先の振り分けにだけ使う)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectTriggered {
    /// 設定内の規則の位置。
    pub rule_id: u32,
    pub emoji: String,
    pub count: u32,
    pub animation: String,
    pub duration_ms: u32,
    #[serde(skip)]
    pub target: EffectTarget,
}

/// 規則のコンパイル結果とクールダウン(単調時計)を持つ判定器。
#[derive(Default)]
pub struct KeywordEffects {
    raw: Vec<EffectRule>,
    rules: Vec<EffectRule>,
    compiled: Vec<Option<regex::Regex>>,
    last_fired: Vec<Option<Instant>>,
}

impl KeywordEffects {
    /// 照合・クールダウン引き継ぎ用のキー。同じキーワードが複数ある時は出現順で区別する。
    fn keys(rules: &[EffectRule]) -> Vec<(String, usize)> {
        let mut seen = std::collections::HashMap::<String, usize>::new();
        rules
            .iter()
            .map(|r| {
                let k = r.keyword.trim().to_lowercase();
                let n = seen.entry(k.clone()).or_insert(0);
                *n += 1;
                (k, *n)
            })
            .collect()
    }

    /// 規則が変わった時だけ作り直す。クールダウンはキーワードが同じ規則に引き継ぎ、
    /// キーワードを変えた規則・新しい規則だけをリセットする。
    fn sync(&mut self, rules: &[EffectRule]) {
        if self.raw == rules {
            return;
        }
        let old: std::collections::HashMap<(String, usize), Option<Instant>> =
            Self::keys(&self.rules).into_iter().zip(self.last_fired.drain(..)).collect();
        self.raw = rules.to_vec();
        self.rules = self.raw.clone();
        for rule in &mut self.rules {
            rule.normalize();
        }
        // 無効・表示物が空の規則は発火しない(キーワードが空白のみなら regex が None)。
        self.compiled = self
            .rules
            .iter()
            .map(|r| {
                if !r.enabled || r.emoji.trim().is_empty() {
                    None
                } else {
                    keyword_regex(&r.keyword)
                }
            })
            .collect();
        self.last_fired = Self::keys(&self.rules)
            .iter()
            .map(|k| old.get(k).copied().flatten())
            .collect();
    }

    /// 1 コメントを判定する。クールダウン中の規則は捨てる(溜めない)。
    pub fn evaluate(
        &mut self,
        cfg: &EffectsConfig,
        msg: &ChatMessage,
        now: Instant,
    ) -> Vec<EffectTriggered> {
        if !cfg.enabled
            || msg.kind == MessageKind::System
            || msg.injected_test
            || msg.route.uncounted
        {
            return Vec::new();
        }
        self.sync(&cfg.rules);
        let mut out = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            let Some(re) = &self.compiled[i] else { continue };
            if !text_matches(re, msg) {
                continue;
            }
            let cooldown = Duration::from_millis(u64::from(rule.cooldown_ms));
            if self.last_fired[i].is_some_and(|last| now.saturating_duration_since(last) < cooldown)
            {
                continue;
            }
            self.last_fired[i] = Some(now);
            out.push(trigger(i as u32, rule));
        }
        out
    }
}

fn trigger(index: u32, rule: &EffectRule) -> EffectTriggered {
    EffectTriggered {
        rule_id: index,
        emoji: rule.emoji.clone(),
        count: rule.count,
        animation: rule.animation.clone(),
        duration_ms: rule.duration_ms,
        target: rule.target,
    }
}

/// 設定画面の「テスト」用。クールダウンを無視して 1 回分を作る。表示物が空なら None。
pub fn test_trigger(index: u32, rule: &EffectRule) -> Option<EffectTriggered> {
    if rule.emoji.trim().is_empty() {
        return None;
    }
    let mut rule = rule.clone();
    rule.normalize();
    Some(trigger(index, &rule))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Fragment, Platform, Roles};

    fn rule(keyword: &str) -> EffectRule {
        EffectRule {
            keyword: keyword.into(),
            emoji: "🎉".into(),
            cooldown_ms: 0,
            ..Default::default()
        }
    }

    fn cfg(rules: Vec<EffectRule>) -> EffectsConfig {
        EffectsConfig { enabled: true, rules }
    }

    fn msg(fragments: Vec<Fragment>) -> ChatMessage {
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
                name: "partyguy".into(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments,
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms: 0,
            raw: None,
            skip_tts: false,
            injected_test: false,
            route: Default::default(),
            sound: None,
        }
    }

    fn text(s: &str) -> ChatMessage {
        msg(vec![Fragment::text(s)])
    }

    fn ids(v: &[EffectTriggered]) -> Vec<u32> {
        v.iter().map(|e| e.rule_id).collect()
    }

    #[test]
    fn matches_case_insensitively_as_substring() {
        let mut fx = KeywordEffects::default();
        let c = cfg(vec![rule("Party")]);
        let now = Instant::now();
        assert_eq!(ids(&fx.evaluate(&c, &text("lets PARTY now"), now)), vec![0]);
        assert_eq!(ids(&fx.evaluate(&c, &text("partying"), now)), vec![0]);
        assert!(fx.evaluate(&c, &text("par ty"), now).is_empty());
    }

    #[test]
    fn keyword_is_literal_not_regex() {
        let mut fx = KeywordEffects::default();
        let c = cfg(vec![rule("a.b"), rule("(")]);
        let now = Instant::now();
        assert!(fx.evaluate(&c, &text("axb"), now).is_empty());
        assert_eq!(ids(&fx.evaluate(&c, &text("a.b ("), now)), vec![0, 1]);
    }

    #[test]
    fn emote_names_and_author_names_are_not_searched() {
        let mut fx = KeywordEffects::default();
        let c = cfg(vec![rule("party")]);
        let now = Instant::now();
        let m = msg(vec![
            Fragment::text("hello "),
            Fragment::Emote { id: "1".into(), name: "party".into(), url: "u".into() },
        ]);
        // 著者名は "partyguy"。どちらも対象外。
        assert!(fx.evaluate(&c, &m, now).is_empty());
        // 断片をまたぐ文字列には一致しない(Webhook と同じ)。
        let split = msg(vec![Fragment::text("par"), Fragment::text("ty")]);
        assert!(fx.evaluate(&c, &split, now).is_empty());
    }

    #[test]
    fn each_matching_rule_fires_once_per_comment() {
        let mut fx = KeywordEffects::default();
        let mut second = rule("nice");
        second.emoji = "✨".into();
        let c = cfg(vec![rule("party"), second, rule("zzz")]);
        // 同じ規則が複数の断片・複数回に当たっても 1 回。
        let m = msg(vec![
            Fragment::text("party party"),
            Fragment::text("nice"),
            Fragment::text("party"),
        ]);
        let out = fx.evaluate(&c, &m, Instant::now());
        assert_eq!(ids(&out), vec![0, 1]);
        assert_eq!(out[1].emoji, "✨");
    }

    #[test]
    fn duplicate_rules_each_fire_independently() {
        let mut fx = KeywordEffects::default();
        let c = cfg(vec![rule("party"), rule("party")]);
        let out = fx.evaluate(&c, &text("party"), Instant::now());
        assert_eq!(ids(&out), vec![0, 1]);
    }

    #[test]
    fn excluded_comments_never_fire() {
        let mut fx = KeywordEffects::default();
        let c = cfg(vec![rule("party")]);
        let now = Instant::now();
        let mut system = text("party");
        system.kind = MessageKind::System;
        let mut injected = text("party");
        injected.injected_test = true;
        let mut history = text("party");
        history.route.uncounted = true;
        for m in [&system, &injected, &history] {
            assert!(fx.evaluate(&c, m, now).is_empty());
        }
        // 除外されたコメントはクールダウンも消費しない。
        let mut slow = rule("party");
        slow.cooldown_ms = 10_000;
        let c2 = cfg(vec![slow]);
        assert!(fx.evaluate(&c2, &system, now).is_empty());
        assert_eq!(ids(&fx.evaluate(&c2, &text("party"), now)), vec![0]);
        // 無効時は何も出ない。
        let off = EffectsConfig { enabled: false, rules: vec![rule("party")] };
        assert!(fx.evaluate(&off, &text("party"), now).is_empty());
    }

    #[test]
    fn blank_keyword_or_emoji_rules_are_inactive() {
        let mut fx = KeywordEffects::default();
        let mut no_emoji = rule("party");
        no_emoji.emoji = "  ".into();
        let c = cfg(vec![rule(""), rule("   "), no_emoji, rule("party")]);
        let out = fx.evaluate(&c, &text("party anything"), Instant::now());
        assert_eq!(ids(&out), vec![3]);
    }

    #[test]
    fn cooldown_drops_within_window_and_does_not_extend() {
        let mut fx = KeywordEffects::default();
        let mut r = rule("party");
        r.cooldown_ms = 1000;
        let c = cfg(vec![r, rule("party")]);
        let t0 = Instant::now();
        let m = text("party");
        assert_eq!(ids(&fx.evaluate(&c, &m, t0)), vec![0, 1]);
        // 規則 1 はクールダウン 0 なので毎回出る。規則 0 は捨てる。
        assert_eq!(ids(&fx.evaluate(&c, &m, t0 + Duration::from_millis(999))), vec![1]);
        // 捨てた一致は窓を延ばさない: 最初の発火から 1000ms で再び出る。
        assert_eq!(ids(&fx.evaluate(&c, &m, t0 + Duration::from_millis(1000))), vec![0, 1]);
    }

    #[test]
    fn cooldown_applies_regardless_of_target() {
        let mut fx = KeywordEffects::default();
        let mut r = rule("party");
        r.cooldown_ms = 5000;
        r.target = EffectTarget::Obs;
        let c = cfg(vec![r]);
        let t0 = Instant::now();
        assert_eq!(fx.evaluate(&c, &text("party"), t0).len(), 1);
        assert!(fx.evaluate(&c, &text("party"), t0 + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn unchanged_rules_keep_cooldown_and_edits_reset_it() {
        let mut fx = KeywordEffects::default();
        let mut r = rule("party");
        r.cooldown_ms = 5000;
        let c = cfg(vec![r.clone()]);
        let t0 = Instant::now();
        assert_eq!(fx.evaluate(&c, &text("party"), t0).len(), 1);
        assert!(fx.evaluate(&cfg(vec![r.clone()]), &text("party"), t0).is_empty());
        r.keyword = "party!".into();
        assert_eq!(fx.evaluate(&cfg(vec![r]), &text("party!"), t0).len(), 1);
    }

    #[test]
    fn disabled_rules_never_fire_but_the_test_button_still_works() {
        let mut fx = KeywordEffects::default();
        let mut off = rule("party");
        off.enabled = false;
        let c = cfg(vec![off.clone(), rule("party")]);
        assert_eq!(ids(&fx.evaluate(&c, &text("party"), Instant::now())), vec![1]);
        assert!(test_trigger(0, &off).is_some());
    }

    #[test]
    fn editing_another_rule_keeps_cooldowns_of_unchanged_keywords() {
        let mut fx = KeywordEffects::default();
        let mut a = rule("alpha");
        a.cooldown_ms = 5000;
        let mut b = rule("beta");
        b.cooldown_ms = 5000;
        let t0 = Instant::now();
        let both = text("alpha beta");
        assert_eq!(ids(&fx.evaluate(&cfg(vec![a.clone(), b.clone()]), &both, t0)), vec![0, 1]);
        // b だけ編集(キーワード変更): a の cooldown は保たれ、b はリセットされる。
        let mut b2 = b.clone();
        b2.keyword = "beta2".into();
        let t1 = t0 + Duration::from_secs(1);
        let out = fx.evaluate(&cfg(vec![a.clone(), b2.clone()]), &text("alpha beta2"), t1);
        assert_eq!(ids(&out), vec![1]);
        // 並べ替え・先頭への新規追加でも、キーワードが同じ規則は cooldown を引き継ぐ。
        let fresh = rule("gamma");
        let c3 = cfg(vec![fresh, a.clone(), b2.clone()]);
        let out = fx.evaluate(&c3, &text("gamma alpha beta2"), t1);
        assert_eq!(ids(&out), vec![0]);
        // 大文字小文字・前後空白だけの違いは同じキーワード扱い。
        let mut a_case = a.clone();
        a_case.keyword = "ALPHA".into();
        assert!(fx.evaluate(&cfg(vec![a_case]), &text("alpha"), t1).is_empty());
    }

    #[test]
    fn duplicate_keywords_keep_their_own_cooldowns() {
        let mut fx = KeywordEffects::default();
        let mut slow = rule("party");
        slow.cooldown_ms = 5000;
        let fast = rule("party");
        let t0 = Instant::now();
        let c = cfg(vec![slow.clone(), fast.clone()]);
        assert_eq!(ids(&fx.evaluate(&c, &text("party"), t0)), vec![0, 1]);
        let mut other = rule("zzz");
        other.cooldown_ms = 0;
        let c2 = cfg(vec![slow, fast, other]);
        let out = fx.evaluate(&c2, &text("party"), t0 + Duration::from_secs(1));
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn triggers_carry_normalized_rule_values_and_target() {
        let mut fx = KeywordEffects::default();
        let mut r = rule("party");
        r.count = 999;
        r.duration_ms = 1;
        r.animation = "nope".into();
        r.target = EffectTarget::Both;
        let out = fx.evaluate(&cfg(vec![r]), &text("party"), Instant::now());
        assert_eq!(
            out,
            vec![EffectTriggered {
                rule_id: 0,
                emoji: "🎉".into(),
                count: 50,
                animation: "float".into(),
                duration_ms: 500,
                target: EffectTarget::Both,
            }]
        );
    }

    #[test]
    fn serialized_event_has_camel_case_fields_without_target() {
        let ev = EffectTriggered {
            rule_id: 2,
            emoji: "🎉".into(),
            count: 3,
            animation: "rain".into(),
            duration_ms: 1500,
            target: EffectTarget::Obs,
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            serde_json::json!({"ruleId":2,"emoji":"🎉","count":3,"animation":"rain","durationMs":1500})
        );
    }

    #[test]
    fn test_trigger_ignores_cooldown_and_normalizes() {
        let mut r = rule("party");
        r.cooldown_ms = 60_000;
        r.count = 0;
        r.duration_ms = 99_999;
        r.target = EffectTarget::Both;
        let ev = test_trigger(4, &r).unwrap();
        assert_eq!((ev.rule_id, ev.count, ev.duration_ms), (4, 1, 15_000));
        assert_eq!(ev.target, EffectTarget::Both);
        // 何度でも出せる。
        assert!(test_trigger(4, &r).is_some());
        // キーワードが空でも表示はできる(テスト用)が、表示物が空なら出さない。
        r.emoji = " ".into();
        assert!(test_trigger(4, &r).is_none());
    }
}
