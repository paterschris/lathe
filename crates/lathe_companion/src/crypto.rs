//! The desktop's companion identity and the libsodium-compatible primitives the
//! phone app shares: `crypto_box_easy` for frames, `crypto_box_seal` for pairing
//! requests, and Ed25519 for authenticating to the relay.

use anyhow::{Context as _, Result, anyhow};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use crypto_box::{
    PublicKey, SalsaBox, SecretKey,
    aead::{Aead, AeadCore, OsRng},
};
use ed25519_dalek::{Signer as _, SigningKey};
use sha2::{Digest as _, Sha256};

const KEY_SIZE: usize = 32;

pub struct Identity {
    signing_key: SigningKey,
    box_secret: SecretKey,
}

impl Identity {
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(&mut OsRng),
            box_secret: SecretKey::generate(&mut OsRng),
        }
    }

    /// Restores an identity saved with [`Identity::to_bytes`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let (signing_seed, box_secret) = bytes
            .split_first_chunk::<KEY_SIZE>()
            .context("companion identity is truncated")?;
        let box_secret: &[u8; KEY_SIZE] = box_secret
            .try_into()
            .context("companion identity has the wrong length")?;
        Ok(Self {
            signing_key: SigningKey::from_bytes(signing_seed),
            box_secret: SecretKey::from_bytes(*box_secret),
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(KEY_SIZE * 2);
        bytes.extend_from_slice(self.signing_key.as_bytes());
        bytes.extend_from_slice(&self.box_secret.to_bytes());
        bytes
    }

    pub fn desktop_id(&self) -> String {
        id_for_public_key(self.signing_key.verifying_key().as_bytes())
    }

    pub fn signing_public_key(&self) -> String {
        STANDARD.encode(self.signing_key.verifying_key().as_bytes())
    }

    pub fn box_public_key(&self) -> PublicKey {
        self.box_secret.public_key()
    }

    /// Answers the relay's authentication challenge by signing the decoded
    /// nonce bytes.
    pub fn sign_challenge(&self, nonce: &str) -> Result<String> {
        let nonce = STANDARD
            .decode(nonce)
            .context("challenge nonce is not base64")?;
        Ok(STANDARD.encode(self.signing_key.sign(&nonce).to_bytes()))
    }

    /// Encrypts a frame for one phone. Returns the base64 nonce and ciphertext.
    pub fn encrypt_for(&self, recipient: &PublicKey, plaintext: &[u8]) -> Result<(String, String)> {
        let nonce = SalsaBox::generate_nonce(&mut OsRng);
        let ciphertext = SalsaBox::new(recipient, &self.box_secret)
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow!("failed to encrypt companion frame"))?;
        Ok((STANDARD.encode(nonce), STANDARD.encode(ciphertext)))
    }

    pub fn decrypt_from(
        &self,
        sender: &PublicKey,
        nonce: &str,
        ciphertext: &str,
    ) -> Result<Vec<u8>> {
        let nonce = STANDARD
            .decode(nonce)
            .context("frame nonce is not base64")?;
        let nonce: [u8; 24] = nonce
            .try_into()
            .map_err(|_| anyhow!("frame nonce has the wrong length"))?;
        let ciphertext = STANDARD
            .decode(ciphertext)
            .context("frame ciphertext is not base64")?;
        SalsaBox::new(sender, &self.box_secret)
            .decrypt(&nonce.into(), ciphertext.as_slice())
            .map_err(|_| anyhow!("frame failed authentication"))
    }

    /// Opens a pairing request a phone sealed to this desktop's box key.
    pub fn unseal(&self, ciphertext: &str) -> Result<Vec<u8>> {
        let ciphertext = STANDARD
            .decode(ciphertext)
            .context("sealed request is not base64")?;
        self.box_secret
            .unseal(&ciphertext)
            .map_err(|_| anyhow!("sealed request failed authentication"))
    }
}

