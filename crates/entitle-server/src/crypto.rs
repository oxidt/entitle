//! Secrets at rest, API key hashing and webhook signatures.

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

const NONCE_LEN: usize = 24;

/// AEAD for per-project secrets. Sealed format: `nonce (24 bytes) || ciphertext`.
pub struct Cipher(XChaCha20Poly1305);

impl Cipher {
    /// `key_b64` is standard base64 of exactly 32 bytes.
    pub fn from_base64(key_b64: &str) -> anyhow::Result<Self> {
        let key = STANDARD
            .decode(key_b64.trim())
            .context("encryption key is not valid base64")?;
        if key.len() != 32 {
            bail!("encryption key must be 32 bytes, got {}", key.len());
        }
        Ok(Cipher(
            XChaCha20Poly1305::new_from_slice(&key).expect("length checked"),
        ))
    }

    /// `aad` binds the ciphertext to its row so it cannot be swapped between rows.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("OS random number generator");
        let ct = self
            .0
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("encryption cannot fail for in-memory buffers");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        out
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
        if sealed.len() < NONCE_LEN {
            bail!("sealed value too short");
        }
        let (nonce, ct) = sealed.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
        self.0
            .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad })
            .map_err(|_| anyhow::anyhow!("decryption failed (wrong key or tampered data)"))
    }
}

/// `prefix` + 32 random bytes, URL-safe base64.
pub fn random_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// API keys are high-entropy, so a plain SHA-256 is enough for lookup-by-hash.
pub fn hash_key(key: &str) -> Vec<u8> {
    Sha256::digest(key.as_bytes()).to_vec()
}

/// Hex HMAC-SHA256 over `"{timestamp}.{body}"`.
pub fn sign_webhook(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Value of the `Entitle-Signature` header: `t=<unix seconds>,v1=<hex hmac>`.
pub fn signature_header(secret: &str, timestamp: i64, body: &[u8]) -> String {
    format!("t={timestamp},v1={}", sign_webhook(secret, timestamp, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher::from_base64(&STANDARD.encode([7u8; 32])).unwrap()
    }

    #[test]
    fn seal_roundtrip_and_aad_binding() {
        let c = cipher();
        let sealed = c.seal(b"secret", b"row-1");
        assert_eq!(c.open(&sealed, b"row-1").unwrap(), b"secret");
        assert!(c.open(&sealed, b"row-2").is_err());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(c.open(&tampered, b"row-1").is_err());
        assert_ne!(c.seal(b"secret", b"row-1"), sealed, "nonce must be random");
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(Cipher::from_base64("not base64!").is_err());
        assert!(Cipher::from_base64(&STANDARD.encode([1u8; 16])).is_err());
    }

    #[test]
    fn tokens_are_prefixed_and_unique() {
        let a = random_token("pk_");
        assert!(a.starts_with("pk_") && a.len() == 3 + 43);
        assert_ne!(a, random_token("pk_"));
    }

    #[test]
    fn webhook_signature_matches_reference() {
        // echo -n '1700000000.{"a":1}' | openssl dgst -sha256 -hmac whsec_test
        assert_eq!(
            sign_webhook("whsec_test", 1_700_000_000, br#"{"a":1}"#),
            "38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789"
        );
    }
}
