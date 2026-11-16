use super::{Matched, Trigger};
use crate::{
    integrations::WebhookDef,
    model::{ChatMessage, MessageKind, Platform},
};
use std::collections::{HashSet, VecDeque};
pub fn decide(def: &WebhookDef, msg: &ChatMessage) -> Option<Matched> {
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
        for k in &def.keywords {
            if k.trim().is_empty() {
                continue;
            }
            if let Ok(re) = regex::RegexBuilder::new(&regex::escape(k))
                .case_insensitive(true)
                .build()
            {
                if msg.text_fragments().any(|t| re.is_match(t)) {
                    return Some(Matched {
                        trigger: Trigger::Keyword,
                        keyword: Some(k.clone()),
                    });
                }
            }
        }
    }
    None
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
