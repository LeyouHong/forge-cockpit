//! At-rest encryption for connector secrets (AES-256-GCM).
//!
//! The key is a random 256-bit key generated on first use and persisted to
//! `~/.forge-web-key` with `0600` perms. Encrypted values are stored as
//! `enc:v1:<base64(nonce ‖ ciphertext)>`; anything without that prefix is
//! treated as legacy plaintext and returned unchanged, so existing configs keep
//! working and only newly-saved secrets are encrypted.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;

/// A URL-safe, unpadded random string of `n` bytes of entropy — used for OAuth
/// PKCE code verifiers and `state` values.
pub(crate) fn random_urlsafe(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

const PREFIX: &str = "enc:v1:";
const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn key_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".forge-web-key"))
}

/// Loads the encryption key, generating and persisting one (`0600`) on first use.
fn load_key() -> Option<[u8; 32]> {
    let path = key_path()?;
    if let Ok(b64) = std::fs::read_to_string(&path)
        && let Ok(bytes) = B64.decode(b64.trim())
        && bytes.len() == 32
    {
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        return Some(k);
    }
    let key = Aes256Gcm::generate_key(OsRng);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = f.write_all(B64.encode(key.as_slice()).as_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    let mut k = [0u8; 32];
    k.copy_from_slice(key.as_slice());
    Some(k)
}

/// Encrypts a secret value. On any failure, returns the plaintext (never worse
/// than the previous plaintext-at-rest behaviour).
pub(crate) fn encrypt(plaintext: &str) -> String {
    let Some(key_bytes) = load_key() else {
        return plaintext.to_string();
    };
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    match cipher.encrypt(&nonce, plaintext.as_bytes()) {
        Ok(ct) => {
            let mut blob = nonce.to_vec();
            blob.extend_from_slice(&ct);
            format!("{PREFIX}{}", B64.encode(blob))
        }
        Err(_) => plaintext.to_string(),
    }
}

/// Decrypts a value. Non-`enc:` values are returned unchanged (legacy plaintext).
pub(crate) fn decrypt(value: &str) -> String {
    let Some(rest) = value.strip_prefix(PREFIX) else {
        return value.to_string();
    };
    let (Some(key_bytes), Ok(blob)) = (load_key(), B64.decode(rest)) else {
        return value.to_string();
    };
    if blob.len() < 12 {
        return value.to_string();
    }
    let (nonce, ct) = blob.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));
    match cipher.decrypt(Nonce::from_slice(nonce), ct) {
        Ok(pt) => String::from_utf8_lossy(&pt).to_string(),
        Err(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        // Needs HOME; the test runner provides one.
        let secret = "glpat-super-secret-123";
        let enc = encrypt(secret);
        assert!(enc.starts_with(PREFIX), "value should be encrypted: {enc}");
        assert_ne!(enc, secret);
        assert_eq!(decrypt(&enc), secret);
        // Legacy plaintext passes through untouched.
        assert_eq!(decrypt("plain-token"), "plain-token");
    }
}
