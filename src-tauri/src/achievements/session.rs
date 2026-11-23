//! Session matching and the shared achievement epoch.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::model::{ChatMessage, LiveEvent, Platform};
use crate::sources::twitcasting::channel_name;

use super::engine::{Counters, EPOCH_IDLE_MS, STALE_SESSION_MS};

pub(super) const PLATFORMS: [(Platform, &str); 5] = [
    (Platform::Twitch, "twitch"),
    (Platform::Youtube, "youtube"),
    (Platform::X, "x"),
    (Platform::Niconico, "niconico"),
    (Platform::Twitcasting, "twitcasting"),
];

pub(super) fn platform_key(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::X => "x",
        Platform::Niconico => "niconico",
        Platform::Twitcasting => "twitcasting",
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Epoch {
    pub key: String,
    opened_at_ms: u64,
    last_activity_ms: u64,
    pub unique_commenters: BTreeSet<String>,
    pub streamer: Counters,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_platform: BTreeMap<String, Counters>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub peak_by_platform_mask: BTreeMap<String, u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Session {
    last_activity_ms: u64,
    ended: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    channel_identifier: Option<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Sessions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<Epoch>,
    #[serde(rename = "sessions")]
    active: BTreeMap<String, Session>,
    pub seen_streams: BTreeSet<String>,
}

impl Sessions {
    /// 期限切れを除く。何か変わったら true(保存が必要)。
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let mut expired_active = false;
        let before = self.active.len();
        self.active.retain(|_, session| {
            let stale = now_ms.saturating_sub(session.last_activity_ms) >= STALE_SESSION_MS;
            expired_active |= stale && !session.ended;
            !stale
        });
        let mut changed = self.active.len() != before;
        if self.active.values().any(|session| !session.ended) {
            return changed;
        }
        if expired_active
            || self
                .epoch
                .as_ref()
                .is_some_and(|epoch| now_ms.saturating_sub(epoch.last_activity_ms) >= EPOCH_IDLE_MS)
        {
            changed |= self.epoch.is_some() || !self.active.is_empty();
            self.epoch = None;
            self.active.clear();
        }
        changed
    }

    pub fn on_live_event(&mut self, event: &LiveEvent, now_ms: u64) -> Option<Platform> {
        match event {
            LiveEvent::Started(stream) => {
                let platform = stream.platform;
                let key = format!("{}:{}", platform_key(platform), stream.stream_id);
                let identifier = channel_key(stream.platform, &stream.channel_identifier);
                // A new stream on the same channel supersedes a missing Ended event.
                // Compare channel names only: a stream ID must not supersede another channel.
                self.active.retain(|other, session| {
                    other == &key || !matches_channel_name(other, session, platform, &identifier)
                });
                let session = self.active.entry(key.clone()).or_insert(Session {
                    last_activity_ms: now_ms,
                    ended: false,
                    channel_identifier: None,
                });
                session.last_activity_ms = session.last_activity_ms.max(now_ms);
                session.ended = false;
                session.channel_identifier = Some(identifier);
                self.touch_epoch(now_ms);
                self.seen_streams.insert(key).then_some(stream.platform)
            }
            LiveEvent::Ended {
                platform,
                channel_identifier,
            } => {
                let identifier = channel_key(*platform, channel_identifier);
                let mut ended = false;
                for (key, session) in &mut self.active {
                    if !session.ended && matches_channel(key, session, *platform, &identifier) {
                        session.ended = true;
                        session.last_activity_ms = session.last_activity_ms.max(now_ms);
                        ended = true;
                    }
                }
                if ended {
                    if let Some(epoch) = &mut self.epoch {
                        epoch.last_activity_ms = epoch.last_activity_ms.max(now_ms);
                    }
                }
                None
            }
        }
    }

    pub fn session_for(&self, msg: &ChatMessage) -> Option<&str> {
        let platform = msg.platform;
        if platform != Platform::Twitcasting {
            let direct = format!("{}:{}", platform_key(platform), msg.channel);
            if let Some((key, session)) = self.active.get_key_value(&direct) {
                if !session.ended {
                    return Some(key);
                }
            }
        }
        self.active
            .iter()
            .filter(|(key, s)| !s.ended && matches_channel(key, s, platform, &msg.channel))
            .max_by_key(|(_, s)| s.last_activity_ms)
            .map(|(key, _)| key.as_str())
    }

    pub fn observe(&mut self, msg: &ChatMessage, now_ms: u64) -> &mut Epoch {
        if let Some(key) = self.session_for(msg).map(str::to_owned) {
            if let Some(session) = self.active.get_mut(&key) {
                session.last_activity_ms = session.last_activity_ms.max(now_ms);
            }
        }
        self.touch_epoch(now_ms)
    }

    fn touch_epoch(&mut self, now_ms: u64) -> &mut Epoch {
        let epoch = self.epoch.get_or_insert_with(|| Epoch {
            key: format!("e:{now_ms}"),
            opened_at_ms: now_ms,
            last_activity_ms: now_ms,
            unique_commenters: BTreeSet::new(),
            streamer: Counters::default(),
            by_platform: BTreeMap::new(),
            peak_by_platform_mask: BTreeMap::new(),
        });
        epoch.last_activity_ms = epoch.last_activity_ms.max(now_ms);
        epoch
    }
}

fn channel_key(platform: Platform, identifier: &str) -> String {
    if platform == Platform::Twitcasting {
        return channel_name(identifier);
    }
    identifier.to_owned()
}

fn matches_channel(key: &str, session: &Session, platform: Platform, identifier: &str) -> bool {
    let Some((session_platform, stream_id)) = key.split_once(':') else {
        return false;
    };
    session_platform == platform_key(platform)
        && ((platform != Platform::Twitcasting && stream_id == identifier)
            || matches_channel_name(key, session, platform, identifier))
}

fn matches_channel_name(key: &str, session: &Session, platform: Platform, identifier: &str) -> bool {
    key.split_once(':')
        .is_some_and(|(session_platform, _)| session_platform == platform_key(platform))
        && session.channel_identifier.as_ref().is_some_and(|channel| {
            if platform == Platform::Twitcasting {
                return channel == identifier;
            }
            channel.eq_ignore_ascii_case(identifier)
                || ((!channel.is_ascii() || !identifier.is_ascii())
                    && channel.to_lowercase() == identifier.to_lowercase())
        })
}
