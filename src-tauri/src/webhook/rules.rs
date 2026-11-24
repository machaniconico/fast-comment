use super::{Matched, Trigger};
use crate::{
    integrations::WebhookDef,
    model::{ChatMessage, MessageKind, Platform},
};
use std::collections::{HashSet, VecDeque};
#[cfg(test)]
thread_local! { pub(crate) static COMPILES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
pub fn decide(def: &WebhookDef, msg: &ChatMessage) -> Option<Matched> {
    CompiledRules::new(def).decide(def, msg)
}
/// キーワードを `regex::escape` した大文字小文字無視の正規表現へ。空白のみは対象外(None)。
/// Webhook とキーワードエフェクトで共用する。
pub(crate) fn keyword_regex(keyword: &str) -> Option<regex::Regex> {
    if keyword.trim().is_empty() {
        return None;
    }
    #[cfg(test)]
    COMPILES.with(|n| n.set(n.get() + 1));
    regex::RegexBuilder::new(&regex::escape(keyword))
        .case_insensitive(true)
        .build()
        .ok()
}
/// テキスト部分(エモート名・著者名は対象外)のどれかがキーワードに一致するか。
pub(crate) fn text_matches(re: &regex::Regex, msg: &ChatMessage) -> bool {
    msg.text_fragments().any(|t| re.is_match(t))
}
pub(crate) struct CompiledRules {
    keywords: Vec<(String, regex::Regex)>,
}
impl CompiledRules {
    pub(crate) fn new(def: &WebhookDef) -> Self {
        let keywords = def
            .keywords
            .iter()
            .filter_map(|k| keyword_regex(k).map(|re| (k.clone(), re)))
            .collect();
        Self { keywords }
    }
    pub(crate) fn decide(&self, def: &WebhookDef, msg: &ChatMessage) -> Option<Matched> {
        if !def.enabled
            || def.paused.is_some()
            || msg.injected_test
            || msg.skip_tts
            || msg.kind == MessageKind::System
        {
            return None;
        }
        if def.triggers.donation
            && matches!(
                msg.kind,
                MessageKind::SuperChat | MessageKind::Bits | MessageKind::Gift
            )
        {
            return Some(Matched {
                trigger: Trigger::Donation,
                keyword: None,
            });
        }
        if def.triggers.membership && msg.kind == MessageKind::Membership {
            return Some(Matched {
                trigger: Trigger::Membership,
                keyword: None,
            });
        }
        if def.triggers.keyword {
            for (keyword, re) in &self.keywords {
                if text_matches(re, msg) {
                    return Some(Matched {
                        trigger: Trigger::Keyword,
                        keyword: Some(keyword.clone()),
                    });
                }
            }
        }
        None
    }
}

pub struct Dedup {
    capacity: usize,
    queue: VecDeque<(u8, String)>,
    seen: HashSet<(u8, String)>,
}
impl Dedup {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: VecDeque::new(),
            seen: HashSet::new(),
        }
    }
    pub fn first_seen(&mut self, platform: Platform, id: &str) -> bool {
        let key = (platform as u8, id.to_owned());
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.queue.push_back(key);
        while self.queue.len() > self.capacity {
            if let Some(k) = self.queue.pop_front() {
                self.seen.remove(&k);
            }
        }
        true
    }
}
