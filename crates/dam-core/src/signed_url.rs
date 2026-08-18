//! Signed delivery tokens (3.1) — the D12 chokepoint.
//!
//! Every download, render and connector fetch goes through one signed URL, so rights and ABAC are enforced
//! by the delivery design rather than by a caller remembering to check. That is what `0005_rights.sql`
//! means by "enforced at the point of distribution": there is one code path, and it is this one.
//!
//! ## A signed URL is permission to *attempt*, not permission to receive
//!
//! This is the distinction the whole design rests on. The signature proves that **we issued this exact
//! request and it has not been altered**. It does not prove the caller may have the bytes — that is decided
//! at delivery, by evaluating rights and access *then*.
//!
//! It has to work that way for revocation to mean anything. A URL issued on Monday, when the licence was
//! valid, must stop working on Tuesday when the licence lapses. If the signature authorised, every issued
//! URL would be an outstanding grant that nothing could withdraw — and 3.3 requires revocation to take
//! effect on an already-issued URL, which is the same property.
//!
//! ## The canonical form is length-prefixed, not delimited
//!
//! A subtle and complete break otherwise. Join fields with `|` and two different payloads can produce the
//! same signing string: `asset=1, transform=ab` and `asset=1a, transform=b` both render `1|ab`. One valid
//! signature then covers both, and a caller who can influence any field can forge another. Length prefixes
//! make the encoding injective, so distinct payloads always sign differently.
//!
//! ## Comparison is constant-time
//!
//! A byte-by-byte early-return comparison leaks the correct signature one byte at a time to anyone who can
//! measure response latency. `subtle` is a dependency for exactly this.

use crate::Secret;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// The token format version, carried in the payload.
///
/// So a format change is a verification failure rather than a misparse. Without it, adding a field later
/// would make old tokens decode into new ones with a shifted meaning.
///
/// Bumped to 2 by 3.3, which added `share_link_id`. A v1 token has one fewer field, so under v1's layout its
/// `expires_at` bytes would be read as the share link id and its key id as the expiry — exactly the shifted
/// meaning this constant exists to prevent. Outstanding v1 tokens stop verifying, which is correct: they were
/// issued for at most 24 hours (`delivery::MAX_TOKEN_TTL`), and the alternative is supporting two layouts so
/// that a URL from yesterday can bypass a check added today.
pub const VERSION: u8 = 2;

/// What a signed URL asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryClaim {
    pub asset_id: Uuid,
    /// The named derivative profile, or `original`.
    ///
    /// Part of the signature, so a thumbnail URL cannot be edited into a request for the master. That is
    /// the most obvious attack on a delivery URL and the cheapest to get wrong.
    pub transform: String,
    /// The distribution channel the rights check will use.
    ///
    /// Signed, because it selects which licence terms apply: a URL issued for `editorial` must not become
    /// an `advertising` delivery by changing a query parameter.
    pub channel: String,
    /// ISO 3166-1 alpha-2, or `WORLD`.
    pub territory: String,
    /// Who the URL was issued to. Carried for the audit trail and so ABAC can be re-checked at delivery.
    pub identity_id: Option<Uuid>,
    /// When the token stops being accepted.
    pub expires_at: DateTime<Utc>,
    /// The share link this URL was issued through, when it was.
    ///
    /// Signed and re-checked at delivery, which is what makes revoking a share take effect on URLs it has
    /// **already issued**. Without it, revoking a share would leave every outstanding delivery token working
    /// for its own TTL — and "revoke" would mean "revoke, eventually".
    pub share_link_id: Option<Uuid>,
    /// Which signing key was used, so a key can be rotated without invalidating every outstanding URL.
    pub key_id: String,
}

/// A signature that failed to verify, and why.
///
/// The variants are for logs and metrics, never for a response body: telling a caller *which* part of their
/// forgery failed is a hint about how to succeed. The HTTP layer collapses all of these to one refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the token was signed for a different format version")]
    WrongVersion,
    #[error("the signature does not match")]
    BadSignature,
    #[error("the token has expired")]
    Expired,
    #[error("no signing key with that id")]
    UnknownKey,
}

