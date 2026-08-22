//! A tenant's own sealed provider credentials (M5a·2).
//!
//! The storage is one table; what is worth defending is the set of promises around it:
//!
//! - **The plaintext is never in the database.** A `SELECT` yields ciphertext, and the round trip only closes for
//!   a caller holding the deployment's sealing keyring.
//! - **A row copied elsewhere does not open.** The key is bound to `{tenant}:{provider}:{id}`, so moving it,
//!   relabelling its provider or duplicating it into a new row all fail closed.
//! - **Exactly one active default**, and withdrawing the default clears the flag rather than leaving a default
//!   nobody can use.
//! - **A rotation can be reported without opening anything**, which is the point of keeping the sealing key id in
//!   its own column.
//! - **The database is the specification** for a usable credential: an OpenAI-compatible row without a base URL
//!   has no vendor, and a `sealed_key` that is not sealed is a row nothing could ever open.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_core::Secret;
use dam_core::sealed::{OpenError, SealingKeyring};
use dam_db::ai_credentials::{self, CredentialRefusal, NewCredential, Provider};
use dam_db::{migrate, testing::PostgresHarness};
use sqlx::PgPool;
use uuid::Uuid;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

fn ring() -> SealingKeyring {
    SealingKeyring::single("k1", &Secret::new("a deployment sealing key".to_owned()))
}

/// Seals a key the way the API layer does, and returns the row to store.
fn credential(
    ring: &SealingKeyring,
    provider: Provider,
    label: &str,
    plaintext: &str,
    make_default: bool,
) -> NewCredential {
    // The id first, because it is part of the associated data — a database-generated id would force a
    // seal-then-update, and a failure between the two would leave ciphertext bound to an id the row does not have.
    let id = Uuid::new_v4();
    let secret = Secret::new(plaintext.to_owned());
    let aad = ai_credentials::associated_data("acme", provider.as_str(), id);
    NewCredential {
        id,
        provider,
        label: label.to_owned(),
        base_url: match provider {
            Provider::OpenAiCompatible => Some("https://api.moonshot.cn/v1".to_owned()),
            Provider::Anthropic => None,
        },
        sealed_key: ring.seal(&secret, &aad).expect("seal"),
        sealing_key_id: "k1".to_owned(),
        hint: dam_core::sealed::hint(&secret),
        default_model: match provider {
            Provider::Anthropic => "claude-opus-5".to_owned(),
            Provider::OpenAiCompatible => "moonshot-v1-128k".to_owned(),
        },
        make_default,
    }
}

#[tokio::test]
async fn the_credential_store_behaves() {
    let (_pg, pool) = db().await;

    a_key_round_trips_and_the_database_never_holds_it(&pool).await;
    a_row_copied_or_relabelled_does_not_open(&pool).await;
    there_is_at_most_one_active_default(&pool).await;
    withdrawing_the_default_clears_the_flag(&pool).await;
    a_rotation_is_reportable_without_opening_anything(&pool).await;
    the_database_refuses_an_unusable_credential(&pool).await;
}

async fn a_key_round_trips_and_the_database_never_holds_it(pool: &PgPool) {
    let ring = ring();
    let new = credential(
        &ring,
        Provider::Anthropic,
        "Anthropic",
        "sk-ant-secret-1234",
        true,
    );
    let stored = ai_credentials::add(c!(pool), &new).await.expect("add");

    // The round trip, for a caller holding the keyring.
    let opened = ring
        .open(&stored.sealed_key, &stored.associated_data("acme"))
        .expect("open");
    assert_eq!(opened.expose(), "sk-ant-secret-1234");

    // And the plaintext is nowhere in the row. Asserted against the *whole* row rendered as text, because the
    // failure this guards against is a key that leaks through some other column — a label somebody pasted it
    // into, a hint that took too much of it.
    let rendered: String =
        sqlx::query_scalar("SELECT ai_credentials::text FROM ai_credentials WHERE id = $1")
            .bind(stored.id)
            .fetch_one(pool)
            .await
            .expect("row as text");
    assert!(
        !rendered.contains("sk-ant-secret-1234"),
        "the plaintext key is in the row: {rendered}"
    );
    // The hint shows four characters so two keys can be told apart, and no more.
    assert_eq!(stored.hint, "…1234");
    assert!(
        stored.sealed_key.starts_with("v1.k1."),
        "{}",
        stored.sealed_key
    );
    assert_eq!(stored.provider(), Some(Provider::Anthropic));
    assert_eq!(stored.default_model, "claude-opus-5");
}

