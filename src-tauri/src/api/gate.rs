use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use subtle::ConstantTimeEq;

#[derive(Clone, Default)]
pub struct ApiGate(Arc<RwLock<Option<[u8; 32]>>>);
impl std::fmt::Debug for ApiGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiGate([redacted])")
    }
}
impl ApiGate {
    pub fn closed() -> Self {
        Self::default()
    }
    pub fn open(&self, token: &str) {
        *self.0.write().unwrap() = Some(Sha256::digest(token.as_bytes()).into());
    }
    pub fn close(&self) {
        *self.0.write().unwrap() = None;
    }
    pub fn is_open(&self) -> bool {
        self.0.read().unwrap().is_some()
    }
    pub fn verify(&self, presented: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        self.0
            .read()
            .unwrap()
            .as_ref()
            .is_some_and(|stored| bool::from(stored.ct_eq(&digest)))
    }
}
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("OS random source unavailable");
    format!(
        "fc_{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}
