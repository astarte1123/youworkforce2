//! Encryption-at-rest dei segreti (chiavi dei provider AI).
//!
//! Decisione PO (25/11/2026): la chiave NON sta in un file di testo o in una variabile d'ambiente;
//! la genera e la custodisce YouWorkforce, la gestisce il superadmin dalla UI (scheda Sistema).
//! - La chiave dei dati (DEK, 32 byte casuali) è in `app.secret_keys`, mai in chiaro:
//!   * sigillata dal SERVER (`seal_server`): Windows = DPAPI di macchina; altri sistemi = chiave di
//!     macchina generata dal core in `YW_KEYSTORE_DIR` (file con permessi 0600, mai da scrivere a mano);
//!   * sigillata con la PASSPHRASE DI RECUPERO del superadmin (Argon2id + sale casuale), per
//!     sbloccarla su un altro server dopo un ripristino o una migrazione.
//! - Formato dei segreti: "enc:v3:<key_id>:<hex nonce>:<hex ct>". I formati precedenti si leggono
//!   ancora per la migrazione: "enc:v2" (Argon2id della vecchia YW_SECRET_KEY), "enc:v1" (sha256
//!   della vecchia YW_SECRET_KEY) e testo in chiaro; all'avvio vengono ricifrati in v3.
use chacha20poly1305::{aead::{Aead, KeyInit}, ChaCha20Poly1305, Key, Nonce};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Default)]
struct Inner {
    keys: HashMap<i32, ChaCha20Poly1305>,
    active: Option<i32>,
    legacy_v2: Option<ChaCha20Poly1305>,
    legacy_v1: Option<ChaCha20Poly1305>,
    /// la chiave attiva esiste nel DB ma questo server non riesce ad aprirla (serve la passphrase)
    locked: bool,
}
pub struct Secrets { inner: RwLock<Inner> }

/// Lunghezza minima della passphrase di recupero.
pub const MIN_PASSPHRASE: usize = 12;

pub fn cipher(key: &[u8]) -> ChaCha20Poly1305 { ChaCha20Poly1305::new(Key::from_slice(key)) }
pub fn random_key() -> [u8; 32] { let mut k = [0u8; 32]; rand::thread_rng().fill_bytes(&mut k); k }
fn seal_with(c: &ChaCha20Poly1305, plain: &[u8]) -> Result<Vec<u8>, String> {
    let mut nonce = [0u8; 12]; rand::thread_rng().fill_bytes(&mut nonce);
    let ct = c.encrypt(Nonce::from_slice(&nonce), plain).map_err(|_| "cifratura fallita".to_string())?;
    let mut out = nonce.to_vec(); out.extend(ct); Ok(out)
}
fn open_with(c: &ChaCha20Poly1305, blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < 13 { return Err("dato sigillato non valido".into()); }
    c.decrypt(Nonce::from_slice(&blob[..12]), &blob[12..]).map_err(|_| "chiave o passphrase errata".to_string())
}
/// Argon2id della passphrase di recupero (sale casuale, 32 byte).
pub fn recovery_kek(passphrase: &str, salt: &[u8]) -> Result<ChaCha20Poly1305, String> {
    let mut out = [0u8; 32];
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13,
        argon2::Params::new(19 * 1024, 2, 1, Some(32)).map_err(|e| e.to_string())?)
        .hash_password_into(passphrase.as_bytes(), salt, &mut out).map_err(|e| e.to_string())?;
    Ok(cipher(&out))
}
pub fn wrap_recovery(dek: &[u8], passphrase: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut salt = vec![0u8; 16]; rand::thread_rng().fill_bytes(&mut salt);
    let w = seal_with(&recovery_kek(passphrase, &salt)?, dek)?;
    Ok((salt, w))
}
pub fn unwrap_recovery(salt: &[u8], wrapped: &[u8], passphrase: &str) -> Result<Vec<u8>, String> {
    open_with(&recovery_kek(passphrase, salt)?, wrapped)
}

// ---- sigillo del server ----
#[cfg(windows)]
mod sealing {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
        CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN};
    const ENTROPY: &[u8] = b"youworkforce/secrets/server-seal";
    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB { CRYPT_INTEGER_BLOB { cbData: b.len() as u32, pbData: b.as_ptr() as *mut u8 } }
    fn call(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let input = blob(data); let ent = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: puntatori validi per la durata della chiamata; l'output è liberato con LocalFree
        let ok = unsafe {
            if protect { CryptProtectData(&input, std::ptr::null(), &ent, std::ptr::null(), std::ptr::null(),
                CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN, &mut out) }
            else { CryptUnprotectData(&input, std::ptr::null_mut(), &ent, std::ptr::null(), std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN, &mut out) }
        };
        if ok == 0 { return Err("DPAPI: sigillo del server non apribile su questa macchina".into()); }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _); }
        Ok(v)
    }
    pub fn seal(d: &[u8]) -> Result<Vec<u8>, String> { call(d, true) }
    pub fn open(d: &[u8]) -> Result<Vec<u8>, String> { call(d, false) }
    pub fn method() -> &'static str { "DPAPI di Windows (chiave di macchina)" }
}
#[cfg(not(windows))]
mod sealing {
    use std::io::Write;
    fn dir() -> std::path::PathBuf {
        std::env::var("YW_KEYSTORE_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| "/var/lib/youworkforce".into())
    }
    /// Chiave di macchina: generata dal core al primo avvio, file 0600, mai da scrivere a mano.
    fn machine_key() -> Result<[u8; 32], String> {
        let p = dir().join("server-seal.key");
        if let Ok(b) = std::fs::read(&p) { if b.len() == 32 { let mut k = [0u8; 32]; k.copy_from_slice(&b); return Ok(k); } }
        std::fs::create_dir_all(dir()).map_err(|e| format!("keystore {}: {e}", dir().display()))?;
        let k = super::random_key();
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p).map_err(|e| format!("keystore {}: {e}", p.display()))?;
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)); }
        f.write_all(&k).map_err(|e| e.to_string())?;
        Ok(k)
    }
    pub fn seal(d: &[u8]) -> Result<Vec<u8>, String> { super::seal_with(&super::cipher(&machine_key()?), d) }
    pub fn open(d: &[u8]) -> Result<Vec<u8>, String> { super::open_with(&super::cipher(&machine_key()?), d) }
    pub fn method() -> &'static str { "chiave di macchina del server (keystore locale protetto)" }
}
pub fn seal_server(d: &[u8]) -> Result<Vec<u8>, String> { sealing::seal(d) }
pub fn open_server(d: &[u8]) -> Result<Vec<u8>, String> { sealing::open(d) }
pub fn seal_method() -> &'static str { sealing::method() }