async fn a_row_copied_or_relabelled_does_not_open(pool: &PgPool) {
    let ring = ring();
    let new = credential(
        &ring,
        Provider::OpenAiCompatible,
        "Kimi",
        "sk-kimi-secret-9876",
        false,
    );
    let stored = ai_credentials::add(c!(pool), &new).await.expect("add");

    // Another tenant's schema is a different tenant slug, which is in the associated data.
    assert_eq!(
        ring.open(
            &stored.sealed_key,
            &ai_credentials::associated_data("globex", &stored.provider, stored.id)
        ),
        Err(OpenError::Refused),
        "a credential copied into another tenant opened"
    );
    // Relabelled as another provider: the client that would then sign with it is the wrong one, so it fails.
    assert_eq!(
        ring.open(
            &stored.sealed_key,
            &ai_credentials::associated_data("acme", "anthropic", stored.id)
        ),
        Err(OpenError::Refused)
    );
    // Duplicated into a new row: the id is in the associated data, so the copy is dead.
    assert_eq!(
        ring.open(
            &stored.sealed_key,
            &ai_credentials::associated_data("acme", &stored.provider, Uuid::new_v4())
        ),
        Err(OpenError::Refused)
    );
}

async fn there_is_at_most_one_active_default(pool: &PgPool) {
    let ring = ring();
    // The first credential added with `make_default` is the default; a second one displaces it rather than
    // failing against the unique index, which would tell a tenant their new key was invalid when the real
    // answer is "there is already a default".
    let second = credential(
        &ring,
        Provider::OpenAiCompatible,
        "OpenAI",
        "sk-openai-secret-4321",
        true,
    );
    let second = ai_credentials::add(c!(pool), &second).await.expect("add");
    assert!(second.is_default);

    let current = ai_credentials::current(c!(pool))
        .await
        .expect("current")
        .expect("a default");
    assert_eq!(current.id, second.id);

    let all = ai_credentials::all(c!(pool)).await.expect("all");
    assert_eq!(
        all.iter().filter(|row| row.is_default).count(),
        1,
        "{all:#?}"
    );
    // Defaults first, so a list opens with the one in use.
    assert!(all[0].is_default);

    // And promoting another displaces it too.
    let other = all
        .iter()
        .find(|row| !row.is_default && row.is_active)
        .expect("another active one");
    let promoted = ai_credentials::make_default(c!(pool), other.id)
        .await
        .expect("promote");
    assert!(promoted.is_default);
    assert_eq!(
        ai_credentials::all(c!(pool))
            .await
            .expect("all")
            .iter()
            .filter(|row| row.is_default)
            .count(),
        1
    );
}

async fn withdrawing_the_default_clears_the_flag(pool: &PgPool) {
    let current = ai_credentials::current(c!(pool))
        .await
        .expect("current")
        .expect("a default");

    let withdrawn = ai_credentials::set_active(c!(pool), current.id, false)
        .await
        .expect("withdraw");
    assert!(!withdrawn.is_active);
    assert!(
        !withdrawn.is_default,
        "a withdrawn credential kept the default flag, so enrichment would pick one nobody meant to be live"
    );
    assert!(
        ai_credentials::current(c!(pool))
            .await
            .expect("current")
            .is_none(),
        "there is a default nobody can use"
    );

    // And it cannot be made the default again while withdrawn — with the reason, rather than a constraint error
    // an operator has to decode.
    let refusal = ai_credentials::make_default(c!(pool), current.id)
        .await
        .expect_err("withdrawn");
    assert!(
        matches!(&refusal, CredentialRefusal::Invalid(reason) if reason.contains("withdrawn")),
        "{refusal:?}"
    );

    // Restoring it does not silently restore the default either: that is a separate decision.
    let restored = ai_credentials::set_active(c!(pool), current.id, true)
        .await
        .expect("restore");
    assert!(restored.is_active);
    assert!(!restored.is_default);
    ai_credentials::make_default(c!(pool), current.id)
        .await
        .expect("promote");
}

