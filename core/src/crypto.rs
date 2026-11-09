//! Encryption-at-rest dei segreti upstream (password Traccar, gql_pass GoAlert).
//! Chiave simmetrica derivata da YW_SECRET_KEY (qualsiasi passphrase -> sha256).
//! Formato memorizzato: "enc:v1:<hex nonce>:<hex ciphertext>". I valori senza
//! prefisso sono trattati come plaintext (percorso di migrazione retrocompatibile).
use chacha20poly1305::{aead::{Aead, KeyInit}, ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use rand::RngCore;

pub struct Secrets {
    cipher: Option<ChaCha20Poly1305>,
}

impl Secrets {
    /// Inizializza dalla env YW_SECRET_KEY. Se assente: cifratura disattivata
    /// (i segreti restano in chiaro, come prima) ma il sistema resta funzionante.
    pub fn from_env() -> Self {
        match std::env::var("YW_SECRET_KEY") {
            Ok(s) if !s.is_empty() => {
                let digest = Sha256::digest(s.as_bytes());
                let key = Key::from_slice(&digest);
                Self { cipher: Some(ChaCha20Poly1305::new(key)) }
            }
            _ => Self { cipher: None },
        }
    }

    pub fn enabled(&self) -> bool { self.cipher.is_some() }

    /// Cifra un valore. Senza chiave o su stringa vuota ritorna il valore invariato.
    pub fn encrypt(&self, plain: &str) -> String {
        let Some(c) = &self.cipher else { return plain.to_string() };
        if plain.is_empty() || plain.starts_with("enc:v1:") { return plain.to_string(); }
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        match c.encrypt(Nonce::from_slice(&nonce), plain.as_bytes()) {
            Ok(ct) => format!("enc:v1:{}:{}", hex::encode(nonce), hex::encode(ct)),
            Err(_) => plain.to_string(),
        }
    }

    /// Decifra un valore cifrato ("enc:v1:..."). Valori senza prefisso (plaintext
    /// legacy) sono ritornati invariati. Fallimento decifratura -> valore grezzo.
    pub fn decrypt(&self, stored: &str) -> String {
        let Some(rest) = stored.strip_prefix("enc:v1:") else { return stored.to_string() };
        let Some(c) = &self.cipher else { return stored.to_string() };
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
