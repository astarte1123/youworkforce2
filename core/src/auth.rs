use rand::Rng;
use sha2::{Sha256, Digest};

/// Demo-grade password hashing (sha256 + random salt). NOTE: for a pilot replace
/// with argon2id. Format: "<salt_hex>:<hash_hex>".
pub fn hash_password(pw: &str) -> String {
    let salt: [u8; 16] = rand::thread_rng().gen();
    let salt_hex = hex::encode(salt);
    let h = hash_with_salt(pw, &salt_hex);
    format!("{salt_hex}:{h}")
}

pub fn verify_password(pw: &str, stored: &str) -> bool {
    let Some((salt_hex, hash_hex)) = stored.split_once(':') else { return false };
    let computed = hash_with_salt(pw, salt_hex);
    // constant-time-ish compare
    computed.as_bytes().ct_eq(hash_hex.as_bytes())
}

fn hash_with_salt(pw: &str, salt_hex: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt_hex.as_bytes());
    h.update(pw.as_bytes());
    hex::encode(h.finalize())
}

pub fn new_token() -> String {
    let b: [u8; 32] = rand::thread_rng().gen();
    hex::encode(b)
}

trait CtEq { fn ct_eq(&self, other: &[u8]) -> bool; }
impl CtEq for [u8] {
    fn ct_eq(&self, other: &[u8]) -> bool {
        if self.len() != other.len() { return false; }
        let mut diff = 0u8;
        for (a, b) in self.iter().zip(other.iter()) { diff |= a ^ b; }
        diff == 0
    }
}