async fn a_rotation_is_reportable_without_opening_anything(pool: &PgPool) {
    // The operator's question during a sealing-key rotation: what is left. Answered from a column, so it can be
    // answered for rows this process cannot decrypt — which is the case that matters, since a half-configured
    // keyring is exactly when somebody asks.
    let outstanding = ai_credentials::sealed_under_other_keys(c!(pool), "k1")
        .await
        .expect("report");
    assert!(outstanding.is_empty(), "{outstanding:#?}");

    let all = ai_credentials::all(c!(pool)).await.expect("all");
    let target = all.first().expect("a credential");

    // Re-seal one under a new key, as a rotation pass would.
    let rotated = SealingKeyring::single("k2", &Secret::new("the next sealing key".to_owned()))
        .with_retired("k1", &Secret::new("a deployment sealing key".to_owned()));
    let plaintext = rotated
        .open(&target.sealed_key, &target.associated_data("acme"))
        .expect("open under the retired key");
    let resealed = rotated
        .seal(&plaintext, &target.associated_data("acme"))
        .expect("seal");
    let after = ai_credentials::replace_key(
        c!(pool),
        target.id,
        &resealed,
        "k2",
        &dam_core::sealed::hint(&plaintext),
    )
    .await
    .expect("replace");
    assert_eq!(after.sealing_key_id, "k2");

    // The report now names the rows still on the old key, and not the one just moved.
    let outstanding = ai_credentials::sealed_under_other_keys(c!(pool), "k2")
        .await
        .expect("report");
    assert!(!outstanding.is_empty());
    assert!(
        outstanding.iter().all(|row| row.id != target.id),
        "the re-sealed row is still reported as outstanding"
    );
    // And what it holds still opens, under the same associated data as before — the id did not change, which is
    // the reason rotation is a `replace_key` rather than a delete and an add.
    assert_eq!(
        rotated
            .open(&after.sealed_key, &after.associated_data("acme"))
            .expect("open")
            .expose(),
        plaintext.expose()
    );

    // An unknown id is `Unknown`, not a silent no-op: an operator whose rotation quietly skipped a row would
    // believe it finished.
    assert!(matches!(
        ai_credentials::replace_key(c!(pool), Uuid::new_v4(), &resealed, "k2", "").await,
        Err(CredentialRefusal::Unknown(_))
    ));
}

async fn the_database_refuses_an_unusable_credential(pool: &PgPool) {
    let ring = ring();

    // An OpenAI-compatible credential with no base URL has no vendor: the URL is what distinguishes OpenAI from
    // Kimi from a local server, so without it nothing could route the request.
    let mut vendorless = credential(
        &ring,
        Provider::OpenAiCompatible,
        "Nowhere",
        "sk-secret-0000",
        false,
    );
    vendorless.base_url = None;
    assert!(
        matches!(
            ai_credentials::add(c!(pool), &vendorless).await,
            Err(CredentialRefusal::Invalid(_))
        ),
        "a credential with no endpoint was accepted"
    );

    // A `sealed_key` that is not a sealed value is a row nothing could ever open, and no code path would notice
    // until somebody tried to enrich.
    let mut plaintext_key = credential(
        &ring,
        Provider::Anthropic,
        "Careless",
        "sk-secret-1111",
        false,
    );
    plaintext_key.sealed_key = "sk-ant-this-is-not-sealed".to_owned();
    assert!(matches!(
        ai_credentials::add(c!(pool), &plaintext_key).await,
        Err(CredentialRefusal::Invalid(_))
    ));

    // A label somebody has to read, and a model something has to call.
    let mut unlabelled = credential(&ring, Provider::Anthropic, "   ", "sk-secret-2222", false);
    unlabelled.label = "   ".to_owned();
    assert!(matches!(
        ai_credentials::add(c!(pool), &unlabelled).await,
        Err(CredentialRefusal::Invalid(_))
    ));
    let mut modelless = credential(
        &ring,
        Provider::Anthropic,
        "No model",
        "sk-secret-3333",
        false,
    );
    modelless.default_model = String::new();
    assert!(matches!(
        ai_credentials::add(c!(pool), &modelless).await,
        Err(CredentialRefusal::Invalid(_))
    ));

    // And a base URL that is not one.
    let mut nonsense_url = credential(
        &ring,
        Provider::OpenAiCompatible,
        "Odd",
        "sk-secret-4444",
        false,
    );
    nonsense_url.base_url = Some("moonshot.cn".to_owned());
    assert!(matches!(
        ai_credentials::add(c!(pool), &nonsense_url).await,
        Err(CredentialRefusal::Invalid(_))
    ));
}
