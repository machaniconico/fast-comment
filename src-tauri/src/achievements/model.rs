use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Platform;

pub use super::presets::presets;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Tier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubjectKind {
    Viewer,
    Streamer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Window {
    Lifetime,
    PerStream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Metric {
    Comments,
    SuperChatCount,
    DonationAmount,
    GiftCount,
    MembershipEvents,
    StreamsAttended,
    UniqueCommenters,
    PeakConcurrentViewers,
    Streams,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "code", rename_all = "camelCase")]
pub enum CurrencyMode {
    None,
    JpyConverted,
    Only(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    pub window: Window,
    pub metric: Metric,
    pub threshold: f64,
    pub currency: CurrencyMode,
    pub platforms: Vec<Platform>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Announce {
    pub tts: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Achievement {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub tier: Tier,
    pub subject: SubjectKind,
    pub enabled: bool,
    pub builtin_id: Option<String>,
    pub edited: bool,
    pub condition: Option<Condition>,
    pub announce: Announce,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationError {
    EmptyName,
    DuplicateId,
    AmountMetricNeedsCurrency,
    CountMetricRejectsCurrency,
    MetricSubjectMismatch,
    WindowNotAllowed,
    BadThreshold,
    PlatinumHasCondition,
    MissingCondition,
    MultiplePlatinum,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyName => "achievement name is empty",
            Self::DuplicateId => "duplicate achievement ID",
            Self::AmountMetricNeedsCurrency => "amount metric requires a currency",
            Self::CountMetricRejectsCurrency => "count metric does not accept a currency",
            Self::MetricSubjectMismatch => "metric is unavailable for this subject",
            Self::WindowNotAllowed => "metric is unavailable for this window",
            Self::BadThreshold => "threshold must be finite and positive",
            Self::PlatinumHasCondition => "platinum achievement cannot have a condition",
            Self::MissingCondition => "achievement requires a condition",
            Self::MultiplePlatinum => "subject already has a platinum achievement",
        })
    }
}

impl std::error::Error for ValidationError {}

pub fn validate_all(list: &[Achievement]) -> Result<(), (String, ValidationError)> {
    let mut ids = HashSet::new();
    let mut platinum_subjects = HashSet::new();
    for a in list {
        let error = |reason| (a.id.clone(), reason);
        if a.name.trim().is_empty() {
            return Err(error(ValidationError::EmptyName));
        }
        if !ids.insert(a.id.as_str()) {
            return Err(error(ValidationError::DuplicateId));
        }
        if a.tier == Tier::Platinum {
            if a.condition.is_some() {
                return Err(error(ValidationError::PlatinumHasCondition));
            }
            if !platinum_subjects.insert(a.subject) {
                return Err(error(ValidationError::MultiplePlatinum));
            }
        } else {
            let condition = a
                .condition
                .as_ref()
                .ok_or_else(|| error(ValidationError::MissingCondition))?;
            validate_condition(condition, a.subject).map_err(error)?;
        }
    }
    Ok(())
}

fn validate_condition(condition: &Condition, subject: SubjectKind) -> Result<(), ValidationError> {
    use Metric::*;

    match (condition.metric, &condition.currency) {
        (DonationAmount, CurrencyMode::None) => {
            return Err(ValidationError::AmountMetricNeedsCurrency);
        }
        (DonationAmount, CurrencyMode::Only(code)) if code.trim().is_empty() => {
            return Err(ValidationError::AmountMetricNeedsCurrency);
        }
        (DonationAmount, _) | (_, CurrencyMode::None) => {}
        _ => return Err(ValidationError::CountMetricRejectsCurrency),
    }
    match (condition.metric, subject) {
        (UniqueCommenters | PeakConcurrentViewers | Streams, SubjectKind::Viewer)
        | (StreamsAttended, SubjectKind::Streamer) => {
            return Err(ValidationError::MetricSubjectMismatch);
        }
        _ => {}
    }
    if matches!(condition.metric, UniqueCommenters | PeakConcurrentViewers)
        && condition.window != Window::PerStream
    {
        return Err(ValidationError::WindowNotAllowed);
    }
    if !condition.threshold.is_finite() || condition.threshold <= 0.0 {
        return Err(ValidationError::BadThreshold);
    }
    Ok(())
}

pub fn duplicate(src: &Achievement, new_id: &str) -> Achievement {
    Achievement {
        id: new_id.to_owned(),
        builtin_id: None,
        edited: false,
        ..src.clone()
    }
}

pub fn reset_to_preset(a: &Achievement) -> Option<Achievement> {
    let builtin_id = a.builtin_id.as_ref()?;
    let preset = presets()
        .into_iter()
        .find(|preset| preset.builtin_id.as_ref() == Some(builtin_id))?;
    Some(Achievement {
        id: a.id.clone(),
        enabled: a.enabled,
        ..preset
    })
}
