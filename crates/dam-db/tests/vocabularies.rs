//! Vocabulary administration, and the gate that was never read (Q.20b).
//!
//! The property that matters most here is the one that had no test at all: **`taxonomies.ai_taggable` decides
//! what an LLM is told**. The column existed from 0001 and nothing read it, so
//! `dam_db::enrichment::vocabulary` offered a model every non-deprecated term in the tenant — including the
//! terms of *category trees*, which are filing structure rather than a label set. §8.2 says a closed
//! vocabulary is what keeps AI tags governable; that is only true if something closes it.
//!
//! The rest is the ordinary administration a vocabulary needs before any of the 2.2 lifecycle operations have
//! anything to operate on: creating one, adding terms, and editing the two settings that change what a model
//! does — the synonyms it matches against and the threshold above which a tag is applied rather than suggested.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use dam_db::{enrichment, migrate, taxonomy, testing::PostgresHarness};
use sqlx::PgPool;

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn held(pool: &PgPool) -> sqlx::pool::PoolConnection<sqlx::Postgres> {
    pool.acquire().await.expect("acquire")
}

/// The slugs a model would be offered, in the order the prompt lists them.
async fn offered(pool: &PgPool) -> Vec<String> {
    enrichment::vocabulary(&mut *held(pool).await, 500)
        .await
        .expect("vocabulary")
        .0
        .into_iter()
        .map(|(slug, _, _)| slug)
        .collect()
}

// ─── the gate ───────────────────────────────────────────────────────────────

async fn a_new_vocabulary_is_closed_to_machine_tagging(pool: &PgPool) {
    let moods = taxonomy::create_vocabulary(&mut *held(pool).await, "moods", "Moods")
        .await
        .expect("create");
    taxonomy::add_term(
        &mut *held(pool).await,
        moods,
        &taxonomy::NewTerm {
            slug: "calm",
            label: "Calm",
            synonyms: &[],
            parent_id: None,
        },
    )
    .await
    .expect("term");

    // The governed default, and the reason the column exists. A vocabulary somebody created five seconds ago
    // has not been reviewed for machine use, so it is in no prompt until they say so.
    let listed = taxonomy::vocabularies(&mut *held(pool).await)
        .await
        .expect("list");
    let made = listed.iter().find(|one| one.id == moods).expect("listed");
    assert!(!made.ai_taggable);
    assert_eq!(made.term_count, 1);
    assert!(
        offered(pool).await.is_empty(),
        "a closed vocabulary is in no prompt"
    );

    assert!(
        taxonomy::set_ai_taggable(&mut *held(pool).await, moods, true)
            .await
            .expect("open it")
    );
    assert_eq!(offered(pool).await, vec!["calm".to_owned()]);

    // And closing it again removes it, so the flag is a live gate rather than a one-way door.
    taxonomy::set_ai_taggable(&mut *held(pool).await, moods, false)
        .await
        .expect("close it");
    assert!(offered(pool).await.is_empty());
}

async fn a_category_tree_is_never_offered_to_a_model(pool: &PgPool) {
    // The half of the defect nobody would have noticed from the outside. A category tree is where assets
    // *live*; inviting an LLM to file them into somebody's browse hierarchy is a much larger claim than
    // inviting it to suggest a tag, and the old query made it without anybody choosing to.
    let tree = dam_db::categories::create_tree(&mut *held(pool).await, "subject", "Subject")
        .await
        .expect("tree");
    dam_db::categories::create(
        &mut *held(pool).await,
        dam_db::categories::NewCategory {
            taxonomy_id: tree,
            parent_id: None,
            slug: "harbour".to_owned(),
            label: "Harbour".to_owned(),
        },
    )
    .await
    .expect("node");

    assert!(
        offered(pool).await.is_empty(),
        "a filing tree is not a tag vocabulary, whatever `ai_taggable` says about it"
    );
    // Even asked directly: `set_ai_taggable` only touches vocabularies, so a tree cannot be opened by id.
    assert!(
        !taxonomy::set_ai_taggable(&mut *held(pool).await, tree, true)
            .await
            .expect("attempt"),
        "a category tree is not a vocabulary and reports so rather than silently becoming one"
    );
    assert!(offered(pool).await.is_empty());

    // And with the flag set *by hand*, which is the case that actually happened: 0034's backfill turned it on
    // for every existing taxonomy to preserve behaviour, and on a tenant whose only taxonomy is a tree that
    // opened a browse hierarchy to the model. The API cannot do it; SQL can, and a migration did. So the
    // query requires the kind as well as the flag, and this is the assertion that holds it to that.
    sqlx::query("UPDATE taxonomies SET ai_taggable = true WHERE id = $1")
        .bind(tree)
        .execute(pool)
        .await
        .expect("open it by hand");
    assert!(
        offered(pool).await.is_empty(),
        "a filing tree is never a label set, whatever the flag on it says"
    );
}

