//! Which signing identity a tenant's derivatives are signed with (G10·3b).
//!
//! One deployment used to sign every tenant's derivatives with one identity, from
//! `security.signing_cert_pem`. That is correct for a dedicated install and is the furthest-reaching of the
//! four key purposes to get wrong on a shared one: a signing identity is what a *consumer* verifies a
//! provenance claim against, so two customers' assets signed by one certificate assert to anybody who checks
//! that they came from the same publisher.
//!
//! ## Resolved per job, not cached
//!
//! Building an identity costs a database read and a PEM parse. A derive job renders an image, which is orders
//! of magnitude more, so the resolution is not worth caching — and a cache here would be worth *less* than
//! nothing, because the invalidation is the part that goes wrong: a rotated identity that keeps signing under
//! the old certificate until a worker restarts is exactly the failure rotation exists to prevent, and it
//! reports success the whole time.
//!
//! ## Falling back is deliberate
//!
//! A tenant with no identity of its own signs with the deployment's, as it did before this existed. The
//! alternative — refusing to sign — would turn "this tenant has not been given a certificate" into
//! "this tenant's derivatives carry no provenance", which is a worse answer and a silent one.
//!
//! An identity that is present but *unusable* is different, and is not a fallback case: a certificate that
//! cannot be parsed, or a key that will not unseal, means somebody configured something and it is broken.
//! Falling through to the deployment's identity there would sign the tenant's assets as somebody else while
//! the operator believed their own certificate was in use, so it renders unsigned instead and says why.

use dam_core::TenantSlug;
use dam_core::sealed::SealingKeyring;
use dam_media::provenance::SigningIdentity;
use uuid::Uuid;

/// The identity this tenant's derivatives should be signed with.
///
/// `None` means render without content credentials — either because nothing is configured anywhere, or
/// because what is configured for this tenant is broken. The two are distinguished in the log, not in the
/// return, because the caller does the same thing in both cases.
///
/// # Errors
/// Only a database failure. A broken identity is logged and reported as `None`, because a certificate problem
/// must not stop a library producing thumbnails — the same posture `dam-worker` takes at startup.
pub async fn for_tenant<'a>(
    global: &sqlx::PgPool,
    slug: &TenantSlug,
    tenant_id: Uuid,
    sealing: Option<&SealingKeyring>,
    deployment: Option<&'a SigningIdentity>,
) -> Result<Resolved<'a>, dam_db::Error> {
    let Some(sealing) = sealing else {
        // No keyring means no sealed key can be opened, so a tenant identity cannot exist in a usable form.
        // Not an error: a deployment that seals nothing has not asked for per-tenant signing.
        return Ok(Resolved::Deployment(deployment));
    };

    let mut conn = dam_db::TenantConn::begin(global, slug).await?;
    let found = dam_db::signing_identities::active(conn.executor()).await?;
    conn.commit().await?;

    let Some(row) = found else {
        return Ok(Resolved::Deployment(deployment));
    };

    let opened = match sealing.open(&row.sealed_key, &row.associated_data(slug.as_str())) {
        Ok(key) => key,
        Err(error) => {
            tracing::error!(
                %tenant_id, identity = %row.id, %error,
                "the tenant's signing key could not be opened; derivatives will be unsigned rather than \
                 signed as somebody else",
            );
            return Ok(Resolved::Unusable);
        }
    };

    match SigningIdentity::from_pem(
        row.cert_pem.as_bytes(),
        opened.expose().as_bytes(),
        &row.algorithm,
        row.timestamp_authority.clone(),
    ) {
        Ok(identity) => Ok(Resolved::Tenant(Box::new(identity))),
        Err(error) => {
            tracing::error!(
                %tenant_id, identity = %row.id, %error,
                "the tenant's signing certificate is unusable; derivatives will be unsigned",
            );
            Ok(Resolved::Unusable)
        }
    }
}

/// Which identity applies, kept as an enum so the caller cannot accidentally fall back past a broken one.
///
/// The `Unusable` case is the reason this is not just an `Option`: "the tenant has none" and "the tenant has
/// one and it is broken" must not both collapse to "use the deployment's", because the second would sign one
/// customer's assets under another's certificate while the operator believed otherwise.
#[derive(Debug)]
pub enum Resolved<'a> {
    /// The tenant's own. Boxed because it is large and this enum is returned by value.
    Tenant(Box<SigningIdentity>),
    /// The tenant has none of its own; the deployment's applies, which may itself be absent.
    Deployment(Option<&'a SigningIdentity>),
    /// The tenant has one and it cannot be used. Render unsigned.
    Unusable,
}

impl Resolved<'_> {
    /// The identity to sign with, if any.
    #[must_use]
    pub fn identity(&self) -> Option<&SigningIdentity> {
        match self {
            Self::Tenant(identity) => Some(identity),
            Self::Deployment(identity) => *identity,
            Self::Unusable => None,
        }
    }
}