/// The signing keys in use, newest first.
///
/// A set rather than one key so rotation does not invalidate outstanding URLs: the new key signs, and the
/// old one still verifies until every token issued under it has expired.
#[derive(Debug, Clone)]
pub struct Keyring {
    /// `(key_id, secret)`, the first entry being the one that signs.
    keys: Vec<(String, Secret<String>)>,
}

impl Keyring {
    /// A keyring with one key.
    pub fn single(key_id: impl Into<String>, secret: Secret<String>) -> Self {
        Self {
            keys: vec![(key_id.into(), secret)],
        }
    }

    /// Adds an older key that may still verify but will not sign.
    #[must_use]
    pub fn with_retired(mut self, key_id: impl Into<String>, secret: Secret<String>) -> Self {
        self.keys.push((key_id.into(), secret));
        self
    }

    /// The key that signs new tokens.
    fn signing(&self) -> Option<&(String, Secret<String>)> {
        self.keys.first()
    }

    fn find(&self, key_id: &str) -> Option<&Secret<String>> {
        // Compared in constant time: a key *id* is not secret, but an early-return comparison over the set
        // would leak which ids exist, and there is no reason to give that away.
        self.keys
            .iter()
            .find(|(id, _)| id.as_bytes().ct_eq(key_id.as_bytes()).into())
            .map(|(_, secret)| secret)
    }
}

/// Signs `claim`, returning the token to put in the URL.
///
/// The `key_id` on the claim is ignored and replaced with the keyring's signing key — a caller choosing
/// which key signs would be a way to pin an about-to-be-retired one.
pub fn sign(keyring: &Keyring, claim: &DeliveryClaim) -> Option<String> {
    let (key_id, secret) = keyring.signing()?;
    let claim = DeliveryClaim {
        key_id: key_id.clone(),
        ..claim.clone()
    };
    let payload = canonical(&claim);
    let signature = mac(secret, &payload)?;

    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Some(format!(
        "{}.{}",
        encoder.encode(&payload),
        encoder.encode(signature)
    ))
}

/// Verifies a token and returns the claim it carries.
///
/// Returning the claim rather than a boolean is deliberate: the caller needs the asset, transform, channel
/// and territory to perform the rights check, and re-parsing an already-verified token in the caller is
/// how the verified and used values drift apart.
pub fn verify(
    keyring: &Keyring,
    token: &str,
    now: DateTime<Utc>,
) -> Result<DeliveryClaim, VerifyError> {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (payload_b64, signature_b64) = token.split_once('.').ok_or(VerifyError::Malformed)?;
    let payload = encoder
        .decode(payload_b64)
        .map_err(|_| VerifyError::Malformed)?;
    let signature = encoder
        .decode(signature_b64)
        .map_err(|_| VerifyError::Malformed)?;

    let claim = parse(&payload)?;
    if claim.key_id.is_empty() {
        return Err(VerifyError::Malformed);
    }
    let secret = keyring.find(&claim.key_id).ok_or(VerifyError::UnknownKey)?;

    // The signature is checked before the expiry. An expired token with a forged signature must report as
    // a bad signature, not as expired — the second tells an attacker their forgery was otherwise accepted.
    let expected = mac(secret, &payload).ok_or(VerifyError::BadSignature)?;
    if !bool::from(expected.ct_eq(&signature)) {
        return Err(VerifyError::BadSignature);
    }
    if now >= claim.expires_at {
        return Err(VerifyError::Expired);
    }
    Ok(claim)
}

/// HMAC-SHA256 over `payload`.
///
/// `None` only if the key is rejected, which HMAC does not do — it accepts a key of any length. Returned as
/// an `Option` rather than unwrapped because a `panic!` reachable from a delivery endpoint is a denial of
/// service, and "this cannot happen" is exactly the reasoning that puts one there. The callers already have
/// a failure path, so propagating costs nothing.
fn mac(secret: &Secret<String>, payload: &[u8]) -> Option<Vec<u8>> {
    let mut hmac = <Hmac<Sha256>>::new_from_slice(secret.expose().as_bytes()).ok()?;
    hmac.update(payload);
    Some(hmac.finalize().into_bytes().to_vec())
}