/// Ids are the first 16 bytes of SHA-256 of an Ed25519 public key, base64url
/// without padding, so the relay can check them without a registry.
pub fn id_for_public_key(public_key: &[u8]) -> String {
    let digest = Sha256::digest(public_key);
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

pub fn decode_key(base64: &str) -> Result<[u8; KEY_SIZE]> {
    decode_either(base64)?
        .try_into()
        .map_err(|_| anyhow!("key has the wrong length"))
}

/// JSON carries standard base64 and the pairing link carries base64url, so
/// values that pass through both are decoded with either alphabet.
pub fn decode_either(base64: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(base64)
        .or_else(|_| URL_SAFE_NO_PAD.decode(base64.trim_end_matches('=')))
        .context("value is not base64")
}

pub fn encode_url_safe(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn fill_random(bytes: &mut [u8]) {
    use crypto_box::aead::rand_core::RngCore as _;
    OsRng.fill_bytes(bytes);
}

/// Compares secrets without leaking how many leading bytes matched.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // `src/crypto/test-vectors.json` in the lathe-companion-app repo, which the
    // app checks against libsodium. Standard base64 throughout.
    const BOX_SENDER_SECRET: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
    const BOX_SENDER_PUBLIC: &str = "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9/AsrhtHHw=";
    const BOX_RECIPIENT_SECRET: &str = "ZWZnaGlqa2xtbm9wcXJzdHV2d3h5ent8fX5/gIGCg4Q=";
    const BOX_RECIPIENT_PUBLIC: &str = "VxR2nRFr92Q2rnS8eT0sMK0ZA8WaxSc4BcfiaYtBDDY=";
    const BOX_NONCE: &str = "ycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g";
    const PLAINTEXT: &str =
        r#"{"type":"send_message","client_msg_id":"m1","thread_id":"t1","text":"héllo ✓"}"#;
    const BOX_CIPHERTEXT: &str = "rQ2S1hh9b99TJkDPcTlgq+658qj802XJzd826eWC8ajm9jOW72Hy5V1Tm9q4xWs90GVVQ1w7Q0zBgrfhidppvCYsAaQA+i/gOVcpv1WDy3PNohOybEsGHj43TzBHJN6fQA==";
    const SEALED_CIPHERTEXT: &str = "REQqmLSwAzON1S9bsUm7ACYJ3zv0AjHN7jvqQIkA/wunhBMi3k6+DLTgZnQujd/hMgdD/NsVhl5IfmtWwx80zTKyoSJLs31qt0TbYCJkAiaps+u053R1YEAwc50b8rNOxwQEGl1uIjtWo4XiuPkYb3Rv3YUjXSw4dRSNvqyYmkSQ";
    const SIGN_SEED: &str = "MzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVI=";
    const SIGN_PUBLIC: &str = "TjDlN9oNdK3AZCssMw2ncymUxGBYXjmnCh3VEaYIxHs=";
    const CHALLENGE_NONCE: &str = "BwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSY=";
    const SIGNATURE: &str =
        "p8xLfysH/XDe+9UAfP/IW0t+cYeg1CLmZlovDbSluDGFkSeYaXmaki1hZwOhJx4jY5wehdLkX1RcympKrCDdCw==";

    fn identity(box_secret: &str) -> Identity {
        let mut bytes = STANDARD.decode(SIGN_SEED).unwrap();
        bytes.extend(STANDARD.decode(box_secret).unwrap());
        Identity::from_bytes(&bytes).unwrap()
    }

    fn public_key(base64: &str) -> PublicKey {
        PublicKey::from_bytes(decode_key(base64).unwrap())
    }

    #[test]
    fn keys_match_the_app() {
        assert_eq!(
            identity(BOX_SENDER_SECRET).box_public_key(),
            public_key(BOX_SENDER_PUBLIC)
        );
        assert_eq!(
            identity(BOX_RECIPIENT_SECRET).box_public_key(),
            public_key(BOX_RECIPIENT_PUBLIC)
        );
        assert_eq!(
            identity(BOX_SENDER_SECRET).signing_public_key(),
            SIGN_PUBLIC
        );
    }

    #[test]
    fn opens_the_app_box_and_produces_the_same_bytes() {
        let recipient = identity(BOX_RECIPIENT_SECRET);
        let opened = recipient
            .decrypt_from(&public_key(BOX_SENDER_PUBLIC), BOX_NONCE, BOX_CIPHERTEXT)
            .unwrap();
        assert_eq!(opened, PLAINTEXT.as_bytes());

        let sender = identity(BOX_SENDER_SECRET);
        let nonce: [u8; 24] = STANDARD.decode(BOX_NONCE).unwrap().try_into().unwrap();
        let boxed = SalsaBox::new(&public_key(BOX_RECIPIENT_PUBLIC), &sender.box_secret)
            .encrypt(&nonce.into(), PLAINTEXT.as_bytes())
            .unwrap();
        assert_eq!(STANDARD.encode(boxed), BOX_CIPHERTEXT);
    }

    #[test]
    fn round_trips_between_two_identities() {
        let desktop = Identity::generate();
        let phone = Identity::generate();
        let (nonce, ciphertext) = desktop
            .encrypt_for(&phone.box_public_key(), PLAINTEXT.as_bytes())
            .unwrap();
        let opened = phone
            .decrypt_from(&desktop.box_public_key(), &nonce, &ciphertext)
            .unwrap();
        assert_eq!(opened, PLAINTEXT.as_bytes());
    }

    #[test]
    fn rejects_a_tampered_frame() {
        let mut ciphertext = STANDARD.decode(BOX_CIPHERTEXT).unwrap();
        ciphertext[20] ^= 1;
        let result = identity(BOX_RECIPIENT_SECRET).decrypt_from(
            &public_key(BOX_SENDER_PUBLIC),
            BOX_NONCE,
            &STANDARD.encode(ciphertext),
        );
        assert!(result.is_err());
    }

    #[test]
    fn unseals_the_app_sealed_box() {
        let opened = identity(BOX_RECIPIENT_SECRET)
            .unseal(SEALED_CIPHERTEXT)
            .unwrap();
        assert_eq!(opened, PLAINTEXT.as_bytes());
    }

    #[test]
    fn signs_challenges_like_the_app() {
        assert_eq!(
            identity(BOX_SENDER_SECRET)
                .sign_challenge(CHALLENGE_NONCE)
                .unwrap(),
            SIGNATURE
        );
    }

    #[test]
    fn identity_survives_a_round_trip() {
        let identity = Identity::generate();
        let restored = Identity::from_bytes(&identity.to_bytes()).unwrap();
        assert_eq!(restored.desktop_id(), identity.desktop_id());
        assert_eq!(restored.box_public_key(), identity.box_public_key());
        assert!(Identity::from_bytes(&identity.to_bytes()[..40]).is_err());
    }

    #[test]
    fn ids_match_the_relay() {
        // Computed the way `idForPublicKey` in infra/companion-relay does it.
        assert_eq!(
            identity(BOX_SENDER_SECRET).desktop_id(),
            "26Z9RUlu2dk0lHppVrffhg"
        );
    }

    #[test]
    fn decodes_both_base64_alphabets() {
        let bytes = [0xfb_u8, 0xff, 0xbf];
        assert_eq!(decode_either("+/+/").unwrap(), bytes);
        assert_eq!(decode_either("-_-_").unwrap(), bytes);
        assert_eq!(decode_either("AQ==").unwrap(), [1]);
        assert_eq!(decode_either("AQ").unwrap(), [1]);
    }

    #[test]
    fn compares_tokens_in_constant_time() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
