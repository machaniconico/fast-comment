#[derive(Debug, Clone, thiserror::Error)]
#[error("Webhook URL の形式が正しくありません")]
pub struct UrlError;
#[derive(Debug, Clone, thiserror::Error)]
#[error("Webhook の送信に失敗しました")]
pub struct DeliveryError;