/// The bytes that get signed.
///
/// Length-prefixed, so the encoding is injective — see the module docs on why a delimiter is not enough.
/// Every field is included: a field outside the signature is a field an attacker may choose.
fn canonical(claim: &DeliveryClaim) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    out.push(VERSION);
    push_field(&mut out, claim.asset_id.as_bytes());
    push_field(&mut out, claim.transform.as_bytes());
    push_field(&mut out, claim.channel.as_bytes());
    push_field(&mut out, claim.territory.as_bytes());
    match claim.identity_id {
        Some(id) => push_field(&mut out, id.as_bytes()),
        // A distinct empty field rather than an omitted one, so `None` and `Some` of an empty value cannot
        // collide — an omitted field would shorten the encoding and change what the next length means.
        None => push_field(&mut out, &[]),
    }
    match claim.share_link_id {
        Some(id) => push_field(&mut out, id.as_bytes()),
        None => push_field(&mut out, &[]),
    }
    push_field(&mut out, &claim.expires_at.timestamp().to_be_bytes());
    push_field(&mut out, claim.key_id.as_bytes());
    out
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    // A 32-bit big-endian length. `u32` rather than `u8` because a transform string or key id can exceed
    // 255 bytes, and a truncating length is a collision.
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn parse(payload: &[u8]) -> Result<DeliveryClaim, VerifyError> {
    let mut cursor = payload;
    let version = take_byte(&mut cursor)?;
    if version != VERSION {
        return Err(VerifyError::WrongVersion);
    }

    let asset_id = take_uuid(&mut cursor)?;
    let transform = take_string(&mut cursor)?;
    let channel = take_string(&mut cursor)?;
    let territory = take_string(&mut cursor)?;
    let identity_id = take_optional_uuid(&mut cursor)?;
    let share_link_id = take_optional_uuid(&mut cursor)?;
    let expiry_raw = take_field(&mut cursor)?;
    let seconds = i64::from_be_bytes(expiry_raw.try_into().map_err(|_| VerifyError::Malformed)?);
    let expires_at = DateTime::from_timestamp(seconds, 0).ok_or(VerifyError::Malformed)?;
    let key_id = take_string(&mut cursor)?;

    // Trailing bytes are a malformed token, not something to ignore. Ignoring them would let an attacker
    // append arbitrary data to a payload whose signature they already have — and while the signature would
    // fail, accepting the shape at all invites the next variation.
    if !cursor.is_empty() {
        return Err(VerifyError::Malformed);
    }

    Ok(DeliveryClaim {
        asset_id,
        transform,
        channel,
        territory,
        identity_id,
        share_link_id,
        expires_at,
        key_id,
    })
}

/// A UUID field that may be a zero-length placeholder for `None`.
fn take_optional_uuid(cursor: &mut &[u8]) -> Result<Option<Uuid>, VerifyError> {
    let raw = take_field(cursor)?;
    if raw.is_empty() {
        return Ok(None);
    }
    Uuid::from_slice(raw)
        .map(Some)
        .map_err(|_| VerifyError::Malformed)
}

fn take_byte(cursor: &mut &[u8]) -> Result<u8, VerifyError> {
    let (first, rest) = cursor.split_first().ok_or(VerifyError::Malformed)?;
    *cursor = rest;
    Ok(*first)
}

fn take_field<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8], VerifyError> {
    if cursor.len() < 4 {
        return Err(VerifyError::Malformed);
    }
    let (len_bytes, rest) = cursor.split_at(4);
    let len =
        u32::from_be_bytes(len_bytes.try_into().map_err(|_| VerifyError::Malformed)?) as usize;
    if rest.len() < len {
        return Err(VerifyError::Malformed);
    }
    let (field, remainder) = rest.split_at(len);
    *cursor = remainder;
    Ok(field)
}

fn take_string(cursor: &mut &[u8]) -> Result<String, VerifyError> {
    let field = take_field(cursor)?;
    String::from_utf8(field.to_vec()).map_err(|_| VerifyError::Malformed)
}

fn take_uuid(cursor: &mut &[u8]) -> Result<Uuid, VerifyError> {
    let field = take_field(cursor)?;
    Uuid::from_slice(field).map_err(|_| VerifyError::Malformed)
}