async fn a_retired_term_leaves_the_prompt_but_stays_resolvable(pool: &PgPool) {
    let colours = taxonomy::create_vocabulary(&mut *held(pool).await, "colours", "Colours")
        .await
        .expect("create");
    taxonomy::set_ai_taggable(&mut *held(pool).await, colours, true)
        .await
        .expect("open");
    let mut ids = Vec::new();
    for slug in ["amber", "cerise"] {
        ids.push(
            taxonomy::add_term(
                &mut *held(pool).await,
                colours,
                &taxonomy::NewTerm {
                    slug,
                    label: slug,
                    synonyms: &[],
                    parent_id: None,
                },
            )
            .await
            .expect("term"),
        );
    }
    assert_eq!(
        offered(pool).await,
        vec!["amber".to_owned(), "cerise".to_owned()]
    );

    taxonomy::deprecate(&mut *held(pool).await, ids[1])
        .await
        .expect("retire");
    assert_eq!(
        offered(pool).await,
        vec!["amber".to_owned()],
        "a retired term is offered to nobody"
    );
    // Still resolvable, which is the whole reason retirement exists instead of deletion: a saved search or a
    // Drupal field holding this id keeps working.
    let resolved = taxonomy::resolve(&mut *held(pool).await, ids[1])
        .await
        .expect("resolve")
        .expect("still there");
    assert_eq!(resolved.effective_id, ids[1]);
    assert!(resolved.deprecated_at.is_some());
}

async fn the_prompt_count_describes_the_prompt(pool: &PgPool) {
    // The count is what tells a caller the prompt was truncated, so it must be over the same set as the rows.
    // Counting every term in the tenant would report a truncation that did not happen, constantly, on a tenant
    // with one open vocabulary and several closed ones — which is what this fixture is by now.
    let (rows, total) = enrichment::vocabulary(&mut *held(pool).await, 500)
        .await
        .expect("vocabulary");
    let open = i64::try_from(rows.len()).expect("small");
    assert_eq!(
        total, open,
        "the count is of the offerable set, not of every term in the tenant"
    );
    assert!(open >= 1, "at least one vocabulary is open by now");

    // Truncated: the rows shrink and the total does not, which is how a caller learns it was cut.
    let (prefix, still) = enrichment::vocabulary(&mut *held(pool).await, 1)
        .await
        .expect("vocabulary");
    assert_eq!(prefix.len(), 1, "the limit bounds the prefix");
    assert_eq!(
        still, open,
        "the total is the reason a caller can tell it was truncated"
    );
}

// ─── the administration ─────────────────────────────────────────────────────

async fn synonyms_and_the_threshold_are_what_a_model_reads(pool: &PgPool) {
    let vocab = taxonomy::create_vocabulary(&mut *held(pool).await, "weather", "Weather")
        .await
        .expect("create");
    taxonomy::set_ai_taggable(&mut *held(pool).await, vocab, true)
        .await
        .expect("open");
    let term = taxonomy::add_term(
        &mut *held(pool).await,
        vocab,
        &taxonomy::NewTerm {
            slug: "overcast",
            label: "Overcast",
            synonyms: &["cloudy".to_owned()],
            parent_id: None,
        },
    )
    .await
    .expect("term");

    // Found by slug, not indexed: the prompt is ordered by slug across *every* open vocabulary, so an earlier
    // case's term can sit in front of this one. That ordering is deliberate — prompt caching matches on bytes.
    let (rows, _) = enrichment::vocabulary(&mut *held(pool).await, 500)
        .await
        .expect("vocabulary");
    let offered = rows
        .iter()
        .find(|(slug, _, _)| slug == "overcast")
        .expect("the term is in the prompt");
    assert_eq!(
        offered.2,
        vec!["cloudy".to_owned()],
        "synonyms reach the prompt"
    );

    assert!(
        taxonomy::amend_term(
            &mut *held(pool).await,
            term,
            "Overcast sky",
            &["cloudy".to_owned(), "grey".to_owned()],
            0.8,
        )
        .await
        .expect("amend")
    );
    let listed = taxonomy::terms(&mut *held(pool).await, vocab)
        .await
        .expect("terms");
    assert_eq!(listed[0].label, "Overcast sky");
    assert_eq!(
        listed[0].synonyms,
        vec!["cloudy".to_owned(), "grey".to_owned()]
    );
    assert!((listed[0].ai_threshold - 0.8).abs() < f32::EPSILON);
    // The slug is untouched, which is the point of it not being a parameter: it is what a model answers with
    // and what an import resolves, so moving it would orphan both.
    assert_eq!(listed[0].slug, "overcast");
}

