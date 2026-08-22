//! Tenant provisioning.
//!
//! Four things have to happen together: a control-plane row, a schema, the tenant
//! migrations, and seed data. They cannot all be one transaction — `CREATE SCHEMA`
//! and the migrator run on their own connections — so the ordering is chosen to make
//! partial failure recoverable rather than to pretend it cannot happen.
//!
//! ## Ordering
//!
//! Schema and migrations first, control-plane row last. A schema with no tenant row
//! is inert: nothing looks for it, and re-running provisioning adopts it. The reverse
//! — a tenant row pointing at a schema that does not exist — is worse, because every
//! request for that tenant then fails deep in the stack instead of at lookup, and the
//! tenant appears in listings as if it worked.
//!
//! ## Seeded state is a guarantee
//!
//! `face_identify` is seeded **off** and DPIA-gated (D14). A tenant that had to
//! remember to disable biometric identification would be processing Article 9 data by
//! default, which is the wrong way round.

use crate::{Error, migrate};
use dam_core::TenantSlug;
use sqlx::PgPool;
use uuid::Uuid;

/// Where a new tenant's objects go.
///
/// Passed in rather than read from configuration here, because `dam-db` has no business knowing how the
/// deployment is configured — and because a test provisions against its own container. The fields mirror
/// `dam_global.storage_pools`, minus the ones this function fills in.
#[derive(Debug, Clone, Copy)]
pub struct StoragePool<'a> {
    /// `None` for AWS, which resolves from the region.
    pub endpoint: Option<&'a str>,
    pub region: &'a str,
    pub bucket: &'a str,
    /// Required for SeaweedFS, MinIO, Ceph RGW and every other non-AWS endpoint.
    pub force_path_style: bool,
    /// Where the credential lives — a 1Password item, an SSM path, an environment variable name. Never the
    /// credential itself.
    pub credentials_ref: &'a str,
}

/// A provisioned tenant.
#[derive(Debug, Clone)]
pub struct Tenant {
    pub id: Uuid,
    pub slug: TenantSlug,
    pub schema_name: String,
    pub storage_prefix: String,
}

/// Provisions a tenant, or returns the existing one.
///
/// Idempotent: a retried run — a crashed CLI, a re-run CI job — adopts whatever
/// already exists rather than failing or duplicating. Every step is written to be
/// safely repeatable, which is cheaper than a rollback path that itself has to be
/// correct under partial failure.
pub async fn tenant(
    pool: &PgPool,
    url: &str,
    slug: &TenantSlug,
    display_name: &str,
    storage: &StoragePool<'_>,
) -> Result<Tenant, Error> {
    let schema_name = slug.schema_name();

    // Already provisioned? Return it. Checked first so the common retry path does no
    // work at all.
    if let Some(existing) = find(pool, slug).await? {
        return Ok(existing);
    }

    // 1. Schema + migrations. Inert if we fail after this: no tenant row points here,
    //    and a later run adopts it.
    migrate::tenant(url, &schema_name).await?;

    // 2. Seed data, before the tenant row exists. Same reasoning — a seeded schema
    //    with no row is invisible, a registered tenant with no seed data is broken.
    seed_tenant_schema(pool, &schema_name).await?;

    // 3. Control-plane row, last, and the flags with it in one transaction so a
    //    tenant can never exist without its DPIA-gated flags.
    let id = Uuid::now_v7();
    let storage_prefix = format!("{id}/");
    let mut tx = pool.begin().await?;

    sqlx::query(
        "INSERT INTO dam_global.tenants \
           (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES ($1, $2, $3, $4, $5, 'active')",
    )
    .bind(id)
    .bind(slug.as_str())
    .bind(&schema_name)
    .bind(display_name)
    .bind(&storage_prefix)
    .execute(&mut *tx)
    .await?;

    seed_feature_flags(&mut tx, id).await?;

    // The tenant's hot pool, in the same transaction as the tenant row. Without one, ingest gets as far as
    // recording a placement and refuses — which is exactly what happened the first time an upload ran through
    // the real pipeline: the job failed with "no instant storage pool is configured" and went straight to
    // `dead`. A tenant that cannot store anything is not provisioned, so it is created here rather than left
    // for an operator to notice.
    sqlx::query(
        "INSERT INTO dam_global.storage_pools \
           (id, tenant_id, name, driver, endpoint, region, bucket, force_path_style, \
            credentials_ref, storage_class, latency_class) \
         VALUES (gen_random_uuid(), $1, 'hot', 's3', $2, $3, $4, $5, $6, 'STANDARD', 'instant')",
    )
    .bind(id)
    .bind(storage.endpoint)
    .bind(storage.region)
    .bind(storage.bucket)
    .bind(storage.force_path_style)
    // A *reference*, never a credential. The pool row says where to find the secret; the secret itself lives
    // where secrets live, and a connection string in a table is a connection string in every backup.
    .bind(storage.credentials_ref)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Tenant {
        id,
        slug: slug.clone(),
        schema_name,
        storage_prefix,
    })
}