impl Secrets {
    /// Stato iniziale: nessuna chiave caricata (le carica `api::secrets_init` dal DB all'avvio).
    /// Se la vecchia YW_SECRET_KEY è ancora presente, serve SOLO a leggere i segreti v1/v2 da migrare.
    pub fn new() -> Self {
        let mut inner = Inner::default();
        if let Ok(s) = std::env::var("YW_SECRET_KEY") { if !s.is_empty() {
            inner.legacy_v1 = Some(cipher(&Sha256::digest(s.as_bytes())));
            let mut out = [0u8; 32];
            if argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13,
                argon2::Params::new(19 * 1024, 2, 1, Some(32)).expect("parametri argon2"))
                .hash_password_into(s.as_bytes(), b"youworkforce/secrets/v2", &mut out).is_ok() {
                inner.legacy_v2 = Some(cipher(&out));
            }
        } }
        Secrets { inner: RwLock::new(inner) }
    }
    pub fn load_key(&self, key_id: i32, dek: &[u8], active: bool) {
        let mut g = self.inner.write().unwrap();
        g.keys.insert(key_id, cipher(dek));
        if active { g.active = Some(key_id); g.locked = false; }
    }
    pub fn forget_key(&self, key_id: i32) { self.inner.write().unwrap().keys.remove(&key_id); }
    pub fn set_locked(&self, v: bool) { self.inner.write().unwrap().locked = v; }
    pub fn locked(&self) -> bool { self.inner.read().unwrap().locked }
    pub fn enabled(&self) -> bool { let g = self.inner.read().unwrap(); g.active.is_some() && !g.locked }
    pub fn active_id(&self) -> Option<i32> { self.inner.read().unwrap().active }
    pub fn has_legacy(&self) -> bool { let g = self.inner.read().unwrap(); g.legacy_v1.is_some() || g.legacy_v2.is_some() }

    /// Cifra con la chiave attiva. Senza chiave attiva (bloccata) rifiuta: un segreto non si salva
    /// mai in chiaro.
    pub fn encrypt(&self, plain: &str) -> Result<String, String> {
        if plain.is_empty() { return Ok(String::new()); }
        let g = self.inner.read().unwrap();
        let (Some(kid), false) = (g.active, g.locked) else {
            return Err("cifratura dei segreti bloccata: sbloccala dalla scheda Sistema del superadmin".into()) };
        let c = g.keys.get(&kid).ok_or("chiave attiva non caricata")?;
        let blob = seal_with(c, plain.as_bytes())?;
        Ok(format!("enc:v3:{kid}:{}:{}", hex::encode(&blob[..12]), hex::encode(&blob[12..])))
    }

    /// Decifra qualunque formato noto. Valori non decifrabili (chiave mancante) restano come sono.
    pub fn decrypt(&self, stored: &str) -> String {
        let g = self.inner.read().unwrap();
        let (c, rest) = if let Some(r) = stored.strip_prefix("enc:v3:") {
            let mut p = r.splitn(2, ':');
            let (Some(kid), Some(rest)) = (p.next(), p.next()) else { return stored.to_string() };
            (kid.parse::<i32>().ok().and_then(|k| g.keys.get(&k)), rest)
        } else if let Some(r) = stored.strip_prefix("enc:v2:") { (g.legacy_v2.as_ref(), r) }
        else if let Some(r) = stored.strip_prefix("enc:v1:") { (g.legacy_v1.as_ref(), r) }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_v3_and_lock() {
        let s = Secrets::new();
        assert!(s.encrypt("x").is_err(), "senza chiave non si cifra");
        s.load_key(1, &random_key(), true);
        let e = s.encrypt("sk-segreto").unwrap();
        assert!(e.starts_with("enc:v3:1:"));
        assert_eq!(s.decrypt(&e), "sk-segreto");
        s.set_locked(true);
        assert!(s.encrypt("y").is_err());
    }
    #[test]
    fn recovery_wrap() {
        let dek = random_key();
        let (salt, w) = wrap_recovery(&dek, "passphrase-lunga-1").unwrap();
        assert_eq!(unwrap_recovery(&salt, &w, "passphrase-lunga-1").unwrap(), dek.to_vec());
        assert!(unwrap_recovery(&salt, &w, "sbagliata-123456").is_err());
    }
    #[test]
    fn server_seal_roundtrip() {
        let d = random_key();
        let s = seal_server(&d).unwrap();
        assert_eq!(open_server(&s).unwrap(), d.to_vec());
    }
}
