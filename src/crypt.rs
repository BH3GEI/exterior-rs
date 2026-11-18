//! Secrets (device passwords, the brief key) are stored encrypted (ChaCha20-Poly1305) in config.json.
//! The key is a random 32-byte file, secret.key, next to it, readable only by the app.
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

fn key() -> [u8; 32] {
    let p = crate::store::path("secret.key");
    if let Ok(b) = std::fs::read(&p) { if b.len() == 32 { let mut k = [0u8; 32]; k.copy_from_slice(&b); return k; } }
    let mut k = [0u8; 32];
    getrandom::getrandom(&mut k).expect("random");
    let _ = std::fs::create_dir_all(crate::store::var());
    let _ = std::fs::write(&p, k);
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)); }
    k
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

pub fn seal(plain: &str) -> String {
    let c = ChaCha20Poly1305::new(Key::from_slice(&key()));
    let mut n = [0u8; 12];
    getrandom::getrandom(&mut n).expect("random");
    let ct = c.encrypt(Nonce::from_slice(&n), plain.as_bytes()).expect("encrypt");
    format!("{}{}", hex(&n), hex(&ct))
}

pub fn open(sealed: &str) -> Option<String> {
    let b = unhex(sealed.trim())?;
    if b.len() < 12 { return None; }
    let c = ChaCha20Poly1305::new(Key::from_slice(&key()));
    let p = c.decrypt(Nonce::from_slice(&b[..12]), &b[12..]).ok()?;
    String::from_utf8(p).ok()
}
