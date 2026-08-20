//! Encryption at rest for a tenant's own credentials (M5).
//!
//! A tenant that brings its own model-provider key hands damrs a secret that damrs must be able to *use* later —
//! so unlike a passcode or an API key of our own, hashing is not an option. It has to come back.
//!
//! ## What this is not
//!
//! It is not a key-management system. The sealing keys live in configuration, the way the URL-signing key does,
//! and rotating them is an operator action. What it buys is the property that matters most in practice: a
//! database dump, a backup, a replica or a stray `SELECT` yields ciphertext, and the plaintext exists only in a
//! process that already holds the deployment's sealing key. G10's BYOK is the larger version of this and is
//! deliberately still ahead of us.
//!
//! ## The ciphertext is bound to where it lives
//!
//! Every seal takes an *associated data* string — the tenant slug and what the credential is for. AAD is
//! authenticated but not encrypted, which means a row lifted from one tenant's table into another's fails to
//! open rather than quietly working. Without it, "copy the row" would be a complete attack against
//! per-tenant isolation, and the schema-per-tenant boundary (D2) would end at the credential table.
//!
//! ## Rotation reads all keys and writes the first
//!
//! The same shape as [`crate::signed_url::Keyring`], for the same reason: a deployment mid-rotation has rows
//! sealed under both, and a scheme that could only read the current key would make rotation an outage.

use crate::Secret;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use subtle::ConstantTimeEq as _;

/// How a sealed value is spelled in a column: `v1.<key_id>.<nonce>.<ciphertext>`.
///
/// Versioned from the first byte, because the alternative is guessing later which scheme produced a blob. The
/// `key_id` is in the clear on purpose: opening requires knowing which key to try, and a key *id* is not a
/// secret — the same argument [`crate::signed_url`] makes for putting it in a token.
const VERSION: &str = "v1";

/// Why a sealed value could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The text is not a sealed value this build understands.
    Malformed,
    /// No key with that id is on the ring — typically a rotation that retired a key too early.
    UnknownKey(String),
    /// The key is right and the ciphertext, nonce or associated data is not.
    ///
    /// **One variant for all three.** Distinguishing "wrong tenant" from "corrupt ciphertext" would tell somebody
    /// holding a stolen row which half of their guess was correct.
    Refused,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed => write!(f, "not a sealed value"),
            Self::UnknownKey(id) => write!(f, "no sealing key {id:?} is configured"),
            Self::Refused => write!(f, "the sealed value did not open"),
        }
    }
}

impl std::error::Error for OpenError {}

/// The keys a deployment can seal and open with.
///
/// The first seals; every key can open. Cloned cheaply and held in application state, like the signing keyring.
#[derive(Clone)]
pub struct SealingKeyring {
    keys: Vec<(String, [u8; 32])>,
}

