//! A short-lived credential a connected site mints for its own browser (M3d·3, §11.1).
//!
//! ## Why this exists at all
//!
//! §11.1 asks for a CORS-enabled browse endpoint "for the embedded asset picker". A picker runs in an editor's
//! browser, and a browser needs a credential — which cannot be the connector's API key. That key is long-lived,
//! grants every read the site has, and putting it in JavaScript hands it to every editor, every browser
//! extension and every page the picker is embedded in. This codebase refuses that shape everywhere else and
//! there is no reason to make an exception for a file picker.
//!
//! ## And why it needs no new mechanism
//!
//! The site already holds a secret it signs render URLs with — that is the whole of §11.3. So it can sign this
//! too, in PHP, with no call to damrs: a token minted server-side when the picker is rendered, valid for
//! minutes, carrying nothing but "browse as this connector". No endpoint to mint one, no round trip in the path
//! of opening a dialog, and rotation and the grace window work on it for free because it is verified with the
//! same keyring.
//!
//! ## It carries no scope of its own
//!
//! Deliberately. A token that could narrow *or widen* what the picker sees would be a second place a
//! connector's reach is decided, and the widening direction is the dangerous one — a site could mint itself a
//! token for assets it was never granted. So this says only which connector is calling; everything about what
//! that connector may see comes from its own row and its own role, resolved the ordinary way.
//!
//! ## Separate from a delivery token, and not by accident
//!
//! A delivery token authorises bytes for one asset and lives for minutes to a day. This authorises a *search*
//! and should live for minutes. Reusing `DeliveryClaim` with an empty asset id would make one signature cover
//! two very different powers, and the first person to widen either would widen both.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use dam_core::Secret;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// The token format version.
///
/// Its own version, not the delivery token's: the two formats change for different reasons, and a shared
/// number would mean adding a field to one invalidating outstanding tokens of the other.
pub const VERSION: u8 = 1;

/// The longest a browse token may be valid for.
///
/// Ten minutes. Long enough to open a picker, search, page through results and choose something; short enough
/// that one leaking out of a browser's history or a proxy log is worth very little. A site wanting a longer
/// session mints another — it costs nothing, because minting is local.
pub const MAX_TTL: chrono::Duration = chrono::Duration::minutes(10);

/// What a browse token says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseClaim {
    /// Which connector is calling. Everything about what it may see is resolved from this, not carried here.
    pub connector_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

/// Why a browse token was not accepted.
///
/// For logs and metrics, never for a response body: telling a caller which part of their forgery failed is a
/// hint about how to succeed. The HTTP layer collapses these to one refusal, as the delivery route does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BrowseError {
    #[error("the token is malformed")]
    Malformed,
    #[error("the token was signed for a different format version")]
    WrongVersion,
    #[error("the signature does not match")]
    BadSignature,
    #[error("the token has expired")]
    Expired,
    /// Its lifetime exceeds [`MAX_TTL`], so it is refused however well it is signed.
    ///
    /// Checked because the *site* chooses the expiry and a site that sets a year would have turned a
    /// short-lived credential into its API key with extra steps. A ceiling enforced at verification cannot be
    /// opted out of by whoever mints.
    #[error("the token's lifetime exceeds the maximum")]
    TooLong,
}

/// Signs a claim with one of the connector's secrets.
///
/// Present for tests and for a Rust-side client; the real signer is PHP. That is the point of documenting the
/// canonical form as precisely as the delivery token's — a format only this crate can produce is a format the
/// integration cannot use.
#[must_use]
pub fn sign(secret: &Secret<String>, claim: &BrowseClaim) -> Option<String> {
    let payload = canonical(claim);
    let signature = mac(secret, &payload)?;
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Some(format!(
        "{}.{}",
        encoder.encode(&payload),
        encoder.encode(signature)
    ))
}

/// The connector a token claims, without verifying it.
///
/// Unauthenticated by construction, and the only safe use is choosing which secrets to verify against — the
/// same argument `dam_core::signed_url::key_id_of` makes at length. Naming the wrong connector produces a
/// signature that does not match.
#[must_use]
pub fn connector_of(token: &str) -> Option<Uuid> {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = encoder.decode(token.split_once('.')?.0).ok()?;
    parse(&payload).ok().map(|claim| claim.connector_id)
}

