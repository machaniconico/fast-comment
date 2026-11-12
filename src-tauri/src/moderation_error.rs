//! 実モデレーション(Twitch / YouTube)共通のエラー型。
//!
//! UI は `message` をそのまま表示し、`code` は「設定を開く」導線や
//! テストの表明にだけ使う。トークンなどの秘匿値は `message` に入れない。

use serde::Serialize;

/// モデレーション操作の失敗理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ModerationErrorCode {
    /// 未接続・認証切れ。
    NotLoggedIn,
    /// ログイン済みだが OAuth スコープが足りない。
    ScopeMissing,
    /// 配信者・モデレーター権限が無い。
    NotModerator,
    /// 配信者・モデレーターなど保護対象への操作。
    ProtectedTarget,
    /// 対象(コメント/ユーザー)が見つからない。
    NotFound,
    /// 機能や配信が利用できない。
    Unavailable,
    /// API のレート制限・quota 枯渇。
    RateLimited,
    /// 既に処理済みなどの競合。
    Conflict,
    /// 通信失敗。
    Network,
    /// 上記以外。
    Other,
}

/// UI に返すモデレーション操作のエラー。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModerationError {
    pub code: ModerationErrorCode,
    pub message: String,
}

impl ModerationError {
    pub fn new(code: ModerationErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ModerationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ModerationError {}