async fn a_threshold_outside_the_range_is_clamped_not_refused(pool: &PgPool) {
    let vocab = taxonomy::create_vocabulary(&mut *held(pool).await, "clamped", "Clamped")
        .await
        .expect("create");
    let term = taxonomy::add_term(
        &mut *held(pool).await,
        vocab,
        &taxonomy::NewTerm {
            slug: "impossible",
            label: "Impossible",
            synonyms: &[],
            parent_id: None,
        },
    )
    .await
    .expect("term");

    // 1.5 and -1 each express a real intention through a typo — "never auto-apply" and "always" — so they are
    // clamped, and the read-back is how the screen shows the operator what they actually got.
    for (sent, stored) in [(1.5_f32, 1.0_f32), (-1.0, 0.0)] {
        taxonomy::amend_term(&mut *held(pool).await, term, "Impossible", &[], sent)
            .await
            .expect("amend");
        let listed = taxonomy::terms(&mut *held(pool).await, vocab)
            .await
            .expect("terms");
        assert!(
            (listed[0].ai_threshold - stored).abs() < f32::EPSILON,
            "{sent} should store as {stored}, got {}",
            listed[0].ai_threshold
        );
    }
}

async fn a_duplicate_slug_is_refused_by_name(pool: &PgPool) {
    let vocab = taxonomy::create_vocabulary(&mut *held(pool).await, "unique", "Unique")
        .await
        .expect("create");
    let new = taxonomy::NewTerm {
        slug: "once",
        label: "Once",
        synonyms: &[],
        parent_id: None,
    };
    taxonomy::add_term(&mut *held(pool).await, vocab, &new)
        .await
        .expect("first");
    // Refused in Rust rather than by the unique index, so the message names the path instead of a constraint.
    // The slug matters as much as the path: it is what a model answers with, so two terms sharing one would
    // make a suggestion ambiguous.
    match taxonomy::add_term(&mut *held(pool).await, vocab, &new).await {
        Err(taxonomy::Error::PathTaken { path }) => assert_eq!(path, "once"),
        other => panic!("a duplicate slug should be refused by name, got {other:?}"),
    }
}

async fn a_nested_term_gets_its_path_from_its_parent(pool: &PgPool) {
    let vocab = taxonomy::create_vocabulary(&mut *held(pool).await, "places", "Places")
        .await
        .expect("create");
    let outdoor = taxonomy::add_term(
        &mut *held(pool).await,
        vocab,
        &taxonomy::NewTerm {
            slug: "outdoor",
            label: "Outdoor",
            synonyms: &[],
            parent_id: None,
        },
    )
    .await
    .expect("parent");
    taxonomy::add_term(
        &mut *held(pool).await,
        vocab,
        &taxonomy::NewTerm {
            slug: "quay",
            label: "Quay",
            synonyms: &[],
            parent_id: Some(outdoor),
        },
    )
    .await
    .expect("child");

    let listed = taxonomy::terms(&mut *held(pool).await, vocab)
        .await
        .expect("terms");
    let paths: Vec<&str> = listed.iter().map(|one| one.path.as_str()).collect();
    // Computed, never supplied: a path that does not match the parent chain makes every ancestor query wrong
    // and quietly, which is the same failure `move_term` rewrites whole subtrees to avoid.
    assert_eq!(paths, vec!["outdoor", "outdoor.quay"]);

    // A retired parent would leave a live term under a branch no picker offers.
    taxonomy::deprecate(&mut *held(pool).await, outdoor)
        .await
        .expect_err("a parent with a live child cannot retire");
}

async fn a_term_cannot_be_parented_across_vocabularies(pool: &PgPool) {
    let left = taxonomy::create_vocabulary(&mut *held(pool).await, "left", "Left")
        .await
        .expect("create");
    let right = taxonomy::create_vocabulary(&mut *held(pool).await, "right", "Right")
        .await
        .expect("create");
    let anchor = taxonomy::add_term(
        &mut *held(pool).await,
        left,
        &taxonomy::NewTerm {
            slug: "anchor",
            label: "Anchor",
            synonyms: &[],
            parent_id: None,
        },
    )
    .await
    .expect("term");

    let refused = taxonomy::add_term(
        &mut *held(pool).await,
        right,
        &taxonomy::NewTerm {
            slug: "stray",
            label: "Stray",
            synonyms: &[],
            parent_id: Some(anchor),
        },
    )
    .await;
    assert!(matches!(
        refused,
        Err(taxonomy::Error::DifferentTaxonomies { .. })
    ));
}

#[tokio::test]
async fn a_vocabulary_is_governable() {
    let (_pg, pool) = db().await;

    a_new_vocabulary_is_closed_to_machine_tagging(&pool).await;
    a_category_tree_is_never_offered_to_a_model(&pool).await;
    a_retired_term_leaves_the_prompt_but_stays_resolvable(&pool).await;
    the_prompt_count_describes_the_prompt(&pool).await;

    synonyms_and_the_threshold_are_what_a_model_reads(&pool).await;
    a_threshold_outside_the_range_is_clamped_not_refused(&pool).await;
    a_duplicate_slug_is_refused_by_name(&pool).await;
    a_nested_term_gets_its_path_from_its_parent(&pool).await;
    a_term_cannot_be_parented_across_vocabularies(&pool).await;
}
