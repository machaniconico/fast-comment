//! Pure announcement state machine. Execute actions in order; if persistence
//! fails, discard subsequent external actions and feed back `LedgerFailed`.
//! `PersistLedger` always refers to the snapshot returned by `ledger()`.

use chrono::{DateTime, Local, Utc};
use serde::Serialize;

use super::error::{XPostError, XPostErrorCode};
use super::ledger::{Ledger, LedgerEntry, LedgerState};
use super::template::{render, weighted_len};
use crate::config::{ChannelConfig, ChannelPlatform, XPostConfig};
use crate::model::{LiveEvent, LiveStream, Platform};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const APPROVAL_MS: u64 = 10 * 60 * 1000;

/// Deliberately excludes credentials; the driver supplies API connection state.
#[derive(Debug, Clone)]
pub struct AutopostSettings {
    pub x_post: XPostConfig,
    pub channels: Vec<ChannelConfig>,
    pub connected: bool,
}

/// Transport-independent result. The API driver maps its result into this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedTweet {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutopostStatus {
    pub state: String,
    pub text: Option<String>,
    pub fire_at_ms: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub reason: Option<String>,
    pub tweet_id: Option<String>,
    /// Identifies the pending announcement shown by `pending`/`awaitingApproval`.
    /// Approve/cancel must echo it so a stale click never hits the next one.
    pub id: Option<u64>,
    /// True only when `UserRetry` would actually do something.
    pub can_retry: bool,
}

