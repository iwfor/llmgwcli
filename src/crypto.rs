//! Encrypts credentials with ChaCha20-Poly1305 using the llmgwcli.key text as the key.

use anyhow::{Result, anyhow, ensure};
use rand::RngExt;
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};

const NONCE_LEN: usize = 12;

fn cipher(key: &str) -> Result<ChaCha20Poly1305> {
    ensure!(key.len() == 32, "key must be 32 bytes");
    let key = Key::try_from(key.as_bytes()).map_err(|_| anyhow!("bad key"))?;
    Ok(ChaCha20Poly1305::new(&key))
}

/// Returns base64(nonce || ciphertext).
pub fn encrypt(key: &str, plain: &str) -> Result<String> {
    let cipher = cipher(key)?;
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill(&mut nonce);
    let mut out = nonce.to_vec();
    out.extend(
        cipher
            .encrypt(&Nonce::try_from(&nonce[..]).unwrap(), plain.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?,
    );
    Ok(STANDARD.encode(out))
}

pub fn decrypt(key: &str, encoded: &str) -> Result<String> {
    let cipher = cipher(key)?;
    let data = STANDARD.decode(encoded)?;
    ensure!(data.len() > NONCE_LEN, "credential is too short");
    let (nonce, ciphertext) = data.split_at(NONCE_LEN);
    let plain = cipher
        .decrypt(&Nonce::try_from(nonce).unwrap(), ciphertext)
        .map_err(|_| anyhow!("decryption failed (wrong llmgwcli.key?)"))?;
    Ok(String::from_utf8(plain)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "abcdefghijklmnopqrstuvwxyz012345";

    #[test]
    fn round_trip() {
        let enc = encrypt(KEY, "sk-secret").unwrap();
        assert_eq!(decrypt(KEY, &enc).unwrap(), "sk-secret");
    }

    #[test]
    fn wrong_key_fails() {
        let enc = encrypt(KEY, "sk-secret").unwrap();
        assert!(decrypt("ABCDEFGHIJKLMNOPQRSTUVWXYZ012345", &enc).is_err());
    }

    #[test]
    fn same_text_encrypts_differently_each_time() {
        assert_ne!(encrypt(KEY, "x").unwrap(), encrypt(KEY, "x").unwrap());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let mut data = STANDARD.decode(encrypt(KEY, "sk-secret").unwrap()).unwrap();
        let last = data.len() - 1;
        data[last] ^= 1;
        assert!(decrypt(KEY, &STANDARD.encode(data)).is_err());
    }

    #[test]
    fn short_or_invalid_input_fails() {
        assert!(decrypt(KEY, &STANDARD.encode([0u8; 5])).is_err());
        assert!(decrypt(KEY, "not base64!").is_err());
    }

    #[test]
    fn wrong_key_length_fails() {
        assert!(encrypt("short", "x").is_err());
        assert!(decrypt("short", "x").is_err());
    }

    #[test]
    fn empty_and_unicode_text_round_trip() {
        for text in ["", "héllo ✓"] {
            assert_eq!(decrypt(KEY, &encrypt(KEY, text).unwrap()).unwrap(), text);
        }
    }
}
