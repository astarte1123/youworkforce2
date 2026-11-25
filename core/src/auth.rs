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

// ---------------- SA-12: TOTP (RFC 6238, HMAC-SHA1, 6 cifre, 30 s) ----------------
const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new(); let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data { buf = (buf << 8) | b as u32; bits += 8;
        while bits >= 5 { out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char); bits -= 5; } }
    if bits > 0 { out.push(B32[((buf << (5 - bits)) & 31) as usize] as char); }
    out
}
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new(); let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = B32.iter().position(|x| *x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v; bits += 5;
        if bits >= 8 { out.push(((buf >> (bits - 8)) & 0xff) as u8); bits -= 8; }
    }
    Some(out)
}
/// Nuovo segreto TOTP (160 bit) in base32.
pub fn totp_new_secret() -> String { let b: [u8; 20] = rand::thread_rng().gen(); base32_encode(&b) }
/// Codice TOTP per l'intervallo `step` (= unix_time / 30).
pub fn totp_code(secret: &[u8], step: i64) -> u32 {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha1::Sha1> as Mac>::new_from_slice(secret).expect("hmac");
    mac.update(&(step as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let bin = ((h[o] as u32 & 0x7f) << 24) | ((h[o+1] as u32) << 16) | ((h[o+2] as u32) << 8) | h[o+3] as u32;
    bin % 1_000_000
}
/// Verifica un codice (tolleranza ±1 intervallo). Ritorna l'intervallo usato, per l'anti-replay.
pub fn totp_verify(secret_b32: &str, code: &str, unix: i64, last_step: Option<i64>) -> Option<i64> {
    let code = code.trim().replace(' ', "");
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) { return None; }
    let want: u32 = code.parse().ok()?;
    let secret = base32_decode(secret_b32)?;
    let now = unix.div_euclid(30);
    for st in [now - 1, now, now + 1] {
        if last_step.map(|l| st <= l).unwrap_or(false) { continue; }
        if totp_code(&secret, st) == want { return Some(st); }
    }
    None
}

#[cfg(test)]
mod totp_tests {
    use super::*;
    #[test]
    fn rfc6238_vector() {
        // RFC 6238, appendice B (SHA1, segreto "12345678901234567890"): T=59 -> 94287082 (8 cifre)
        let k = b"12345678901234567890";
        assert_eq!(totp_code(k, 59 / 30), 94287082 % 1_000_000);
        assert_eq!(totp_code(k, 1111111109 / 30), 7081804 % 1_000_000);
    }
    #[test]
    fn base32_roundtrip_and_verify() {
        let s = totp_new_secret();
        let raw = base32_decode(&s).unwrap();
        assert_eq!(base32_encode(&raw), s);
        let code = format!("{:06}", totp_code(&raw, 1000));
        assert_eq!(totp_verify(&s, &code, 1000 * 30 + 5, None), Some(1000));
        assert_eq!(totp_verify(&s, &code, 1000 * 30 + 5, Some(1000)), None, "stesso codice due volte");
        assert_eq!(totp_verify(&s, "12345", 0, None), None);
    }
}
