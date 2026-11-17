use crate::integrations::WebhookFormat;
use std::time::Duration;
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Status {
        code: u16,
        header_retry_after: Option<Duration>,
        body_retry_after: Option<Duration>,
        remaining_zero_reset_after: Option<Duration>,
    },
    ConnectFailed,
    TimedOutAfterSend,
    ReadFailed,
    ReadFailedWithRateLimit {
        header_retry_after: Option<Duration>,
        remaining_zero_reset_after: Option<Duration>,
    },
}
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Delivered,
    Retry { wait: Option<Duration> },
    Drop { uncertain: bool },
    DropAndPause { reason: &'static str },
}
pub fn classify(format: WebhookFormat, outcome: &Outcome) -> Decision {
    match outcome {
        Outcome::ConnectFailed => Decision::Retry { wait: None },
        Outcome::TimedOutAfterSend
        | Outcome::ReadFailed
        | Outcome::ReadFailedWithRateLimit { .. } => {
            if format == WebhookFormat::Discord {
                Decision::Drop { uncertain: true }
            } else {
                Decision::Retry {
                    wait: match outcome {
                        Outcome::ReadFailedWithRateLimit {
                            header_retry_after, ..
                        } => *header_retry_after,
                        _ => None,
                    },
                }
            }
        }
        Outcome::Status {
            code,
            header_retry_after,
            body_retry_after,
            ..
        } => match code {
            200..=299 => Decision::Delivered,
            300..=399 => Decision::DropAndPause {
                reason: "redirected",
            },
            401 | 403 => Decision::DropAndPause {
                reason: "unauthorized",
            },
            404 | 410 => Decision::DropAndPause { reason: "notFound" },
            429 => Decision::Retry {
                wait: if format == WebhookFormat::Discord {
                    (*header_retry_after)
                        .max(*body_retry_after)
                        .map(|d| d.saturating_add(Duration::from_millis(250)))
                } else {
                    *header_retry_after
                },
            },
            408 | 425 | 500..=599 => Decision::Retry { wait: None },
            _ => Decision::Drop { uncertain: false },
        },
    }
}
