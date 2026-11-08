use rand::Rng;
use sha2::{Sha256, Digest};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, PasswordHash, rand_core::OsRng};

/// Password hashing: argon2id (PHC string). Verification also accepts the legacy
/// demo format ("<salt_hex>:<sha256_hex>") so pre-existing seeded users keep working
/// and are transparently upgradeable on next set.
pub fn hash_password(pw: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

pub fn verify_password(pw: &str, stored: &str) -> bool {
    if stored.starts_with("$argon2") {
        match PasswordHash::new(stored) {
            Ok(ph) => Argon2::default().verify_password(pw.as_bytes(), &ph).is_ok(),
            Err(_) => false,
        }
    } else {
        // legacy demo format: "<salt_hex>:<sha256_hex>"
        let Some((salt_hex, hash_hex)) = stored.split_once(':') else { return false };
        let computed = legacy_hash_with_salt(pw, salt_hex);
        computed.as_bytes().ct_eq(hash_hex.as_bytes())
    }
}

/// True when a stored hash is in the legacy format and should be re-hashed with
/// argon2id after a successful login (opportunistic upgrade).
pub fn needs_rehash(stored: &str) -> bool {
    !stored.starts_with("$argon2")
}

fn legacy_hash_with_salt(pw: &str, salt_hex: &str) -> String {
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
