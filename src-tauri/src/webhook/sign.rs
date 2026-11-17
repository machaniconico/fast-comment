use hmac::{Hmac, Mac};
use sha2::Sha256;
pub fn signature_header(secret: &[u8], timestamp_secs: u64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(format!("{timestamp_secs}.").as_bytes());
    mac.update(body);
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}
