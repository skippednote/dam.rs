//! From a stored credential to a client that can be asked (M5a·4).
//!
//! One function, and the reason it is worth a module is what it refuses.
//!
//! A credential is a row: a provider, a base URL, a model name and a sealed key. Turning it into a
//! [`crate::model::Model`] means opening the key, which can fail for reasons a caller must be able to tell
//! apart — the wrong keyring is a deployment fault an operator fixes, a provider this build does not know is an
//! older binary reading a newer row, and a missing base URL is a credential that was never usable. All three
//! would otherwise arrive as one opaque failure at enrichment time, which is the worst place to diagnose them.
//!
//! ## The plaintext key exists for one expression
//!
//! [`open`] takes the keyring, opens the key, hands it to the client's constructor and drops its own copy. The
//! client holds it in a [`Secret`], which cannot be printed. No caller of this module ever sees plaintext, which
//! is the same promise `dam_db::ai_credentials` makes from the other side.

use crate::anthropic::AnthropicModel;
use crate::model::{Model, Transport};
use crate::openai_compatible::OpenAiCompatibleModel;
use dam_core::sealed::{OpenError, SealingKeyring};
use dam_db::ai_credentials::{Credential, Provider};
use std::sync::Arc;

/// Why a stored credential could not become a client.
#[derive(Debug, thiserror::Error)]
pub enum UnusableCredential {
    /// The row names a provider this build has no client for.
    ///
    /// Almost always an older binary against a newer schema during a rolling deploy. Refused rather than
    /// approximated: guessing a wire format would send a tenant's key to a vendor it was not issued for.
    #[error("this build has no client for the provider {0}")]
    UnknownProvider(String),

    /// The key could not be opened.
    ///
    /// Either the sealing key that wrote it is not on this keyring — a deployment that rotated without keeping
    /// the retired key — or the ciphertext has been moved between rows and its associated data no longer
    /// matches. Both are operator-visible faults rather than anything a retry fixes.
    #[error("the stored key could not be opened: {0}")]
    Sealed(#[from] OpenError),

    /// An OpenAI-compatible credential with no endpoint.
    ///
    /// There is no default: the prefix before `/chat/completions` differs per vendor and per deployment, and a
    /// guess would send the key to whoever owns the URL that was guessed.
    #[error(
        "an openai-compatible credential needs a base url; there is no default to fall back on"
    )]
    NoEndpoint,
}

/// Builds a client for a stored credential.
///
/// `tenant` is the schema name the row lives in: it is part of the associated data the key was sealed under, so
/// a ciphertext read from one tenant's schema cannot be opened as another's even by a caller holding the
/// keyring.
///
/// `model` overrides the credential's default — that is §8.3's "model routing per pipeline stage is
/// configuration, not code", and the reason bulk classification can run on a cheap model while alt text does
/// not.
pub fn open(
    credential: &Credential,
    tenant: &str,
    keyring: &SealingKeyring,
    transport: Arc<dyn Transport>,
    model: Option<&str>,
) -> Result<Box<dyn Model>, UnusableCredential> {
    let provider = credential
        .provider()
        .ok_or_else(|| UnusableCredential::UnknownProvider(credential.provider.clone()))?;
    let key = keyring.open(&credential.sealed_key, &credential.associated_data(tenant))?;
    let model_name = model.unwrap_or(&credential.default_model);

    Ok(match provider {
        Provider::Anthropic => Box::new(AnthropicModel::new(
            transport,
            key,
            credential.base_url.as_deref(),
            model_name,
        )),
        Provider::OpenAiCompatible => {
            let base = credential
                .base_url
                .as_deref()
                .ok_or(UnusableCredential::NoEndpoint)?;
            Box::new(OpenAiCompatibleModel::new(transport, key, base, model_name))
        }
    })
}
