//! Tauri に依存しない Webhook 判定・配送。
pub mod delivery;
pub mod error;
pub mod payload;
pub mod rules;
pub mod runtime;
pub mod sender;
pub mod sign;
pub mod target;
pub use runtime::*;
pub use sender::*;
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    Donation,
    Membership,
    Keyword,
    Test,
}
#[derive(Clone, Debug)]
pub struct Matched {
    pub trigger: Trigger,
    pub keyword: Option<String>,
}
