use base64::Engine;
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use subtle::ConstantTimeEq;

#[derive(Clone, Default)]
pub struct ApiGate(Arc<RwLock<GateState>>);
#[derive(Default)]
struct GateState {
    digest: Option<[u8; 32]>,
    session: tokio_util::sync::CancellationToken,
    transition: Option<Box<dyn Fn(bool) + Send + Sync>>,
}
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
        let mut state = self.0.write().unwrap();
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        if state
            .digest
            .as_ref()
            .is_some_and(|stored| bool::from(stored.ct_eq(&digest)))
        {
            return;
        }
        state.session.cancel();
        state.session = Default::default();
        let was_open = state.digest.is_some();
        state.digest = Some(digest);
        if !was_open {
            if let Some(transition) = &state.transition {
                transition(true);
            }
        }
    }
    pub fn close(&self) {
        let mut state = self.0.write().unwrap();
        state.session.cancel();
        state.digest = None;
        if let Some(transition) = &state.transition {
            transition(false);
        }
    }
    pub fn is_open(&self) -> bool {
        self.0.read().unwrap().digest.is_some()
    }
    // Install once during setup; synchronously subscribe/unsubscribe at the gate boundary.
    pub(crate) fn on_transition(&self, transition: impl Fn(bool) + Send + Sync + 'static) {
        let mut state = self.0.write().unwrap();
        transition(state.digest.is_some());
        state.transition = Some(Box::new(transition));
    }
    pub fn verify(&self, presented: &str) -> bool {
        self.authenticate(presented).is_some()
    }
    pub(crate) fn authenticate(
        &self,
        presented: &str,
    ) -> Option<tokio_util::sync::CancellationToken> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let state = self.0.read().unwrap();
        state
            .digest
            .as_ref()
            .filter(|stored| bool::from(stored.ct_eq(&digest)))
            .map(|_| state.session.clone())
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
