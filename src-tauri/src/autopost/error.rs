//! Public errors contain only fixed, safe messages, never remote responses or credentials.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum XPostErrorCode {
    MissingUrl,
    TooLong,
    PortInUse,
    LoginTimeout,
    Cancelled,
    NotLoggedIn,
    ReloginRequired,
    TokenStore,
    Network,
    Duplicate,
    Forbidden,
    InsufficientCredits,
    RateLimited,
    Uncertain,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XPostError {
    pub code: XPostErrorCode,
    pub message: String,
}

impl XPostError {
    /// Do not accept arbitrary error text: HTTP errors can contain credentials.
    pub fn new(code: XPostErrorCode) -> Self {
        let message = match code {
            XPostErrorCode::MissingUrl => "テンプレートに {url} が必要です。",
            XPostErrorCode::TooLong => {
                "固定文面と URL が長すぎます。テンプレートを短くしてください。"
            }
            XPostErrorCode::PortInUse => {
                "認証ポートを使用できません。ポートと X のコールバック設定を確認してください。"
            }
            XPostErrorCode::LoginTimeout => "ログインがタイムアウトしました。",
            XPostErrorCode::Cancelled => "操作を取り消しました。",
            XPostErrorCode::NotLoggedIn => "X に接続してください。",
            XPostErrorCode::ReloginRequired => "X に再ログインしてください。",
            XPostErrorCode::TokenStore => "認証情報を保存または読み込みできませんでした。",
            XPostErrorCode::Network => "X に接続できませんでした。",
            XPostErrorCode::Duplicate => {
                "同じ文面が記録されています。テンプレートを見直してください。"
            }
            XPostErrorCode::Forbidden => {
                "投稿が許可されませんでした。アプリの権限を確認してください。"
            }
            XPostErrorCode::InsufficientCredits => {
                "X Developer Console のクレジットを確認してください。"
            }
            XPostErrorCode::RateLimited => "X の利用上限に達しました。",
            XPostErrorCode::Uncertain => "投稿されたか不明です。X を確認してください。",
            XPostErrorCode::Other => "X との連携に失敗しました。",
        };
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for XPostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for XPostError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_serializable_safe_messages() {
        let error = XPostError::new(XPostErrorCode::TooLong);
        let json = serde_json::to_value(&error).unwrap();
        assert_eq!(json["code"], "tooLong");
        assert_eq!(json["message"], error.to_string());
        assert!(!format!("{error:?}").contains("Bearer"));
    }
}
