use super::{comments, error, feed::Item, ApiDeps};
use crate::model::ModerationEvent;
use axum::{
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
};
use futures_util::stream;
use serde::Deserialize;
use serde_json::json;
use std::{collections::VecDeque, convert::Infallible, time::Duration};
use tokio::time::{Instant, MissedTickBehavior};

#[derive(Default, Deserialize)]
pub(super) struct Params {
    after: Option<String>,
}

pub async fn events(
    State(deps): State<ApiDeps>,
    headers: HeaderMap,
    Extension(session): Extension<tokio_util::sync::CancellationToken>,
    params: Result<Query<Params>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(mut params)) = params else {
        return comments::invalid("クエリ");
    };
    if let Some(header) = headers.get("last-event-id") {
        let Ok(value) = header.to_str() else {
            return comments::invalid("Last-Event-ID");
        };
        if !value.is_empty() {
            params.after = Some(value.to_owned());
        }
    }
    if !comments::valid_after(params.after.as_deref(), &deps.feed.snapshot(), &deps.epoch) {
        return comments::invalid("after / Last-Event-ID");
    }
    let Some((history, pending)) = deps.feed.subscribe() else {
        return error::response(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_streams",
            "同時接続数の上限です",
        );
    };
    let mut output = VecDeque::new();
    if params.after.is_some() {
        let (history, gap, dropped) =
            comments::select(history, params.after.as_deref(), &deps.epoch);
        if gap {
            output.push_back(lagged(dropped));
        }
        for chunk in history.chunks(500) {
            output.push_back(chat(&deps.epoch, chunk));
        }
    }
    let mut ticker = tokio::time::interval_at(
        Instant::now() + Duration::from_millis(16),
        Duration::from_millis(16),
    );
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let stream = stream::unfold(
        (
            deps,
            session,
            pending,
            params,
            ticker,
            output,
            Instant::now(),
        ),
        |(deps, session, pending, params, mut ticker, mut output, mut last_sent)| async move {
            loop {
                if session.is_cancelled() || deps.shutdown.is_cancelled() {
                    return None;
                }
                if let Some(event) = output.pop_front() {
                    last_sent = Instant::now();
                    return Some((
                        Ok::<_, Infallible>(event),
                        (deps, session, pending, params, ticker, output, last_sent),
                    ));
                }
                tokio::select! {
                    biased;
                    _ = session.cancelled() => return None,
                    _ = deps.shutdown.cancelled() => return None,
                    _ = ticker.tick() => {}
                }
                {
                    let mut batch = pending.lock().unwrap();
                    if batch.dropped > 0 {
                        output.push_back(lagged(std::mem::take(&mut batch.dropped)));
                    }
                    // Per frame: chat, then moderation. A chat that a queued clear
                    // would match arrived after it, so it starts a new chat/moderation pair.
                    let cursor = params.after.as_deref().and_then(comments::cursor);
                    let mut entries = Vec::new();
                    let mut events = Vec::new();
                    for item in batch.items.drain(..) {
                        match item {
                            Item::Chat(entry) => {
                                if events
                                    .iter()
                                    .any(|e: &ModerationEvent| e.matches(&entry.target))
                                {
                                    flush(&mut output, &deps.epoch, &mut entries, &mut events);
                                }
                                if cursor.is_none_or(|(epoch, seq)| {
                                    epoch != deps.epoch || entry.seq > seq
                                }) {
                                    entries.push(entry);
                                }
                            }
                            Item::Moderation(event) => events.push(event),
                            Item::Achievement(value) => {
                                flush(&mut output, &deps.epoch, &mut entries, &mut events);
                                output.push_back(
                                    Event::default().event("achievement").data(value.to_string()),
                                );
                            }
                        }
                    }
                    flush(&mut output, &deps.epoch, &mut entries, &mut events);
                }
                if output.is_empty() && last_sent.elapsed() >= Duration::from_secs(15) {
                    output.push_back(Event::default().comment("ping"));
                }
            }
        },
    );
    Sse::new(stream).into_response()
}
fn lagged(dropped: u64) -> Event {
    Event::default()
        .event("lagged")
        .data(json!({"dropped":dropped}).to_string())
}
fn flush(
    output: &mut VecDeque<Event>,
    epoch: &str,
    entries: &mut Vec<std::sync::Arc<super::feed::Entry>>,
    events: &mut Vec<ModerationEvent>,
) {
    if !entries.is_empty() {
        output.push_back(chat(epoch, &std::mem::take(entries)));
    }
    if !events.is_empty() {
        let items = std::mem::take(events);
        output.push_back(
            Event::default()
                .event("moderation")
                .data(json!({"items":items}).to_string()),
        );
    }
}
fn chat(epoch: &str, entries: &[std::sync::Arc<super::feed::Entry>]) -> Event {
    let items: Vec<_> = entries.iter().map(|e| &e.value).collect();
    Event::default()
        .event("chat")
        .id(format!("{epoch}.{}", entries.last().unwrap().seq))
        .data(json!({"items":items}).to_string())
}