impl std::fmt::Debug for SealingKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ids, never the key material. A `Debug` that printed keys is how a key reaches a log.
        f.debug_struct("SealingKeyring")
            .field(
                "key_ids",
                &self.keys.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SealingKeyring {
    /// A keyring with one key, derived from a configured secret.
    ///
    /// The secret is any string; the key is BLAKE3 of it, so an operator may configure a passphrase or a base64
    /// blob and neither is wrong. Deriving rather than requiring exactly 32 bytes is the difference between a
    /// deployment that works and one that fails on a trailing newline.
    pub fn single(key_id: impl Into<String>, secret: &Secret<String>) -> Self {
        Self {
            keys: vec![(key_id.into(), derive(secret, SEALING_CONTEXT))],
        }
    }

    /// Adds a key that may open but will not seal.
    #[must_use]
    pub fn with_retired(mut self, key_id: impl Into<String>, secret: &Secret<String>) -> Self {
        self.keys
            .push((key_id.into(), derive(secret, SEALING_CONTEXT)));
        self
    }

    /// Seals `plaintext`, bound to `aad`.
    ///
    /// The nonce is random per call. ChaCha20-Poly1305's nonce is 96 bits, which is small enough that reuse
    /// under one key is a real risk at extreme volumes — and irrelevant here, where a seal happens when somebody
    /// saves a credential rather than per request.
    pub fn seal(&self, plaintext: &Secret<String>, aad: &str) -> Result<String, OpenError> {
        let (key_id, key) = self.keys.first().ok_or(OpenError::Malformed)?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce_bytes: [u8; 12] = rand::random();
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.expose().as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| OpenError::Refused)?;

        Ok(format!(
            "{VERSION}.{key_id}.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce_bytes),
            URL_SAFE_NO_PAD.encode(&ciphertext)
        ))
    }

    /// Opens a sealed value, which must have been sealed under the same `aad`.
    pub fn open(&self, sealed: &str, aad: &str) -> Result<Secret<String>, OpenError> {
        let mut parts = sealed.split('.');
        let (Some(version), Some(key_id), Some(nonce), Some(ciphertext), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(OpenError::Malformed);
        };
        if version != VERSION {
            return Err(OpenError::Malformed);
        }

        // Constant-time over the ids: an id is not secret, but an early return would leak which ids a
        // deployment holds, and there is no reason to hand that over.
        let key = self
            .keys
            .iter()
            .find(|(id, _)| id.as_bytes().ct_eq(key_id.as_bytes()).into())
            .map(|(_, key)| key)
            .ok_or_else(|| OpenError::UnknownKey(key_id.to_owned()))?;

        let nonce = URL_SAFE_NO_PAD
            .decode(nonce)
            .map_err(|_| OpenError::Malformed)?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(ciphertext)
            .map_err(|_| OpenError::Malformed)?;
        if nonce.len() != 12 {
            return Err(OpenError::Malformed);
        }

        let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| OpenError::Refused)?;

        String::from_utf8(plaintext)
            .map(Secret::new)
            .map_err(|_| OpenError::Refused)
    }

    /// Which key sealed a value, without opening it.
    ///
    /// For reporting what a rotation still has to re-seal — a question an operator asks about rows they
    /// deliberately cannot read.
    pub fn key_id_of(sealed: &str) -> Option<&str> {
        let mut parts = sealed.split('.');
        match (parts.next(), parts.next()) {
            (Some(VERSION), Some(key_id)) if !key_id.is_empty() => Some(key_id),
            _ => None,
        }
    }
}

/// What this keyring's keys are *for*.
///
/// Domain separation: an operator who configures the same string for two purposes should not end up with one key
/// doing both jobs. Passed as a parameter rather than baked into [`derive`] so the property is testable — with the
/// context hard-coded there was no way to write a test that could tell domain separation from its absence, which
/// mutation testing pointed out by surviving.
const SEALING_CONTEXT: &str = "damrs credential sealing v1";

/// The 32-byte key a configured secret stands for, under a named purpose.
fn derive(secret: &Secret<String>, context: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key(context);
    hasher.update(secret.expose().as_bytes());
    *hasher.finalize().as_bytes()
}

