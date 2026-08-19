//! Named download formats (Q.11a).
//!
//! The storage is one ordinary table. What is worth testing is the set of promises made about it:
//!
//! - **A redefinition renders fresh.** The cache key is the recipe, so changing any field of a conversion must
//!   change its `op_hash` — otherwise every asset keeps being served the bytes rendered under the old
//!   definition, silently and forever. That is the failure the whole design is arranged around.
//! - **The key is not editable.** A delivery token carries it, so a rename would strand links that were valid
//!   when they were sent.
//! - **Withdrawn is not deleted.** What is offered shrinks; what has been rendered stays resolvable.
//! - **The database is the specification for a usable recipe.** A 0×0 rendition, a quality of 500, an unknown
//!   format, a key called `original`: each is refused by a constraint, and the refusal names it.
//! - **A permission narrows and never widens.**
//!
//! One container; cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::conversions::{self, ConversionRefusal, NewConversion};
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

/// A connection from the pool. The pool is already pinned to the tenant schema.
macro_rules! c {
    ($pool:expr) => {
        &mut *$pool.acquire().await.expect("connection")
    };
}

fn spec(key: &str) -> NewConversion {
    NewConversion {
        key: key.to_owned(),
        label: "Web JPEG".to_owned(),
        description: "Sized for a web page, and small enough to email.".to_owned(),
        media_class: "image".to_owned(),
        max_width: 2048,
        max_height: 2048,
        format: "jpeg".to_owned(),
        quality: 82,
        fit: "contain".to_owned(),
        background: "ffffff".to_owned(),
        required_permission: None,
        sort_order: 0,
    }
}

#[tokio::test]
async fn the_conversion_set_behaves() {
    let (_pg, pool) = db().await;

    a_redefinition_changes_the_cache_key(&pool).await;
    a_duplicate_key_is_named_rather_than_a_database_error(&pool).await;
    the_key_survives_a_redefinition(&pool).await;
    withdrawing_hides_it_from_offers_and_keeps_it_resolvable(&pool).await;
    the_database_refuses_an_unusable_recipe(&pool).await;
    a_permission_narrows_what_is_offered(&pool).await;
    the_offer_order_is_the_configured_one(&pool).await;
    only_the_assets_own_class_is_offered(&pool).await;
    an_administrators_list_shows_the_withdrawn_ones(&pool).await;
}

async fn a_redefinition_changes_the_cache_key(pool: &PgPool) {
    // The property everything else is arranged around, and the reason there is no revision column: the recipe
    // *is* the key, so an edit cannot be served from the old cache.
    let created = conversions::create(c!(pool), &spec("web-2048"), None)
        .await
        .expect("create");
    let before = created.op_hash().expect("renderable");

    let mut smaller = spec("web-2048");
    smaller.max_width = 1024;
    smaller.max_height = 1024;
    let after = conversions::redefine(c!(pool), created.id, &smaller)
        .await
        .expect("redefine")
        .op_hash()
        .expect("renderable");
    assert_ne!(
        before, after,
        "a redefined conversion keeps its cache key, so every asset keeps being served the old bytes"
    );

    // Quality alone, which is the change most likely to be thought cosmetic.
    let mut lossier = smaller.clone();
    lossier.quality = 60;
    let lossier_hash = conversions::redefine(c!(pool), created.id, &lossier)
        .await
        .expect("redefine")
        .op_hash()
        .expect("renderable");
    assert_ne!(after, lossier_hash, "quality is not in the key");

    // And a change that is genuinely only presentation does *not* move it: the label and description are what a
    // person reads, not what the renderer does, and moving the key would re-render every asset for a typo fix.
    let mut relabelled = lossier.clone();
    relabelled.label = "Web JPEG (small)".to_owned();
    relabelled.description = "A smaller web size.".to_owned();
    relabelled.sort_order = 3;
    let relabelled_hash = conversions::redefine(c!(pool), created.id, &relabelled)
        .await
        .expect("redefine")
        .op_hash()
        .expect("renderable");
    assert_eq!(
        lossier_hash, relabelled_hash,
        "renaming a conversion re-renders every asset"
    );
}

async fn a_duplicate_key_is_named_rather_than_a_database_error(pool: &PgPool) {
    // Two administrators naming a format the same thing on the same afternoon is ordinary, and the second one
    // needs to be told which word to change rather than shown a 500.
    let refusal = conversions::create(c!(pool), &spec("web-2048"), None)
        .await
        .expect_err("the key is taken");
    assert!(
        matches!(&refusal, ConversionRefusal::DuplicateKey(key) if key == "web-2048"),
        "{refusal:?}"
    );
}

async fn the_key_survives_a_redefinition(pool: &PgPool) {
    // A delivery token carries the key. `redefine` takes a whole definition including a key field, and it must
    // ignore that field — otherwise a link sent last week stops resolving because somebody tidied a name.
    let existing = conversions::by_key(c!(pool), "web-2048")
        .await
        .expect("read")
        .expect("present");
    let mut renamed = spec("something-else");
    renamed.max_width = 1600;
    renamed.max_height = 1600;
    let after = conversions::redefine(c!(pool), existing.id, &renamed)
        .await
        .expect("redefine");
    assert_eq!(after.key, "web-2048", "a redefinition renamed the format");
    assert_eq!(after.max_width, 1600, "and did not apply the recipe");
    assert!(
        conversions::by_key(c!(pool), "something-else")
            .await
            .expect("read")
            .is_none(),
        "the new name resolves to something"
    );
}

