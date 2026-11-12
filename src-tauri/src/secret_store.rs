//! OS 資格情報ストア(keyring)の共通ヘルパ。
//!
//! 1 エントリに JSON を 1 つ保存する。keyring の操作はブロッキングなので
//! `spawn_blocking` で実行し、同じサービスへの同時アクセスは内部 mutex で直列化する。
//! 秘匿値そのものはログにもエラー文にも出さない。

use std::sync::OnceLock;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::sync::Mutex;

const SERVICE: &str = "fast-comment";

fn keyring_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 保存済みの JSON を読み出す。エントリが無ければ `Ok(None)`。
pub async fn load<T>(user: &str) -> Result<Option<T>, String>
where
    T: DeserializeOwned + Send + 'static,
{
    let user = user.to_string();
    let _guard = keyring_lock().lock().await;
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(SERVICE, &user)
            .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
        match entry.get_password() {
            Ok(value) => serde_json::from_str::<T>(&value)
                .map(Some)
                .map_err(|_| "保存済み認証情報が壊れています".to_string()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(format!("OS資格情報ストアから認証情報を読めませんでした: {e}")),
        }
    })
    .await
    .map_err(|e| format!("OS資格情報ストアの読込タスクに失敗しました: {e}"))?
}

/// JSON にシリアライズして保存する。
pub async fn save<T>(user: &str, value: &T) -> Result<(), String>
where
    T: Serialize,
{
    let encoded = serde_json::to_string(value)
        .map_err(|e| format!("認証情報の保存形式作成に失敗しました: {e}"))?;
    let user = user.to_string();
    let _guard = keyring_lock().lock().await;
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(SERVICE, &user)
            .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
        entry
            .set_password(&encoded)
            .map_err(|e| format!("認証情報をOS資格情報ストアへ保存できませんでした: {e}"))
    })
    .await
    .map_err(|e| format!("OS資格情報ストアの保存タスクに失敗しました: {e}"))?
}

/// 保存済みのエントリを削除する。存在しない場合は成功扱い。
pub async fn delete(user: &str) -> Result<(), String> {
    let user = user.to_string();
    let _guard = keyring_lock().lock().await;
    tokio::task::spawn_blocking(move || {
        let entry = keyring::Entry::new(SERVICE, &user)
            .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("認証情報をOS資格情報ストアから削除できませんでした: {e}")),
        }
    })
    .await
    .map_err(|e| format!("OS資格情報ストアの削除タスクに失敗しました: {e}"))?
}