/// The last few characters of a credential, for telling two of them apart in a list.
///
/// Not a hash: a hint has to be recognisable to the person who pasted the key, and four characters of a
/// provider key is what every console shows. Fewer than eight characters yields nothing at all rather than
/// most of a short secret.
pub fn hint(plaintext: &Secret<String>) -> String {
    let value = plaintext.expose();
    if value.chars().count() < 8 {
        return String::new();
    }
    let tail: String = value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring() -> SealingKeyring {
        SealingKeyring::single("k1", &Secret::new("a sealing passphrase".to_owned()))
    }

    #[test]
    fn a_sealed_value_opens_under_the_same_associated_data() {
        let ring = ring();
        let sealed = ring
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        assert_eq!(
            ring.open(&sealed, "acme:anthropic").expect("open").expose(),
            "sk-secret-value"
        );
    }

    #[test]
    fn the_plaintext_is_not_in_the_sealed_text() {
        // The whole point. A `SELECT` on the column, a backup or a replica yields this string.
        let sealed = ring()
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        assert!(!sealed.contains("sk-secret-value"), "{sealed}");
        assert!(sealed.starts_with("v1.k1."), "{sealed}");
    }

    #[test]
    fn a_row_copied_to_another_tenant_does_not_open() {
        // Without authenticated associated data this would succeed, and "copy the row" would be a complete
        // attack against per-tenant isolation — the schema boundary (D2) would end at this table.
        let ring = ring();
        let sealed = ring
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        assert_eq!(
            ring.open(&sealed, "globex:anthropic"),
            Err(OpenError::Refused)
        );
        // And the same tenant with a different purpose is refused too, so one credential cannot stand in for
        // another provider's.
        assert_eq!(ring.open(&sealed, "acme:openai"), Err(OpenError::Refused));
    }

    #[test]
    fn a_different_key_does_not_open_it() {
        let sealed = ring()
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        let other = SealingKeyring::single("k1", &Secret::new("a different passphrase".to_owned()));
        assert_eq!(
            other.open(&sealed, "acme:anthropic"),
            Err(OpenError::Refused)
        );
    }

    #[test]
    fn rotation_opens_what_the_retired_key_sealed_and_seals_with_the_new_one() {
        // A deployment mid-rotation holds rows under both keys. A scheme that could only read the current key
        // would make rotation an outage, which is how rotations get postponed forever.
        let old = SealingKeyring::single("k1", &Secret::new("old passphrase".to_owned()));
        let sealed_old = old
            .seal(&Secret::new("sk-old".to_owned()), "acme:anthropic")
            .expect("seal");

        let rotated = SealingKeyring::single("k2", &Secret::new("new passphrase".to_owned()))
            .with_retired("k1", &Secret::new("old passphrase".to_owned()));
        assert_eq!(
            rotated
                .open(&sealed_old, "acme:anthropic")
                .expect("open")
                .expose(),
            "sk-old"
        );

        // New seals use the first key, which is what makes a re-seal pass move rows forward.
        let sealed_new = rotated
            .seal(&Secret::new("sk-new".to_owned()), "acme:anthropic")
            .expect("seal");
        assert_eq!(SealingKeyring::key_id_of(&sealed_new), Some("k2"));
        assert_eq!(SealingKeyring::key_id_of(&sealed_old), Some("k1"));

        // And retiring a key too early is a *named* failure, not a generic refusal: the operator needs to know
        // it is a configuration mistake rather than a corrupt row.
        let without = SealingKeyring::single("k2", &Secret::new("new passphrase".to_owned()));
        assert_eq!(
            without.open(&sealed_old, "acme:anthropic"),
            Err(OpenError::UnknownKey("k1".to_owned()))
        );
    }

    #[test]
    fn two_seals_of_one_value_differ() {
        // A fresh nonce per call. Identical ciphertexts would tell anybody reading the table which tenants share
        // a credential, which is a fact about their arrangements rather than about the key.
        let ring = ring();
        let secret = Secret::new("sk-secret-value".to_owned());
        let first = ring.seal(&secret, "acme:anthropic").expect("seal");
        let second = ring.seal(&secret, "acme:anthropic").expect("seal");
        assert_ne!(first, second);
        assert_eq!(
            ring.open(&first, "acme:anthropic").expect("open").expose(),
            "sk-secret-value"
        );
        assert_eq!(
            ring.open(&second, "acme:anthropic").expect("open").expose(),
            "sk-secret-value"
        );
    }

    #[test]
    fn a_tampered_value_is_refused_rather_than_returning_rubbish() {
        let ring = ring();
        let sealed = ring
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        // Tampered at the *byte* level, then re-encoded. The first version flipped the last base64 character,
        // which was flaky: that character carries padding bits, so flipping it sometimes produced a
        // non-canonical encoding that failed to *decode* — `Malformed` rather than `Refused`, a different claim
        // about a different failure. Whether it happened depended on the random nonce, so the test passed
        // repeatedly and then did not.
        let parts: Vec<&str> = sealed.split('.').collect();
        let mut ciphertext = URL_SAFE_NO_PAD.decode(parts[3]).expect("decode");
        ciphertext[0] ^= 0x01;
        let tampered = format!(
            "{}.{}.{}.{}",
            parts[0],
            parts[1],
            parts[2],
            URL_SAFE_NO_PAD.encode(&ciphertext)
        );

        // Authentication fails, which is the point: ChaCha20-Poly1305 will not hand back plaintext it cannot
        // vouch for, so an edited or corrupted row is a refusal rather than a silently wrong credential.
        assert_eq!(
            ring.open(&tampered, "acme:anthropic"),
            Err(OpenError::Refused)
        );

        // The nonce is covered by the same tag.
        let mut nonce = URL_SAFE_NO_PAD.decode(parts[2]).expect("decode");
        nonce[0] ^= 0x01;
        let renonced = format!(
            "{}.{}.{}.{}",
            parts[0],
            parts[1],
            URL_SAFE_NO_PAD.encode(&nonce),
            parts[3]
        );
        assert_eq!(
            ring.open(&renonced, "acme:anthropic"),
            Err(OpenError::Refused)
        );
    }

    #[test]
    fn the_same_secret_under_another_purpose_is_another_key() {
        // What domain separation is for: an operator configuring one string for two jobs should not get one key
        // doing both. The earlier version of this baked the context into `derive`, which left the property
        // unfalsifiable — a mutation removing the separation survived, because no test could see it.
        let secret = Secret::new("one configured string".to_owned());
        assert_ne!(
            derive(&secret, SEALING_CONTEXT),
            derive(&secret, "some other purpose")
        );
    }

    #[test]
    fn the_derivation_is_pinned() {
        // A golden vector. The derivation is a wire format in the sense that matters: every credential already in
        // a database was sealed under it, so changing the hash, the context string or the ordering silently
        // orphans every row. This fails when that happens, which is the only way to notice.
        let key = derive(
            &Secret::new("a sealing passphrase".to_owned()),
            SEALING_CONTEXT,
        );
        assert_eq!(
            URL_SAFE_NO_PAD.encode(key),
            "3GCpvqEJgDUqPIKn8avdbiO_PHRrERrlJlLtlTbDGGo",
            "the sealing key derivation changed; every sealed credential in every database is now unreadable"
        );
    }

    #[test]
    fn a_well_formed_value_with_another_version_is_refused() {
        // The version check, made falsifiable: a value whose *only* fault is its version. The earlier case used
        // `v2.k1.aa.bb`, whose nonce is too short — so it was rejected by the length check whether or not the
        // version was ever looked at, and a mutation removing the check survived.
        let ring = ring();
        let sealed = ring
            .seal(&Secret::new("sk-secret-value".to_owned()), "acme:anthropic")
            .expect("seal");
        let bumped = sealed.replacen("v1.", "v2.", 1);
        assert_eq!(
            ring.open(&bumped, "acme:anthropic"),
            Err(OpenError::Malformed),
            "a value from a scheme this build does not know was opened anyway"
        );
        // And `key_id_of` agrees, so a rotation report cannot be built from values it cannot read.
        assert_eq!(SealingKeyring::key_id_of(&bumped), None);
    }

    #[test]
    fn nonsense_is_malformed_rather_than_refused() {
        // The distinction an operator needs: `Malformed` means the column does not hold a sealed value at all —
        // a bad migration, a truncated copy — while `Refused` means it does and the key or tenant is wrong.
        let ring = ring();
        for text in [
            "",
            "not sealed",
            "v2.k1.aa.bb",
            "v1.k1.aa",
            "v1.k1.aa.bb.cc",
        ] {
            assert_eq!(
                ring.open(text, "acme:anthropic"),
                Err(OpenError::Malformed),
                "{text}"
            );
        }
        // A well-formed shape naming an absent key is the other named case.
        assert!(matches!(
            ring.open("v1.k9.AAAAAAAAAAAAAAAA.AAAA", "acme:anthropic"),
            Err(OpenError::UnknownKey(_))
        ));
    }

    #[test]
    fn a_hint_shows_four_characters_and_never_a_short_secret() {
        assert_eq!(
            hint(&Secret::new("sk-ant-api03-abcd1234".to_owned())),
            "…1234"
        );
        // Nothing at all for something short: four of eight characters is most of the secret.
        assert_eq!(hint(&Secret::new("short".to_owned())), "");
        assert_eq!(hint(&Secret::new(String::new())), "");
    }

    #[test]
    fn debug_never_prints_key_material() {
        let printed = format!("{:?}", ring());
        assert!(printed.contains("k1"), "{printed}");
        assert!(!printed.contains("passphrase"), "{printed}");
    }
}