async fn withdrawing_hides_it_from_offers_and_keeps_it_resolvable(pool: &PgPool) {
    let doomed = conversions::create(c!(pool), &spec("print-png"), None)
        .await
        .expect("create");
    assert!(
        offered_keys(pool, &[])
            .await
            .contains(&"print-png".to_owned()),
        "not offered while active"
    );

    let withdrawn = conversions::set_active(c!(pool), doomed.id, false)
        .await
        .expect("withdraw");
    assert!(!withdrawn.is_active);
    assert!(
        !offered_keys(pool, &[])
            .await
            .contains(&"print-png".to_owned()),
        "a withdrawn conversion is still offered"
    );

    // Still resolvable by key, with the same recipe and so the same cache key: a link in somebody's email
    // points at bytes they were promised, and an administrator tidying a list must not break it.
    let still = conversions::by_key(c!(pool), "print-png")
        .await
        .expect("read")
        .expect("a withdrawn conversion is unresolvable, so an issued link 404s");
    assert_eq!(still.op_hash(), doomed.op_hash());

    // And restorable, because withdrawing by accident is a thing people do.
    conversions::set_active(c!(pool), doomed.id, true)
        .await
        .expect("restore");
    assert!(
        offered_keys(pool, &[])
            .await
            .contains(&"print-png".to_owned())
    );
    conversions::set_active(c!(pool), doomed.id, false)
        .await
        .expect("withdraw again");
}

