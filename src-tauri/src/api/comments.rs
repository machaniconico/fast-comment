use super::{
    error,
    feed::{Entry, History},
    ApiDeps,
};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(super) struct Params {
    pub after: Option<String>,
    pub limit: Option<String>,
    pub channel: Option<String>,
    pub kind: Option<String>,
}
pub(super) fn invalid(field: &str) -> Response {
    error::response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("{field} が不正です"),
    )
}
pub(super) fn cursor(value: &str) -> Option<(&str, u64)> {
    let (epoch, seq) = value.split_once('.')?;
    if epoch.len() != 12
        || !epoch.bytes().all(|b| b.is_ascii_hexdigit())
        || seq.is_empty()
        || !seq.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((epoch, seq.parse().ok()?))
}
impl Params {
    pub fn validate(&self) -> Result<usize, &'static str> {
        let limit = self.limit.as_deref().unwrap_or("100");
        if limit.is_empty() || !limit.bytes().all(|b| b.is_ascii_digit()) {
            return Err("limit");
        }
        let limit = limit.parse::<usize>().map_err(|_| "limit")?;
        if !(1..=500).contains(&limit) {
            return Err("limit");
        }
        if self.after.as_deref().is_some_and(|v| cursor(v).is_none()) {
            return Err("after");
        }
        if self.kind.as_deref().is_some_and(|k| {
            k.split(',').any(|k| {
                ![
                    "normal",
                    "superChat",
                    "membership",
                    "bits",
                    "gift",
                    "system",
                ]
                .contains(&k)
            })
        }) {
            return Err("kind");
        }
        Ok(limit)
    }
    pub fn matches(&self, entry: &Entry) -> bool {
        self.channel
            .as_ref()
            .is_none_or(|ch| entry.value["channel"] == *ch)
            && self
                .kind
                .as_ref()
                .is_none_or(|k| k.split(',').any(|k| entry.value["kind"] == k))
    }
}
pub(super) fn valid_after(after: Option<&str>, history: &History, epoch: &str) -> bool {
    after.is_none_or(|value| {
        cursor(value).is_some_and(|(previous_epoch, seq)| {
            previous_epoch != epoch || seq <= history.latest_seq
        })
    })
}
pub(super) fn select(
    history: History,
    after: Option<&str>,
    epoch: &str,
) -> (Vec<Arc<Entry>>, bool, u64) {
    let Some((previous_epoch, seq)) = after.and_then(cursor) else {
        return (history.entries, false, 0);
    };
    if previous_epoch != epoch {
        return (history.entries, true, 0);
    }
    let dropped = history.discarded_through.saturating_sub(seq);
    (
        history
            .entries
            .into_iter()
            .filter(|e| e.seq > seq)
            .collect(),
        dropped > 0,
        dropped,
    )
}
pub async fn comments(
    State(deps): State<ApiDeps>,
    params: Result<Query<Params>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return invalid("クエリ");
    };
    let limit = match params.validate() {
        Ok(limit) => limit,
        Err(field) => return invalid(field),
    };
    let history = deps.feed.snapshot();
    if !valid_after(params.after.as_deref(), &history, &deps.epoch) {
        return invalid("after");
    }
    let (entries, gap, _) = select(history, params.after.as_deref(), &deps.epoch);
    let mut entries: Vec<_> = entries.into_iter().filter(|e| params.matches(e)).collect();
    let tail = params
        .after
        .as_deref()
        .and_then(cursor)
        .is_none_or(|(epoch, _)| epoch != deps.epoch);
    if tail {
        entries = entries.split_off(entries.len().saturating_sub(limit));
    } else {
        entries.truncate(limit);
    }
    let next = entries
        .last()
        .map(|e| format!("{}.{}", deps.epoch, e.seq))
        .or_else(|| params.after.clone());
    let items: Vec<Value> = entries.iter().map(|e| e.value.clone()).collect();
    Json(json!({"items":items,"nextCursor":next,"gap":gap})).into_response()
}