/// Verifies a token against every secret the connector currently has.
///
/// `secrets` is the current one and, while it is inside its grace window, the superseded one — the same set the
/// delivery route builds. Every secret is tried rather than the first, and the loop does not short-circuit, so
/// the time taken does not say which secret matched and therefore how far through a rotation a site is.
pub fn verify<'a>(
    secrets: impl IntoIterator<Item = &'a Secret<String>>,
    token: &str,
    now: DateTime<Utc>,
) -> Result<BrowseClaim, BrowseError> {
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (payload_b64, signature_b64) = token.split_once('.').ok_or(BrowseError::Malformed)?;
    let payload = encoder
        .decode(payload_b64)
        .map_err(|_| BrowseError::Malformed)?;
    let signature = encoder
        .decode(signature_b64)
        .map_err(|_| BrowseError::Malformed)?;

    let claim = parse(&payload)?;

    let mut matched = false;
    let mut any = false;
    for secret in secrets {
        any = true;
        if let Some(expected) = mac(secret, &payload) {
            matched |= bool::from(expected.ct_eq(&signature));
        }
    }
    // No secrets is a bad signature rather than its own variant: a revoked connector has none, and saying so
    // would distinguish "revoked" from "forged" to whoever holds the token.
    if !any || !matched {
        return Err(BrowseError::BadSignature);
    }

    // The signature first, then the clock — an expired forgery must report as a bad signature, not as expired,
    // for the reason `signed_url::verify` gives.
    if now >= claim.expires_at {
        return Err(BrowseError::Expired);
    }
    // The ceiling last, so a token that is both over-long and forged reports the forgery.
    if claim.expires_at - now > MAX_TTL {
        return Err(BrowseError::TooLong);
    }
    Ok(claim)
}

/// The bytes that get signed.
///
/// Length-prefixed, so the encoding is injective — the same rule `dam_core::signed_url` documents, and the
/// reason is the same: joining fields with a delimiter lets two different claims render identically, and one
/// signature then covers both.
fn canonical(claim: &BrowseClaim) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.push(VERSION);
    push_field(&mut out, claim.connector_id.as_bytes());
    push_field(&mut out, &claim.expires_at.timestamp().to_be_bytes());
    out
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn parse(payload: &[u8]) -> Result<BrowseClaim, BrowseError> {
    let mut cursor = payload;
    let version = take_byte(&mut cursor)?;
    if version != VERSION {
        return Err(BrowseError::WrongVersion);
    }
    let connector_id =
        Uuid::from_slice(take_field(&mut cursor)?).map_err(|_| BrowseError::Malformed)?;
    let seconds = i64::from_be_bytes(
        take_field(&mut cursor)?
            .try_into()
            .map_err(|_| BrowseError::Malformed)?,
    );
    let expires_at = DateTime::from_timestamp(seconds, 0).ok_or(BrowseError::Malformed)?;
    // Trailing bytes are a refusal rather than ignored input: a payload with something appended is a payload
    // somebody is experimenting with, and accepting it makes the encoding non-injective again.
    if !cursor.is_empty() {
        return Err(BrowseError::Malformed);
    }
    Ok(BrowseClaim {
        connector_id,
        expires_at,
    })
}

fn take_byte(cursor: &mut &[u8]) -> Result<u8, BrowseError> {
    let (first, rest) = cursor.split_first().ok_or(BrowseError::Malformed)?;
    *cursor = rest;
    Ok(*first)
}

fn take_field<'a>(cursor: &mut &'a [u8]) -> Result<&'a [u8], BrowseError> {
    let (len_bytes, rest) = cursor.split_at_checked(4).ok_or(BrowseError::Malformed)?;
    let len =
        u32::from_be_bytes(len_bytes.try_into().map_err(|_| BrowseError::Malformed)?) as usize;
    let (field, rest) = rest.split_at_checked(len).ok_or(BrowseError::Malformed)?;
    *cursor = rest;
    Ok(field)
}

fn mac(secret: &Secret<String>, payload: &[u8]) -> Option<Vec<u8>> {
    let mut hmac = <Hmac<Sha256>>::new_from_slice(secret.expose().as_bytes()).ok()?;
    hmac.update(payload);
    Some(hmac.finalize().into_bytes().to_vec())
}