/// Looks up a tenant by slug.
pub async fn find(pool: &PgPool, slug: &TenantSlug) -> Result<Option<Tenant>, Error> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, schema_name, storage_prefix FROM dam_global.tenants WHERE slug = $1",
    )
    .bind(slug.as_str())
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, schema_name, storage_prefix)| Tenant {
        id,
        slug: slug.clone(),
        schema_name,
        storage_prefix,
    }))
}

/// Feature flags, created with the tenant row so the gated ones cannot be missing.
///
/// `requires_dpia` is a property of the **feature**, so it is set here rather than
/// left to an operator. Combined with the CHECK on `feature_flags`, that makes
/// `face_identify` unenableable without a DPIA reference and a recorded legal basis
/// even for someone with direct database access.
async fn seed_feature_flags(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
) -> Result<(), Error> {
    // (key, enabled, requires_dpia)
    const FLAGS: &[(&str, bool, bool)] = &[
        // D14: biometric identification is off by default and gated. Face
        // *detection* — blur, crop-to-subject, counting — is a separate, ungated
        // capability; only identification is Article 9 data.
        ("face_identify", false, true),
        ("ai_enrichment", true, false),
        ("nl_search", true, false),
        ("mcp_server", false, false),
    ];

    for (key, enabled, requires_dpia) in FLAGS {
        sqlx::query(
            "INSERT INTO dam_global.feature_flags (tenant_id, key, enabled, requires_dpia) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (tenant_id, key) DO NOTHING",
        )
        .bind(tenant_id)
        .bind(key)
        .bind(enabled)
        .bind(requires_dpia)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Seeds the tenant schema's starting state.
///
/// Runs through a `search_path`-scoped connection rather than qualifying every
/// statement, so the SQL here reads the same as the SQL a handler writes. `ON
/// CONFLICT DO NOTHING` throughout keeps a retried provisioning from duplicating.
async fn seed_tenant_schema(pool: &PgPool, schema: &str) -> Result<(), Error> {
    let mut tx = pool.begin().await?;

    // Scoped to this transaction, so nothing leaks onto the pooled connection —
    // the same rule TenantConn enforces structurally.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL search_path TO \"{schema}\", dam_global, extensions, public"
    )))
    .execute(&mut *tx)
    .await?;

    // The default asset group. The ABAC compiler (0.10) resolves an unscoped grant
    // through this, so exactly one group must carry `is_default`.
    sqlx::query(
        "INSERT INTO asset_groups (id, key, label, is_default) \
         VALUES ($1, 'everyone', 'Everyone', true) ON CONFLICT (key) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await?;

    // Built-in roles. `all_asset_groups` on admin rather than an enumerated list, so
    // a new group does not silently fall outside the administrator's reach.
    for (key, label, perms, all_groups) in [
        (
            "admin",
            "Administrator",
            vec!["asset:*", "metadata:*", "tenant:*", "rights:*"],
            true,
        ),
        (
            "contributor",
            "Contributor",
            vec!["asset:read", "asset:create", "metadata:write"],
            false,
        ),
        ("viewer", "Viewer", vec!["asset:read"], false),
    ] {
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, all_asset_groups, is_builtin) \
             VALUES ($1, $2, $3, $4, $5, true) ON CONFLICT (key) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(key)
        .bind(label)
        .bind(&perms)
        .bind(all_groups)
        .execute(&mut *tx)
        .await?;
    }

    // A minimal metadata schema. Deliberately small: the field set is the customer's
    // to define, and a long opinionated default is something they then have to delete.
    // `alt_text` is here because accessibility (D10) needs somewhere for AI-generated
    // alt text to land with provenance, and a tenant should not have to invent it.
    for (key, label, kind, searchable, ai_writable) in [
        ("title", "Title", "text", true, true),
        ("description", "Description", "textarea", true, true),
        ("alt_text", "Alt text", "text", true, true),
        ("copyright", "Copyright", "text", false, false),
        ("credit", "Credit", "text", false, false),
    ] {
        sqlx::query(
            "INSERT INTO field_defs (id, key, label, kind, searchable, ai_writable) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (key) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(key)
        .bind(label)
        .bind(kind)
        .bind(searchable)
        .bind(ai_writable)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}