impl AutopostStatus {
    pub fn new(state: &str) -> Self {
        Self {
            state: state.into(),
            text: None,
            fire_at_ms: None,
            expires_at_ms: None,
            reason: None,
            tweet_id: None,
            id: None,
            can_retry: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Input {
    Live(LiveEvent),
    Tick,
    Settings(AutopostSettings),
    UserCancel { id: u64 },
    UserRetry,
    UserApprove { id: u64 },
    PostFinished(Result<PostedTweet, XPostError>),
    /// The driver could not open the intent URL produced by the last dispatch.
    IntentOpenFailed,
    LedgerFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    PrewarmToken,
    PersistLedger,
    Post { text: String },
    OpenIntent { url: String, text: String },
    Publish(AutopostStatus),
}

#[derive(Debug, Clone)]
struct Pending {
    id: u64,
    streams: Vec<LiveStream>,
    fire_at_ms: u64,
    expires_at_ms: u64,
}

#[derive(Debug, Clone)]
struct Attempt {
    streams: Vec<LiveStream>,
    text: String,
}

/// What an explicit retry re-runs.
#[derive(Debug, Clone)]
enum Retry {
    /// Resend the already-rendered text.
    Send(Attempt),
    /// Rendering failed (template/duplicate); render again with current settings.
    Render(Vec<LiveStream>),
}

pub struct Coordinator {
    ledger: Ledger,
    settings: AutopostSettings,
    pending: Vec<Pending>,
    ended: Vec<(Platform, String)>,
    in_flight: Option<Attempt>,
    /// Intent attempt whose browser launch result is still unknown.
    opening: Option<Attempt>,
    retry: Option<Retry>,
    next_id: u64,
    persistence_failed: bool,
    status: AutopostStatus,
}

impl Coordinator {
    pub fn new(mut ledger: Ledger, mut settings: AutopostSettings) -> Self {
        ledger.recover_posting();
        settings.x_post.normalize();
        let mut status = AutopostStatus::new("idle");
        if let Some(entry) = ledger
            .entries()
            .iter()
            .find(|e| e.state == LedgerState::Uncertain)
        {
            status.state = "uncertain".into();
            status.text = entry.text.clone();
            status.reason = entry.reason.clone();
        }
        Self {
            ledger,
            settings,
            pending: Vec::new(),
            ended: Vec::new(),
            in_flight: None,
            opening: None,
            retry: None,
            next_id: 1,
            persistence_failed: false,
            status,
        }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }
    pub fn status(&self) -> &AutopostStatus {
        &self.status
    }

    pub fn handle(&mut self, input: Input, now_ms: u64) -> Vec<Action> {
        let mut actions = Vec::new();
        // The open result is fed back immediately after the dispatch batch.
        let opening = self.opening.take();
        if let Input::LedgerFailed(_) = &input {
            // Do not echo arbitrary driver error strings (paths/credentials).
            self.persistence_failed = true;
            self.pending.clear();
            self.in_flight = None;
            self.retry = None;
            let mut status = AutopostStatus::new("failed");
            status.reason = Some("ledgerFailed".into());
            self.publish(status, &mut actions);
            return actions;
        }
        if self.persistence_failed {
            return actions;
        }
        match input {
            Input::Live(LiveEvent::Started(stream)) => self.started(stream, now_ms, &mut actions),
            Input::Live(LiveEvent::Ended {
                platform,
                channel_identifier,
            }) => {
                if !self.ended.contains(&(platform, channel_identifier.clone())) {
                    self.ended.push((platform, channel_identifier.clone()));
                }
                for pending in &mut self.pending {
                    pending.streams.retain(|s| {
                        s.platform != platform || s.channel_identifier != channel_identifier
                    });
                }
                self.pending.retain(|p| !p.streams.is_empty());
                self.publish_pending(now_ms, &mut actions);
            }
            Input::Settings(mut settings) => {
                settings.x_post.normalize();
                // Switching dispatch policy cancels pending work; it must never
                // silently convert a button approval into an automatic post.
                let policy_changed = settings.x_post.mode != self.settings.x_post.mode
                    || settings.x_post.intent_open != self.settings.x_post.intent_open
                    || settings.x_post.combine != self.settings.x_post.combine;
                self.settings = settings;
                if policy_changed {
                    self.pending.clear();
                }
                self.prune();
                self.publish_pending(now_ms, &mut actions);
            }
            Input::Tick => self.advance(now_ms, false, &mut actions),
            Input::UserApprove { id } => {
                if self.pending.first().is_some_and(|p| p.id == id) {
                    self.advance(now_ms, true, &mut actions);
                }
            }
            Input::UserCancel { id } => {
                // Only the announcement the banner showed may be cancelled.
                if self.pending.first().is_some_and(|p| p.id == id) {
                    let pending = self.pending.remove(0);
                    self.record(
                        &pending.streams,
                        LedgerState::Cancelled,
                        now_ms,
                        None,
                        None,
                        None,
                    );
                    actions.push(Action::PersistLedger);
                    self.publish(AutopostStatus::new("cancelled"), &mut actions);
                }
            }
            Input::UserRetry => {
                if self.in_flight.is_none() && self.settings.x_post.enabled && self.can_dispatch() {
                    match self.retry.take() {
                        Some(Retry::Send(mut attempt)) => {
                            attempt.streams.retain(|s| eligible(&self.settings, s));
                            if !attempt.streams.is_empty() {
                                self.dispatch(attempt, now_ms, &mut actions);
                            }
                        }
                        Some(Retry::Render(mut streams)) => {
                            streams.retain(|s| eligible(&self.settings, s));
                            if !streams.is_empty() {
                                self.render_and_dispatch(streams, now_ms, &mut actions);
                            }
                        }
                        None => {}
                    }
                }
            }
            Input::PostFinished(result) => {
                if let Some(attempt) = self.in_flight.take() {
                    let (state, reason, tweet_id) = match result {
                        Ok(posted) => (LedgerState::Posted, None, Some(posted.id)),
                        Err(error) => {
                            self.retry = Some(Retry::Send(attempt.clone()));
                            let state = if error.code == XPostErrorCode::Uncertain {
                                LedgerState::Uncertain
                            } else {
                                LedgerState::Failed
                            };
                            (state, Some(error_reason(error.code)), None)
                        }
                    };
                    self.record(
                        &attempt.streams,
                        state,
                        now_ms,
                        Some(&attempt.text),
                        reason,
                        tweet_id.as_deref(),
                    );
                    actions.push(Action::PersistLedger);
                    let mut status = AutopostStatus::new(state_name(state));
                    status.text = Some(attempt.text);
                    status.reason = reason.map(str::to_owned);
                    status.tweet_id = tweet_id;
                    self.publish(status, &mut actions);
                }
            }
            Input::IntentOpenFailed => {
                if let Some(attempt) = opening {
                    // Nothing was shown: record failure (not counted toward
                    // cooldown/daily limit) and allow an explicit retry.
                    self.record(
                        &attempt.streams,
                        LedgerState::Failed,
                        now_ms,
                        Some(&attempt.text),
                        Some("openFailed"),
                        None,
                    );
                    actions.push(Action::PersistLedger);
                    let mut status = AutopostStatus::new("failed");
                    status.text = Some(attempt.text.clone());
                    status.reason = Some("openFailed".into());
                    self.retry = Some(Retry::Send(attempt));
                    self.publish(status, &mut actions);
                }
            }
            Input::LedgerFailed(_) => unreachable!(),
        }
        // Once the current announcement completes, expose the next banner.
        // Do not refresh it on every Tick (the UI owns the countdown).
        if self.in_flight.is_none()
            && !self.pending.is_empty()
            && !matches!(self.status.state.as_str(), "pending" | "awaitingApproval")
        {
            self.publish_pending(now_ms, &mut actions);
        }
        actions
    }

    fn publish(&mut self, mut status: AutopostStatus, actions: &mut Vec<Action>) {
        status.can_retry =
            self.retry.is_some() && matches!(status.state.as_str(), "failed" | "uncertain");
        if self.status != status {
            self.status = status.clone();
            actions.push(Action::Publish(status));
        }
    }

    fn can_dispatch(&self) -> bool {
        self.settings.x_post.mode == "intent" || self.settings.connected
    }

    fn button(&self) -> bool {
        self.settings.x_post.mode == "intent" && self.settings.x_post.intent_open == "button"
    }

    fn started(&mut self, stream: LiveStream, now: u64, actions: &mut Vec<Action>) {
        // Keep the specification's order: disabled rows never write history.
        if !eligible(&self.settings, &stream) {
            return;
        }
        let key = stream_key(&stream);
        if self.ledger.get(&key).is_some()
            || self
                .pending
                .iter()
                .any(|p| p.streams.iter().any(|s| stream_key(s) == key))
        {
            return;
        }
        // An Ended seen in this run means the start itself was observed.
        let observed = self
            .ended
            .contains(&(stream.platform, stream.channel_identifier.clone()));
        let stale_ms = u64::from(self.settings.x_post.stale_minutes) * 60_000;
        let reason = match stream.started_at_ms {
            // staleMinutes=0: announce only observed starts (§6.5). Detection
            // lag always puts the start time slightly in the past.
            Some(_) if stale_ms == 0 && !observed => Some("stale"),
            Some(start) if stale_ms > 0 && now.saturating_sub(start) > stale_ms => Some("stale"),
            None if !observed => Some("unknownStart"),
            _ => None,
        }
        .or_else(|| self.limit_reason(&stream, now));
        if let Some(reason) = reason {
            self.skip(&[stream], reason, now, actions);
            return;
        }
        if !self.can_dispatch() {
            let mut status = AutopostStatus::new("skipped");
            status.reason = Some("notConnected".into());
            self.publish(status, actions);
            return;
        }
        if self.settings.x_post.combine && !self.pending.is_empty() {
            self.pending[0].streams.push(stream);
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.pending.push(Pending {
                id,
                streams: vec![stream],
                fire_at_ms: now.saturating_add(u64::from(self.settings.x_post.delay_secs) * 1000),
                expires_at_ms: now.saturating_add(APPROVAL_MS),
            });
            if self.settings.x_post.mode == "api" {
                actions.push(Action::PrewarmToken);
            }
        }
        self.publish_pending(now, actions);
        self.advance(now, false, actions);
    }

    fn limit_reason(&self, stream: &LiveStream, now: u64) -> Option<&'static str> {
        // Count announcements, not ledger rows: a combined post has one row per
        // stream, sharing timestamp/text/tweet ID. Posting reserves capacity
        // while an API request is in flight, preventing queued overshoot.
        let attempts: Vec<_> = self
            .ledger
            .entries()
            .iter()
            .filter(|e| {
                matches!(
                    e.state,
                    LedgerState::Posted | LedgerState::Opened | LedgerState::Posting
                )
            })
            .collect();
        let cooldown = u64::from(self.settings.x_post.cooldown_minutes) * 60_000;
        if attempts.iter().any(|e| {
            (self.settings.x_post.combine || e.platform == stream.platform)
                && now.saturating_sub(e.at_ms) < cooldown
        }) {
            return Some("cooldown");
        }
        let mut distinct = Vec::new();
        for e in attempts {
            if now.saturating_sub(e.at_ms) < DAY_MS {
                let identity = (
                    e.at_ms,
                    e.text.as_deref(),
                    e.tweet_id.as_deref(),
                    // Without text or an ID, there is no evidence two records
                    // belong to one combined announcement.
                    (e.text.is_none() && e.tweet_id.is_none()).then_some(e.key.as_str()),
                );
                if !distinct.contains(&identity) {
                    distinct.push(identity);
                }
            }
        }
        (distinct.len() >= self.settings.x_post.daily_limit as usize).then_some("dailyLimit")
    }

    fn skip(&mut self, streams: &[LiveStream], reason: &str, now: u64, actions: &mut Vec<Action>) {
        self.record(streams, LedgerState::Skipped, now, None, Some(reason), None);
        actions.push(Action::PersistLedger);
        let mut status = AutopostStatus::new("skipped");
        status.reason = Some(reason.into());
        self.publish(status, actions);
    }

    fn prune(&mut self) {
        for pending in &mut self.pending {
            pending.streams.retain(|s| eligible(&self.settings, s));
        }
        self.pending.retain(|p| !p.streams.is_empty());
    }

    fn publish_pending(&mut self, now: u64, actions: &mut Vec<Action>) {
        if let Some(pending) = self.pending.first() {
            let mut status = AutopostStatus::new(if self.button() {
                "awaitingApproval"
            } else {
                "pending"
            });
            status.id = Some(pending.id);
            status.fire_at_ms = Some(pending.fire_at_ms);
            status.expires_at_ms = self.button().then_some(pending.expires_at_ms);
            status.text = local_time(now)
                .and_then(|time| {
                    render(&self.settings.x_post.template, &pending.streams, time).ok()
                })
                .map(|r| r.text);
            self.publish(status, actions);
        } else if matches!(self.status.state.as_str(), "pending" | "awaitingApproval") {
            self.publish(AutopostStatus::new("idle"), actions);
        }
    }

    fn advance(&mut self, now: u64, approved: bool, actions: &mut Vec<Action>) {
        self.prune();
        if self.in_flight.is_some() {
            return;
        }
        if self.button() && self.pending.first().is_some_and(|p| now >= p.expires_at_ms) {
            let expired = self.pending.remove(0);
            self.record(
                &expired.streams,
                LedgerState::Cancelled,
                now,
                None,
                Some("expired"),
                None,
            );
            actions.push(Action::PersistLedger);
            let mut status = AutopostStatus::new("cancelled");
            status.reason = Some("expired".into());
            self.publish(status, actions);
            return;
        }
        let ready = self.pending.first().is_some_and(|p| {
            if self.button() {
                approved
            } else {
                now >= p.fire_at_ms
            }
        });
        if !ready {
            // Do not re-render on ticks: UI computes its own countdown.
            return;
        }
        let pending = self.pending.remove(0);
        if !self.can_dispatch() {
            let mut status = AutopostStatus::new("skipped");
            status.reason = Some("notConnected".into());
            self.publish(status, actions);
            return;
        }
        // Queued individual announcements must see successes since detection.
        if let Some(reason) = pending
            .streams
            .iter()
            .find_map(|s| self.limit_reason(s, now))
        {
            self.skip(&pending.streams, reason, now, actions);
            return;
        }
        self.render_and_dispatch(pending.streams, now, actions);
    }

    /// Render the text (with the one-time duplicate suffix) and dispatch it;
    /// on failure record it and keep the streams for an explicit retry.
    fn render_and_dispatch(&mut self, streams: Vec<LiveStream>, now: u64, actions: &mut Vec<Action>) {
        match self.render_text(&streams, now) {
            Ok(text) => self.dispatch(Attempt { streams, text }, now, actions),
            Err(error) => {
                let reason = error_reason(error.code);
                self.record(&streams, LedgerState::Failed, now, None, Some(reason), None);
                actions.push(Action::PersistLedger);
                self.retry = Some(Retry::Render(streams));
                let mut status = AutopostStatus::new("failed");
                status.reason = Some(reason.into());
                self.publish(status, actions);
            }
        }
    }

    fn render_text(&self, streams: &[LiveStream], now: u64) -> Result<String, XPostError> {
        let time = local_time(now).ok_or_else(|| XPostError::new(XPostErrorCode::Other))?;
        let template = &self.settings.x_post.template;
        let text = render(template, streams, time)?.text;
        if !self.ledger.recent_texts().any(|old| old == text) {
            return Ok(text);
        }
        let mut text = format!("{text} {}", time.format("%H:%M"));
        if weighted_len(&text) > 280 {
            // Let title truncation absorb the suffix; render alone decides TooLong.
            text = render(&format!("{template} {{time}}"), streams, time)?.text;
        }
        if self.ledger.recent_texts().any(|old| old == text) {
            return Err(XPostError::new(XPostErrorCode::Duplicate));
        }
        Ok(text)
    }

    fn dispatch(&mut self, attempt: Attempt, now: u64, actions: &mut Vec<Action>) {
        self.retry = None;
        let intent = self.settings.x_post.mode == "intent";
        let state = if intent {
            LedgerState::Opened
        } else {
            LedgerState::Posting
        };
        self.record(
            &attempt.streams,
            state,
            now,
            Some(&attempt.text),
            None,
            None,
        );
        actions.push(Action::PersistLedger);
        if intent {
            let mut url =
                reqwest::Url::parse("https://x.com/intent/post").expect("static intent URL");
            url.query_pairs_mut().append_pair("text", &attempt.text);
            actions.push(Action::OpenIntent {
                url: url.into(),
                text: attempt.text.clone(),
            });
            self.opening = Some(attempt.clone());
        } else {
            self.in_flight = Some(attempt.clone());
            actions.push(Action::Post {
                text: attempt.text.clone(),
            });
        }
        let mut status = AutopostStatus::new(state_name(state));
        status.text = Some(attempt.text);
        self.publish(status, actions);
    }

    fn record(
        &mut self,
        streams: &[LiveStream],
        state: LedgerState,
        now: u64,
        text: Option<&str>,
        reason: Option<&str>,
        tweet_id: Option<&str>,
    ) {
        for stream in streams {
            self.ledger
                .upsert(LedgerEntry {
                    key: stream_key(stream),
                    platform: stream.platform,
                    stream_id: stream.stream_id.clone(),
                    channel: stream.channel_identifier.clone(),
                    state,
                    at_ms: now,
                    tweet_id: tweet_id.map(str::to_owned),
                    text: text.map(str::to_owned),
                    reason: reason.map(str::to_owned),
                })
                .expect("coordinator always supplies a reason for skipped entries");
        }
    }
}

fn local_time(now: u64) -> Option<DateTime<Local>> {
    DateTime::<Utc>::from_timestamp_millis(i64::try_from(now).ok()?)
        .map(|time| time.with_timezone(&Local))
}

fn stream_key(stream: &LiveStream) -> String {
    let platform = match stream.platform {
        Platform::Youtube => "youtube",
        Platform::Twitch => "twitch",
        Platform::Niconico => "niconico",
        Platform::X => "x",
    };
    format!("{platform}:{}", stream.stream_id)
}

fn eligible(settings: &AutopostSettings, stream: &LiveStream) -> bool {
    if !settings.x_post.enabled || stream.stream_id.trim().is_empty() {
        return false;
    }
    settings.channels.iter().any(|row| {
        let platform = match row.platform {
            ChannelPlatform::Youtube => Platform::Youtube,
            ChannelPlatform::Twitch => Platform::Twitch,
            ChannelPlatform::Niconico => Platform::Niconico,
            ChannelPlatform::X => Platform::X,
        };
        row.enabled
            && row.own_stream
            && platform == stream.platform
            && row.identifier == stream.channel_identifier
            && match platform {
                Platform::X => false,
                Platform::Youtube => youtube_channel(&row.identifier),
                _ => true,
            }
    })
}

// Match the channel forms supported by the YouTube source, without depending
// on the source module or doing any resolution/network I/O.
fn youtube_channel(identifier: &str) -> bool {
    fn channel_id(id: &str) -> bool {
        id.len() == 24
            && id.starts_with("UC")
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }
    fn handle(value: &str) -> bool {
        !value
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('@')
            .is_empty()
    }
    let id = identifier.trim();
    if let Some(value) = id.strip_prefix('@') {
        return handle(value);
    }
    if channel_id(id) {
        return true;
    }
    let normalized = if id.contains("://") {
        id.to_owned()
    } else {
        format!("https://{id}")
    };
    let Ok(url) = reqwest::Url::parse(&normalized) else {
        return false;
    };
    if !matches!(
        url.host_str(),
        Some("youtube.com" | "www.youtube.com" | "m.youtube.com")
    ) {
        return false;
    }
    let mut parts = url.path().split('/').filter(|s| !s.is_empty());
    match parts.next() {
        Some(first) if first.starts_with('@') => handle(&first[1..]),
        Some(first) if first.eq_ignore_ascii_case("channel") => {
            parts.next().is_some_and(channel_id)
        }
        _ => false,
    }
}

fn state_name(state: LedgerState) -> &'static str {
    match state {
        LedgerState::Posting => "posting",
        LedgerState::Posted => "posted",
        LedgerState::Opened => "opened",
        LedgerState::Failed => "failed",
        LedgerState::Uncertain => "uncertain",
        LedgerState::Cancelled => "cancelled",
        LedgerState::Skipped => "skipped",
    }
}

fn error_reason(code: XPostErrorCode) -> &'static str {
    match code {
        XPostErrorCode::Duplicate => "Duplicate",
        XPostErrorCode::TooLong => "TooLong",
        XPostErrorCode::MissingUrl => "MissingUrl",
        XPostErrorCode::Uncertain => "Uncertain",
        XPostErrorCode::Network => "Network",
        XPostErrorCode::ReloginRequired => "ReloginRequired",
        XPostErrorCode::NotLoggedIn => "NotLoggedIn",
        XPostErrorCode::Forbidden => "Forbidden",
        XPostErrorCode::InsufficientCredits => "InsufficientCredits",
        XPostErrorCode::RateLimited => "RateLimited",
        XPostErrorCode::Cancelled => "Cancelled",
        XPostErrorCode::PortInUse => "PortInUse",
        XPostErrorCode::LoginTimeout => "LoginTimeout",
        XPostErrorCode::TokenStore => "TokenStore",
        XPostErrorCode::Other => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn stream(platform: Platform, id: &str) -> LiveStream {
        LiveStream {
            platform,
            channel_identifier: match platform {
                Platform::Youtube => "@mine",
                _ => "mine",
            }
            .into(),
            stream_id: id.into(),
            title: Some("日本語 & title #?".into()),
            url: format!("https://example.com/{id}?a=1&b=2"),
            started_at_ms: Some(NOW),
        }
    }

    fn settings(mode: &str) -> AutopostSettings {
        AutopostSettings {
            x_post: XPostConfig {
                enabled: true,
                mode: mode.into(),
                intent_open: "auto".into(),
                delay_secs: 60,
                ..XPostConfig::default()
            },
            channels: vec![
                ChannelConfig {
                    platform: ChannelPlatform::Youtube,
                    identifier: "@mine".into(),
                    enabled: true,
                    own_stream: true,
                },
                ChannelConfig {
                    platform: ChannelPlatform::Twitch,
                    identifier: "mine".into(),
                    enabled: true,
                    own_stream: true,
                },
            ],
            connected: true,
        }
    }

    fn shown(c: &Coordinator) -> u64 {
        c.status().id.expect("banner shows an announcement id")
    }

    fn started(c: &mut Coordinator, s: LiveStream, now: u64) -> Vec<Action> {
        c.handle(Input::Live(LiveEvent::Started(s)), now)
    }

    fn external(actions: &[Action]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, Action::Post { .. } | Action::OpenIntent { .. }))
            .count()
    }

    fn assert_order(actions: &[Action]) {
        let persist = actions
            .iter()
            .position(|a| matches!(a, Action::PersistLedger))
            .expect("persist action");
        let send = actions
            .iter()
            .position(|a| matches!(a, Action::Post { .. } | Action::OpenIntent { .. }))
            .expect("external action");
        assert!(persist < send);
    }

    fn seed(ledger: &mut Ledger, s: &LiveStream, state: LedgerState, now: u64, text: Option<&str>) {
        ledger
            .upsert(LedgerEntry {
                key: stream_key(s),
                platform: s.platform,
                stream_id: s.stream_id.clone(),
                channel: s.channel_identifier.clone(),
                state,
                at_ms: now,
                text: text.map(str::to_owned),
                tweet_id: None,
                reason: (state == LedgerState::Skipped).then(|| "stale".into()),
            })
            .unwrap();
    }

    #[test]
    fn repeated_started_fires_once_and_always_persists_first() {
        for mode in ["api", "intent"] {
            let mut c = Coordinator::new(Ledger::default(), settings(mode));
            for _ in 0..3 {
                assert_eq!(
                    external(&started(&mut c, stream(Platform::Youtube, "1"), NOW)),
                    0
                );
            }
            let fired = c.handle(Input::Tick, NOW + 60_000);
            assert_eq!(external(&fired), 1);
            assert_order(&fired);
            assert_eq!(external(&c.handle(Input::Tick, NOW + 61_000)), 0);
            assert_eq!(
                external(&started(
                    &mut c,
                    stream(Platform::Youtube, "1"),
                    NOW + 62_000
                )),
                0
            );
        }
    }

    #[test]
    fn every_ledger_state_blocks_replayed_stream_across_restart() {
        for state in [
            LedgerState::Posting,
            LedgerState::Posted,
            LedgerState::Opened,
            LedgerState::Failed,
            LedgerState::Uncertain,
            LedgerState::Skipped,
            LedgerState::Cancelled,
        ] {
            let s = stream(Platform::Youtube, "1");
            let mut ledger = Ledger::default();
            seed(&mut ledger, &s, state, NOW, None);
            let mut c = Coordinator::new(ledger, settings("intent"));
            assert!(started(&mut c, s, NOW).is_empty());
            assert!(c.handle(Input::Tick, NOW + 60_000).is_empty());
            if state == LedgerState::Posting {
                assert_eq!(c.ledger().entries()[0].state, LedgerState::Uncertain);
                assert_eq!(c.status().state, "uncertain");
            }
        }
    }

    #[test]
    fn ledger_failure_stops_all_future_external_work_even_after_settings_retry() {
        for mode in ["api", "intent"] {
            let mut c = Coordinator::new(Ledger::default(), settings(mode));
            started(&mut c, stream(Platform::Youtube, "1"), NOW);
            let actions = c.handle(Input::Tick, NOW + 60_000);
            assert_order(&actions);
            // Driver encounters persistence failure and discards external action.
            c.handle(Input::LedgerFailed("sensitive error".into()), NOW + 60_000);
            assert_eq!(c.status().reason.as_deref(), Some("ledgerFailed"));
            for input in [
                Input::Tick,
                Input::UserApprove { id: 1 },
                Input::UserRetry,
                Input::Settings(settings(mode)),
                Input::Live(LiveEvent::Started(stream(Platform::Twitch, "2"))),
            ] {
                assert_eq!(external(&c.handle(input, NOW + 120_000)), 0);
            }
        }
    }

    #[test]
    fn ended_removes_only_its_stream_and_leaves_no_ledger_record() {
        let mut c = Coordinator::new(Ledger::default(), settings("intent"));
        let s = stream(Platform::Youtube, "1");
        started(&mut c, s.clone(), NOW);
        c.handle(
            Input::Live(LiveEvent::Ended {
                platform: s.platform,
                channel_identifier: s.channel_identifier.clone(),
            }),
            NOW + 1000,
        );
        assert!(c.ledger().entries().is_empty());
        assert_eq!(c.status().state, "idle");
        started(&mut c, s, NOW + 2000);
        assert_eq!(c.status().fire_at_ms, Some(NOW + 62_000));
        assert_eq!(external(&c.handle(Input::Tick, NOW + 60_000)), 0);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 62_000)), 1);
    }

    #[test]
    fn combine_preserves_deadline_and_counts_one_announcement() {
        let mut c = Coordinator::new(Ledger::default(), settings("intent"));
        started(&mut c, stream(Platform::Youtube, "1"), NOW);
        started(&mut c, stream(Platform::Twitch, "2"), NOW + 30_000);
        assert_eq!(c.status().fire_at_ms, Some(NOW + 60_000));
        let actions = c.handle(Input::Tick, NOW + 60_000);
        assert_eq!(external(&actions), 1);
        assert_eq!(c.ledger().entries().len(), 2);
        let text = c.status().text.as_ref().unwrap();
        assert!(text.contains("/1?") && text.contains("/2?"));
        // Disable cooldown only by advancing beyond it. Limit of 2 allows a
        // second combined announcement even though the first has two rows.
        let mut config = settings("intent");
        config.x_post.daily_limit = 2;
        c.handle(Input::Settings(config), NOW + 3_660_000);
        let mut s = stream(Platform::Youtube, "3");
        s.started_at_ms = Some(NOW + 3_660_000);
        started(&mut c, s, NOW + 3_660_000);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 3_720_000)), 1);
    }

    #[test]
    fn individual_posts_have_independent_deadlines_and_platform_cooldowns() {
        let mut config = settings("intent");
        config.x_post.combine = false;
        let mut c = Coordinator::new(Ledger::default(), config);
        started(&mut c, stream(Platform::Youtube, "1"), NOW);
        started(&mut c, stream(Platform::Twitch, "2"), NOW + 1000);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 60_000)), 1);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 61_000)), 1);
        assert_eq!(c.ledger().entries().len(), 2);
        started(&mut c, stream(Platform::Youtube, "3"), NOW + 62_000);
        assert_eq!(
            c.ledger().get("youtube:3").unwrap().reason.as_deref(),
            Some("cooldown")
        );
    }

    #[test]
    fn freshness_and_limit_checks_follow_specification_order() {
        for (kind, expected) in [
            ("stale", "stale"),
            ("unknown", "unknownStart"),
            ("cooldown", "cooldown"),
            ("daily", "dailyLimit"),
        ] {
            let mut ledger = Ledger::default();
            seed(
                &mut ledger,
                &stream(Platform::Twitch, "previous"),
                LedgerState::Opened,
                if kind == "daily" {
                    NOW - 4_000_000
                } else {
                    NOW
                },
                None,
            );
            let mut config = settings("intent");
            config.x_post.daily_limit = 1;
            let mut c = Coordinator::new(ledger, config);
            let mut s = stream(Platform::Youtube, "next");
            if kind == "stale" {
                s.started_at_ms = Some(NOW - 900_001);
            }
            if kind == "unknown" {
                s.started_at_ms = None;
            }
            assert_eq!(external(&started(&mut c, s, NOW)), 0);
            let entry = c.ledger().get("youtube:next").unwrap();
            assert_eq!(entry.state, LedgerState::Skipped);
            assert_eq!(entry.reason.as_deref(), Some(expected));
        }
    }

    #[test]
    fn unknown_start_after_ended_and_exact_freshness_boundary_are_allowed() {
        for unknown in [true, false] {
            let mut c = Coordinator::new(Ledger::default(), settings("intent"));
            let mut s = stream(Platform::Youtube, "1");
            if unknown {
                s.started_at_ms = None;
                c.handle(
                    Input::Live(LiveEvent::Ended {
                        platform: s.platform,
                        channel_identifier: s.channel_identifier.clone(),
                    }),
                    NOW,
                );
            } else {
                s.started_at_ms = Some(NOW - 900_000);
            }
            started(&mut c, s, NOW);
            assert_eq!(external(&c.handle(Input::Tick, NOW + 60_000)), 1);
        }
    }

    #[test]
    fn disabled_unmarked_unsupported_and_disabled_rows_never_write() {
        for kind in 0..5 {
            let mut config = settings("intent");
            let mut s = stream(Platform::Youtube, "1");
            match kind {
                0 => config.x_post.enabled = false,
                1 => config.channels[0].own_stream = false,
                2 => config.channels[0].enabled = false,
                3 => {
                    config.channels[0].identifier = "dQw4w9WgXcQ".into();
                    s.channel_identifier = "dQw4w9WgXcQ".into();
                }
                _ => {
                    config.channels[0].platform = ChannelPlatform::X;
                    s.platform = Platform::X;
                }
            }
            let mut c = Coordinator::new(Ledger::default(), config);
            assert!(started(&mut c, s, NOW).is_empty());
            c.handle(Input::Tick, NOW + 60_000);
            assert!(c.ledger().entries().is_empty());
        }
    }

    #[test]
    fn cancellation_is_durable_and_button_requires_approval_or_expires() {
        let mut config = settings("intent");
        config.x_post.intent_open = "button".into();
        for cancel in [true, false] {
            let mut c = Coordinator::new(Ledger::default(), config.clone());
            let s = stream(Platform::Youtube, "1");
            let actions = started(&mut c, s.clone(), NOW);
            assert_eq!(external(&actions), 0);
            assert_eq!(c.status().state, "awaitingApproval");
            assert!(c.handle(Input::Tick, NOW + 60_000).is_empty());
            if cancel {
                let id = shown(&c);
                c.handle(Input::UserCancel { id }, NOW + 61_000);
                assert_eq!(c.ledger().entries()[0].state, LedgerState::Cancelled);
                assert!(started(&mut c, s, NOW + 62_000).is_empty());
            } else {
                let id = shown(&c);
                let actions = c.handle(Input::UserApprove { id }, NOW + 61_000);
                assert_order(&actions);
                assert_eq!(external(&actions), 1);
                assert_eq!(c.ledger().entries()[0].state, LedgerState::Opened);
            }
        }
        let mut c = Coordinator::new(Ledger::default(), config);
        started(&mut c, stream(Platform::Youtube, "1"), NOW);
        assert_eq!(
            external(&c.handle(Input::UserApprove { id: shown(&c) }, NOW + APPROVAL_MS)),
            0
        );
        assert_eq!(c.ledger().entries()[0].reason.as_deref(), Some("expired"));
        assert_eq!(
            external(&c.handle(Input::UserApprove { id: 1 }, NOW + APPROVAL_MS + 1)),
            0
        );
    }

    #[test]
    fn intent_url_round_trips_unicode_newlines_and_query_punctuation() {
        let mut config = settings("intent");
        config.x_post.delay_secs = 0;
        config.connected = false; // Intent never needs OAuth.
        let mut c = Coordinator::new(Ledger::default(), config);
        let actions = started(&mut c, stream(Platform::Youtube, "1"), NOW);
        let (url, text) = actions
            .iter()
            .find_map(|a| match a {
                Action::OpenIntent { url, text } => Some((url, text)),
                _ => None,
            })
            .unwrap();
        assert!(url.starts_with("https://x.com/intent/post?text="));
        assert!(!url.contains('日') && !url.contains('\n'));
        let url = reqwest::Url::parse(url).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            vec![("text".into(), text.as_str().into())]
        );
        assert!(!actions.contains(&Action::PrewarmToken));
    }

    #[test]
    fn duplicate_text_gets_one_time_suffix_then_fails_if_still_duplicate() {
        for duplicate_suffix in [false, true] {
            let s = stream(Platform::Youtube, "1");
            let mut config = settings("intent");
            config.x_post.delay_secs = 0;
            let base = render(
                &config.x_post.template,
                &[s.clone()],
                local_time(NOW).unwrap(),
            )
            .unwrap()
            .text;
            let suffix = format!("{base} {}", local_time(NOW).unwrap().format("%H:%M"));
            let mut ledger = Ledger::default();
            seed(
                &mut ledger,
                &stream(Platform::Youtube, "old"),
                LedgerState::Failed,
                NOW - 1,
                Some(&base),
            );
            if duplicate_suffix {
                seed(
                    &mut ledger,
                    &stream(Platform::Twitch, "older"),
                    LedgerState::Failed,
                    NOW - 2,
                    Some(&suffix),
                );
            }
            let mut c = Coordinator::new(ledger, config);
            let actions = started(&mut c, s, NOW);
            if duplicate_suffix {
                assert_eq!(external(&actions), 0);
                assert_eq!(
                    c.ledger().get("youtube:1").unwrap().reason.as_deref(),
                    Some("Duplicate")
                );
            } else {
                assert_eq!(external(&actions), 1);
                assert_eq!(c.status().text.as_deref(), Some(suffix.as_str()));
            }
        }
    }

    #[test]
    fn settings_are_rechecked_and_never_backfill_pending_work() {
        let mut c = Coordinator::new(Ledger::default(), settings("intent"));
        started(&mut c, stream(Platform::Youtube, "1"), NOW);
        started(&mut c, stream(Platform::Twitch, "2"), NOW + 1000);
        let mut config = settings("intent");
        config.channels[0].own_stream = false;
        c.handle(Input::Settings(config), NOW + 2000);
        let actions = c.handle(Input::Tick, NOW + 60_000);
        assert_eq!(external(&actions), 1);
        assert!(c.ledger().get("youtube:1").is_none());
        assert!(c.ledger().get("twitch:2").is_some());
        c.handle(Input::Settings(settings("intent")), NOW + 61_000);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 62_000)), 0);
    }

    #[test]
    fn api_results_are_recorded_and_only_explicit_retry_resends() {
        let mut config = settings("api");
        config.x_post.delay_secs = 0;
        let mut c = Coordinator::new(Ledger::default(), config);
        let actions = started(&mut c, stream(Platform::Youtube, "1"), NOW);
        assert!(actions.contains(&Action::PrewarmToken));
        assert_order(&actions);
        c.handle(
            Input::PostFinished(Err(XPostError::new(XPostErrorCode::Uncertain))),
            NOW + 1,
        );
        assert_eq!(c.ledger().entries()[0].state, LedgerState::Uncertain);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 2)), 0);
        let actions = c.handle(Input::UserRetry, NOW + 3);
        assert_order(&actions);
        assert_eq!(external(&actions), 1);
        assert_eq!(external(&c.handle(Input::UserRetry, NOW + 4)), 0);
        c.handle(
            Input::PostFinished(Ok(PostedTweet { id: "tweet".into() })),
            NOW + 5,
        );
        assert_eq!(c.ledger().entries()[0].state, LedgerState::Posted);
        assert_eq!(c.ledger().entries()[0].tweet_id.as_deref(), Some("tweet"));
    }

    #[test]
    fn queued_posts_recheck_daily_limit_before_dispatch() {
        let mut config = settings("intent");
        config.x_post.combine = false;
        config.x_post.daily_limit = 1;
        let mut c = Coordinator::new(Ledger::default(), config);
        started(&mut c, stream(Platform::Youtube, "1"), NOW);
        started(&mut c, stream(Platform::Twitch, "2"), NOW + 1);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 60_000)), 1);
        assert_eq!(external(&c.handle(Input::Tick, NOW + 60_001)), 0);
        assert_eq!(
            c.ledger().get("twitch:2").unwrap().reason.as_deref(),
            Some("dailyLimit")
        );
    }
    #[test]
    fn individual_button_queue_exposes_next_banner_after_open_and_cancel() {
        for cancel in [true, false] {
            let mut config = settings("intent");
            config.x_post.combine = false;
            config.x_post.intent_open = "button".into();
            let mut c = Coordinator::new(Ledger::default(), config);
            started(&mut c, stream(Platform::Youtube, "1"), NOW);
            started(&mut c, stream(Platform::Twitch, "2"), NOW + 1);
            let id = shown(&c);
            let action = if cancel {
                Input::UserCancel { id }
            } else {
                Input::UserApprove { id }
            };
            c.handle(action, NOW + 2);
            assert_eq!(c.status().state, "awaitingApproval");
            assert_eq!(c.status().fire_at_ms, Some(NOW + 60_001));
            assert!(c.status().text.as_ref().unwrap().contains("/2?"));
            assert_eq!(
                external(&c.handle(Input::UserApprove { id: shown(&c) }, NOW + 3)),
                1
            );
        }
    }

    #[test]
    fn youtube_eligibility_matches_supported_channel_forms() {
        for id in [
            "@mine",
            "youtube.com/@mine/live",
            "https://WWW.YOUTUBE.COM/@mine",
            "UC1234567890123456789012",
            "https://youtube.com/channel/UC1234567890123456789012/live",
        ] {
            assert!(youtube_channel(id), "{id}");
        }
        for id in [
            "",
            "@",
            "UCvideo12345",
            "dQw4w9WgXcQ",
            "https://youtube.com/watch?v=id",
            "https://youtube.com/live/id",
            "https://youtube.com/channel/UCinvalid",
            "https://example.com/youtube.com/@mine",
            "https://youtube.com/c/mine",
        ] {
            assert!(!youtube_channel(id), "{id}");
        }
    }
    #[test]
    fn duplicate_check_ignores_text_older_than_the_latest_twenty_entries() {
        let s = stream(Platform::Youtube, "new");
        let mut config = settings("intent");
        config.x_post.delay_secs = 0;
        let text = render(
            &config.x_post.template,
            &[s.clone()],
            local_time(NOW).unwrap(),
        )
        .unwrap()
        .text;
        let mut ledger = Ledger::default();
        seed(
            &mut ledger,
            &stream(Platform::Youtube, "old"),
            LedgerState::Failed,
            NOW - 100,
            Some(&text),
        );
        for index in 0..20 {
            seed(
                &mut ledger,
                &stream(Platform::Youtube, &format!("entry-{index}")),
                LedgerState::Failed,
                NOW - index,
                Some("different text"),
            );
        }
        let mut c = Coordinator::new(ledger, config);
        assert_eq!(external(&started(&mut c, s, NOW)), 1);
        assert_eq!(c.status().text.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn disconnected_api_skips_without_writing_and_without_prewarming() {
        let mut config = settings("api");
        config.connected = false;
        let mut c = Coordinator::new(Ledger::default(), config);
        let actions = started(&mut c, stream(Platform::Youtube, "new"), NOW);
        assert!(!actions.contains(&Action::PersistLedger));
        assert!(!actions.contains(&Action::PrewarmToken));
        assert!(c.ledger().entries().is_empty());
        assert_eq!(c.status().reason.as_deref(), Some("notConnected"));
    }

    #[test]
    fn stale_zero_announces_only_observed_starts() {
        let mut config = settings("intent");
        config.x_post.stale_minutes = 0;
        config.x_post.delay_secs = 0;
        // Already live when observed: skipped.
        let mut c = Coordinator::new(Ledger::default(), config.clone());
        let mut s = stream(Platform::Twitch, "1");
        s.started_at_ms = Some(NOW - 1000);
        assert_eq!(external(&started(&mut c, s, NOW)), 0);
        assert_eq!(c.ledger().get("twitch:1").unwrap().reason.as_deref(), Some("stale"));
        // Ended observed in this run, then Started 1 s after the start time.
        let mut c = Coordinator::new(Ledger::default(), config);
        let mut s = stream(Platform::Twitch, "2");
        s.started_at_ms = Some(NOW - 1000);
        c.handle(
            Input::Live(LiveEvent::Ended {
                platform: s.platform,
                channel_identifier: s.channel_identifier.clone(),
            }),
            NOW - 5000,
        );
        assert_eq!(external(&started(&mut c, s, NOW)), 1);
    }

    #[test]
    fn stale_ids_never_act_on_the_next_announcement() {
        for intent_open in ["button", "auto"] {
            let mut config = settings("intent");
            config.x_post.combine = false;
            config.x_post.intent_open = intent_open.into();
            let mut c = Coordinator::new(Ledger::default(), config);
            started(&mut c, stream(Platform::Youtube, "1"), NOW);
            started(&mut c, stream(Platform::Twitch, "2"), NOW + 1000);
            let first = shown(&c);
            if intent_open == "button" {
                // Double click on approve: the second click must not open #2.
                assert_eq!(external(&c.handle(Input::UserApprove { id: first }, NOW + 2)), 1);
                assert_eq!(external(&c.handle(Input::UserApprove { id: first }, NOW + 3)), 0);
            } else {
                // #1 fires before the cancel for it arrives.
                assert_eq!(external(&c.handle(Input::Tick, NOW + 60_000)), 1);
                c.handle(Input::UserCancel { id: first }, NOW + 60_001);
                assert!(c.ledger().get("twitch:2").is_none());
            }
            assert_ne!(shown(&c), first);
            assert!(c.status().text.as_ref().unwrap().contains("/2?"));
        }
    }

    #[test]
    fn duplicate_suffix_reshortens_title_instead_of_too_long() {
        let mut s = stream(Platform::Youtube, "1");
        s.title = Some("a".repeat(300));
        let mut config = settings("intent");
        config.x_post.delay_secs = 0;
        config.x_post.template = "{title}\n{url}".into();
        let base = render(&config.x_post.template, &[s.clone()], local_time(NOW).unwrap())
            .unwrap()
            .text;
        assert_eq!(weighted_len(&base), 280);
        let mut ledger = Ledger::default();
        seed(&mut ledger, &stream(Platform::Youtube, "old"), LedgerState::Failed, NOW - 1, Some(&base));
        let mut c = Coordinator::new(ledger, config);
        assert_eq!(external(&started(&mut c, s, NOW)), 1);
        let text = c.status().text.clone().unwrap();
        assert_ne!(text, base);
        assert!(weighted_len(&text) <= 280);
        assert!(text.ends_with(&local_time(NOW).unwrap().format(" %H:%M").to_string()));
    }

    #[test]
    fn intent_render_failure_can_be_retried_after_fixing_template() {
        let mut config = settings("intent");
        config.x_post.delay_secs = 0;
        config.x_post.template = "配信開始".into();
        let mut c = Coordinator::new(Ledger::default(), config.clone());
        assert_eq!(external(&started(&mut c, stream(Platform::Youtube, "1"), NOW)), 0);
        assert_eq!(c.status().state, "failed");
        assert_eq!(c.status().reason.as_deref(), Some("MissingUrl"));
        assert!(c.status().can_retry);
        config.x_post.template = "{title}\n{url}".into();
        c.handle(Input::Settings(config), NOW + 1);
        let actions = c.handle(Input::UserRetry, NOW + 2);
        assert_order(&actions);
        assert_eq!(external(&actions), 1);
        assert_eq!(c.ledger().get("youtube:1").unwrap().state, LedgerState::Opened);
        assert!(!c.status().can_retry);
    }

    #[test]
    fn uncertain_restored_from_ledger_offers_no_dead_retry() {
        let s = stream(Platform::Youtube, "1");
        let mut ledger = Ledger::default();
        seed(&mut ledger, &s, LedgerState::Posting, NOW, Some("text"));
        let c = Coordinator::new(ledger, settings("api"));
        assert_eq!(c.status().state, "uncertain");
        assert!(!c.status().can_retry);
    }

    #[test]
    fn open_failure_is_recorded_failed_and_not_counted_toward_limits() {
        let mut config = settings("intent");
        config.x_post.delay_secs = 0;
        let mut c = Coordinator::new(Ledger::default(), config);
        assert_eq!(external(&started(&mut c, stream(Platform::Youtube, "1"), NOW)), 1);
        c.handle(Input::IntentOpenFailed, NOW);
        assert_eq!(c.ledger().get("youtube:1").unwrap().state, LedgerState::Failed);
        assert_eq!(c.status().reason.as_deref(), Some("openFailed"));
        assert!(c.status().can_retry);
        // A later open failure feedback without a dispatch is ignored.
        let before = c.ledger().entries().to_vec();
        c.handle(Input::Tick, NOW + 1);
        c.handle(Input::IntentOpenFailed, NOW + 2);
        assert_eq!(c.ledger().entries(), &before[..]);
        let actions = c.handle(Input::UserRetry, NOW + 3);
        assert_eq!(external(&actions), 1);
        assert_eq!(c.ledger().get("youtube:1").unwrap().state, LedgerState::Opened);
    }
}
