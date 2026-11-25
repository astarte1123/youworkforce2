//! Encryption-at-rest dei segreti (chiave del provider AI).
//! IO-13 (25/11/2026): chiave simmetrica derivata da YW_SECRET_KEY con **Argon2id** (formato
//! "enc:v2:<hex nonce>:<hex ciphertext>"). Il vecchio formato "enc:v1:" (chiave = sha256 della
//! passphrase, senza KDF) si legge ancora e viene ricifrato in v2 all'avvio (crypto_migrate).
//! I valori senza prefisso sono plaintext legacy (ricifrati all'avvio).
use chacha20poly1305::{aead::{Aead, KeyInit}, ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use rand::RngCore;

pub struct Secrets {
    cipher: Option<ChaCha20Poly1305>,
    legacy: Option<ChaCha20Poly1305>,
}
/// Lunghezza minima della passphrase YW_SECRET_KEY.
pub const MIN_KEY_LEN: usize = 16;

impl Secrets {
    /// Inizializza dalla env YW_SECRET_KEY. Se assente: cifratura disattivata
    /// (i segreti restano in chiaro, come prima) ma il sistema resta funzionante.
    pub fn from_env() -> Self {
        match std::env::var("YW_SECRET_KEY") {
            Ok(s) if !s.is_empty() => {
                let digest = Sha256::digest(s.as_bytes());
                let legacy = ChaCha20Poly1305::new(Key::from_slice(&digest));
                // Argon2id con sale di dominio fisso: la passphrase non è usata come chiave diretta
                let mut out = [0u8; 32];
                let a2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13,
                    argon2::Params::new(19 * 1024, 2, 1, Some(32)).expect("parametri argon2"));
                a2.hash_password_into(s.as_bytes(), b"youworkforce/secrets/v2", &mut out).expect("derivazione chiave");
                Self { cipher: Some(ChaCha20Poly1305::new(Key::from_slice(&out))), legacy: Some(legacy) }
            }
            _ => Self { cipher: None, legacy: None },
        }
    }

    pub fn enabled(&self) -> bool { self.cipher.is_some() }

    /// Cifra un valore. Senza chiave o su stringa vuota ritorna il valore invariato.
    pub fn encrypt(&self, plain: &str) -> String {
        let Some(c) = &self.cipher else { return plain.to_string() };
        if plain.is_empty() || plain.starts_with("enc:v1:") || plain.starts_with("enc:v2:") { return plain.to_string(); }
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        match c.encrypt(Nonce::from_slice(&nonce), plain.as_bytes()) {
            Ok(ct) => format!("enc:v2:{}:{}", hex::encode(nonce), hex::encode(ct)),
            Err(_) => plain.to_string(),
        }
    }

    /// Decifra un valore cifrato ("enc:v1:..."). Valori senza prefisso (plaintext
    /// legacy) sono ritornati invariati. Fallimento decifratura -> valore grezzo.
    pub fn decrypt(&self, stored: &str) -> String {
        let (rest, c) = if let Some(r) = stored.strip_prefix("enc:v2:") { (r, &self.cipher) }
            else if let Some(r) = stored.strip_prefix("enc:v1:") { (r, &self.legacy) }
            else { return stored.to_string() };
        let Some(c) = c else { return stored.to_string() };
        let mut parts = rest.splitn(2, ':');
        let (Some(nh), Some(ch)) = (parts.next(), parts.next()) else { return stored.to_string() };
        let (Ok(nonce), Ok(ct)) = (hex::decode(nh), hex::decode(ch)) else { return stored.to_string() };
        if nonce.len() != 12 { return stored.to_string(); }
        match c.decrypt(Nonce::from_slice(&nonce), ct.as_ref()) {
            Ok(pt) => String::from_utf8_lossy(&pt).to_string(),
            Err(_) => stored.to_string(),
        }
    }
}