async fn the_database_refuses_an_unusable_recipe(pool: &PgPool) {
    // The CHECK constraints are the specification for what a recipe is. Each case here would otherwise reach
    // the renderer as work that cannot be done, at the moment somebody is waiting for a download.
    /// One way to break a recipe: a name for the failure, and the edit that causes it.
    type Break = (&'static str, fn(&mut NewConversion));

    let cases: [Break; 8] = [
        ("a zero width", |s| s.max_width = 0),
        ("an absurd width", |s| s.max_width = 40_000),
        ("a quality above 100", |s| s.quality = 500),
        ("a quality of zero", |s| s.quality = 0),
        ("an unknown format", |s| s.format = "tiff".to_owned()),
        ("an unknown fit", |s| s.fit = "letterbox".to_owned()),
        ("a background that is not hex", |s| {
            s.background = "white".to_owned();
        }),
        // The reserved name. `original` is the untransformed bytes, and a row claiming it would shadow them at
        // the one place that resolves a transform — a failure that would read as a caching bug.
        ("the reserved key", |s| s.key = "original".to_owned()),
    ];

    for (label, break_it) in cases {
        let mut broken = spec(&format!("broken-{}", label.replace(' ', "-")));
        break_it(&mut broken);
        let refusal = conversions::create(c!(pool), &broken, None)
            .await
            .expect_err(label);
        assert!(
            matches!(refusal, ConversionRefusal::Invalid(_)),
            "{label} was not refused as invalid: {refusal:?}"
        );
        // And the refusal names the constraint, so an administrator is told which field rather than "no".
        if let ConversionRefusal::Invalid(named) = refusal {
            assert!(
                !named.is_empty(),
                "{label} was refused without naming what it broke"
            );
        }
    }

    // A media class nothing can render is refused too. The CHECK is `image` alone because `derive::render` is
    // vips and there is no parameterised video recipe — a row for a class the worker cannot honour would be
    // offered in a dialog and fail at the moment of download.
    let mut video = spec("video-720");
    video.media_class = "video".to_owned();
    assert!(
        matches!(
            conversions::create(c!(pool), &video, None).await,
            Err(ConversionRefusal::Invalid(_))
        ),
        "a video conversion was accepted, and nothing can render one"
    );

    // An empty label or description is refused: a list of format names with nothing explaining which to pick
    // is what this table exists to replace.
    let mut blank = spec("blank-label");
    blank.label = "   ".to_owned();
    assert!(matches!(
        conversions::create(c!(pool), &blank, None).await,
        Err(ConversionRefusal::Invalid(_))
    ));
    let mut undescribed = spec("undescribed");
    undescribed.description = String::new();
    assert!(matches!(
        conversions::create(c!(pool), &undescribed, None).await,
        Err(ConversionRefusal::Invalid(_))
    ));
}

async fn a_permission_narrows_what_is_offered(pool: &PgPool) {
    let mut restricted = spec("print-tiff-sized");
    restricted.required_permission = Some("conversion:print".to_owned());
    restricted.sort_order = 9;
    conversions::create(c!(pool), &restricted, None)
        .await
        .expect("create");

    // Absent, not shown-and-refused. A list of formats you cannot have is a worse answer than a shorter list.
    let without = offered_keys(pool, &[]).await;
    assert!(
        !without.contains(&"print-tiff-sized".to_owned()),
        "a restricted format was offered to somebody without the permission: {without:?}"
    );

    let with = offered_keys(pool, &["conversion:print".to_owned()]).await;
    assert!(
        with.contains(&"print-tiff-sized".to_owned()),
        "the permission bought nothing: {with:?}"
    );

    // Holding *a* permission is not holding *this* one.
    let other = offered_keys(pool, &["conversion:web".to_owned()]).await;
    assert!(!other.contains(&"print-tiff-sized".to_owned()), "{other:?}");

    // And not a prefix of it. `conversion:print-extra` is a different permission, and matching it as this one
    // would hand out a format nobody granted — the whole point of naming permissions.
    let longer = offered_keys(pool, &["conversion:print-extra".to_owned()]).await;
    assert!(
        !longer.contains(&"print-tiff-sized".to_owned()),
        "a permission matched by prefix: {longer:?}"
    );

    // The unrestricted ones are still there for everybody — a permission narrows one format, not the list.
    assert!(without.contains(&"web-2048".to_owned()), "{without:?}");
}

async fn the_offer_order_is_the_configured_one(pool: &PgPool) {
    // Two unrestricted formats whose alphabetical order is the reverse of their configured order. With one
    // row — which is what this suite had until mutation testing pointed it out — any ORDER BY passes.
    let mut first = spec("zzz-thumbnail");
    first.label = "Small JPEG".to_owned();
    first.sort_order = 1;
    first.max_width = 512;
    first.max_height = 512;
    conversions::create(c!(pool), &first, None)
        .await
        .expect("create");

    // Both orders set here rather than inherited from what earlier cases left behind: a case whose premise is
    // a side effect of its neighbours passes or fails for reasons that have nothing to do with what it claims.
    let web = conversions::by_key(c!(pool), "web-2048")
        .await
        .expect("read")
        .expect("present");
    let mut later = spec("web-2048");
    later.sort_order = 5;
    conversions::redefine(c!(pool), web.id, &later)
        .await
        .expect("redefine");

    let offered = offered_keys(pool, &[]).await;
    let zzz = offered.iter().position(|key| key == "zzz-thumbnail");
    let web = offered.iter().position(|key| key == "web-2048");
    assert!(
        matches!((zzz, web), (Some(z), Some(w)) if z < w),
        "the offer order is alphabetical rather than configured: {offered:?}"
    );
}

async fn only_the_assets_own_class_is_offered(pool: &PgPool) {
    // Nothing is offered for a class with no conversions, rather than everything. An image recipe applied to a
    // PDF is not a download; it is a failure with a spinner in front of it.
    let documents = conversions::offerable(c!(pool), "document", &[])
        .await
        .expect("offerable");
    assert!(documents.is_empty(), "{documents:?}");

    let images = conversions::offerable(c!(pool), "image", &[])
        .await
        .expect("offerable");
    assert!(images.len() > 1, "one row makes the ordering unfalsifiable");
    assert!(images.iter().all(|row| row.media_class == "image"));
    // In the order somebody set, not alphabetical: a dialog lists a considered order.
    let orders: Vec<i32> = images.iter().map(|row| row.sort_order).collect();
    let mut sorted = orders.clone();
    sorted.sort_unstable();
    assert_eq!(
        orders, sorted,
        "the offer order is not the configured order"
    );
}

async fn an_administrators_list_shows_the_withdrawn_ones(pool: &PgPool) {
    // Administration is a different question from what to offer: somebody has to be able to see what they
    // withdrew in order to restore it.
    let all = conversions::all(c!(pool)).await.expect("all");
    assert!(
        all.iter()
            .any(|row| row.key == "print-png" && !row.is_active),
        "the withdrawn conversion is invisible to administration: {all:?}"
    );
    // Active first, so a list opens with what is in use.
    let first_withdrawn = all.iter().position(|row| !row.is_active);
    let last_active = all.iter().rposition(|row| row.is_active);
    if let (Some(withdrawn), Some(active)) = (first_withdrawn, last_active) {
        assert!(withdrawn > active, "withdrawn rows are mixed in: {all:?}");
    }
}

/// The keys offered for an image, for a caller holding `permissions`.
async fn offered_keys(pool: &PgPool, permissions: &[String]) -> Vec<String> {
    conversions::offerable(c!(pool), "image", permissions)
        .await
        .expect("offerable")
        .into_iter()
        .map(|row| row.key)
        .collect()
}

#[tokio::test]
async fn a_conversion_cannot_be_created_for_an_unknown_id() {
    let (_pg, pool) = db().await;
    // Redefining and withdrawing something that is not there is `Unknown`, not a silent no-op: an
    // administrator whose request quietly did nothing goes on believing it worked.
    let missing = Uuid::new_v4();
    assert!(matches!(
        conversions::redefine(c!(pool), missing, &spec("web-2048")).await,
        Err(ConversionRefusal::Unknown(id)) if id == missing
    ));
    assert!(matches!(
        conversions::set_active(c!(pool), missing, false).await,
        Err(ConversionRefusal::Unknown(id)) if id == missing
    ));
}
