//! Saved searches (3.7, G15).
//!
//! The named property decides whether a saved search is safe to share at all: it is **re-evaluated against
//! current access, not the access at save time.** Store the results, or store the query with its access filter
//! baked in, and a search saved by an administrator becomes a permanent leak wearing the shape of a bookmark.
//!
//! One container; the cases are functions over a borrowed pool.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::fields::{Constraints, FieldDef, FieldKind};
use dam_core::policy::{self, Action, Grant, Grants};
use dam_core::query::{Comparison, Endpoint, Literal, Query};
use dam_db::saved_searches::{self, SaveSpec};
use dam_db::{migrate, query_sql, testing::PostgresHarness};
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

fn def(key: &str, kind: FieldKind) -> FieldDef {
    FieldDef {
        key: key.to_owned(),
        kind,
        taxonomy_id: None,
        multivalued: false,
        required: false,
        read_only: false,
        ai_writable: false,
        facetable: true,
        constraints: Constraints::default(),
    }
}

fn defs() -> Vec<FieldDef> {
    vec![
        def("brand", FieldKind::Text),
        def("year", FieldKind::Int),
        def("live", FieldKind::Bool),
        def("shot_on", FieldKind::Date),
    ]
}

fn access(groups: Option<&[Uuid]>) -> policy::AccessPredicate {
    let (ids, all) = match groups {
        Some(ids) => (ids.to_vec(), false),
        None => (vec![], true),
    };
    policy::compile(
        &Grants::from(vec![Grant {
            permissions: vec!["asset:read".to_owned()],
            asset_group_ids: ids,
            all_asset_groups: all,
            valid_from: None,
            valid_until: None,
            requires_eula: false,
            eula_accepted: true,
        }]),
        Action::Read,
        now(),
    )
}

async fn db() -> (PostgresHarness, PgPool) {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    let pool = pg.pool_for_schema("t_acme").await.expect("pool");
    (pg, pool)
}

async fn group(pool: &PgPool, key: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO asset_groups (id, key, label) VALUES ($1, $2, $2)")
        .bind(id)
        .bind(key)
        .execute(pool)
        .await
        .expect("group");
    id
}

async fn asset(pool: &PgPool, label: &str, brand: &str, groups: &[Uuid]) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO assets (id, content_hash, filename, mime, bytes, version_group_id) \
         VALUES ($1, $2, $3, 'image/jpeg', 10, $1)",
    )
    .bind(id)
    .bind(blake3::hash(label.as_bytes()).to_hex().to_string())
    .bind(format!("{label}.jpg"))
    .execute(pool)
    .await
    .expect("asset");
    sqlx::query("INSERT INTO asset_metadata (asset_id, values) VALUES ($1, $2)")
        .bind(id)
        .bind(serde_json::json!({"brand": brand}))
        .execute(pool)
        .await
        .expect("metadata");
    for g in groups {
        sqlx::query("INSERT INTO asset_group_members (group_id, asset_id) VALUES ($1, $2)")
            .bind(g)
            .bind(id)
            .execute(pool)
            .await
            .expect("membership");
    }
    id
}

fn spec<'a>(name: &'a str, query: &'a Query, owner: Option<Uuid>) -> SaveSpec<'a> {
    SaveSpec {
        owner_id: owner,
        name,
        query,
        is_smart_collection: false,
        shared: false,
        shared_with_roles: &[],
        notify_path_id: None,
    }
}

/// Runs a planned saved search and returns the ids it matches.
async fn run(pool: &PgPool, planned: &dam_core::query::Planned) -> Vec<Uuid> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT assets.id FROM assets \
         LEFT JOIN asset_metadata ON asset_metadata.asset_id = assets.id WHERE ",
    );
    query_sql::push_where(&mut builder, planned).expect("render");
    let mut ids: Vec<Uuid> = builder
        .build_query_scalar()
        .fetch_all(pool)
        .await
        .expect("query");
    ids.sort_unstable();
    ids
}

// ─── the named property ─────────────────────────────────────────────────────

async fn a_search_saved_by_an_administrator_runs_as_the_viewer(pool: &PgPool) {
    // The property that makes sharing safe. Store the results, or bake the saver's access filter into the
    // stored query, and this search becomes a permanent leak wearing the shape of a bookmark.
    let visible = group(pool, "vis-saved").await;
    let hidden = group(pool, "hid-saved").await;
    let seen = asset(pool, "seen-saved", "Acme", &[visible]).await;
    let unseen = asset(pool, "unseen-saved", "Acme", &[hidden]).await;

    let query = Query::Field {
        key: "brand".to_owned(),
        op: Comparison::Equals(Literal::Text("Acme".to_owned())),
    };
    // Saved by an administrator, who can see both.
    let saved = saved_searches::save(pool, &spec("all Acme", &query, None))
        .await
        .expect("save");

    let as_admin = saved_searches::plan(&saved, access(None), &defs()).expect("plan");
    let admin_results = run(pool, &as_admin).await;
    assert!(admin_results.contains(&seen) && admin_results.contains(&unseen));

    // Opened by somebody scoped to one group.
    let as_contractor =
        saved_searches::plan(&saved, access(Some(&[visible])), &defs()).expect("plan");
    let contractor_results = run(pool, &as_contractor).await;
    assert!(contractor_results.contains(&seen));
    assert!(
        !contractor_results.contains(&unseen),
        "the saved search must run against the viewer's access, not the saver's"
    );
}

async fn the_stored_query_contains_no_access_filter_at_all(pool: &PgPool) {
    // Asserted on the stored bytes rather than inferred from behaviour. If a group id ever appeared in here, the
    // saved row would carry one person's scope and the property above would be one refactor from breaking.
    let scoped_group = group(pool, "leak-check").await;
    let query = Query::Field {
        key: "brand".to_owned(),
        op: Comparison::Equals(Literal::Text("Acme".to_owned())),
    };
    let saved = saved_searches::save(pool, &spec("no filter", &query, None))
        .await
        .expect("save");

    let stored = serde_json::to_string(&saved.query).expect("json");
    assert!(
        !stored.contains(&scoped_group.to_string()),
        "the stored query must not mention any group: {stored}"
    );
    assert!(
        !stored.contains("asset_group_members") && !stored.contains("deleted_at"),
        "and it must be the user's query, not rendered SQL: {stored}"
    );
}

// ─── the round trip ─────────────────────────────────────────────────────────

async fn every_query_shape_survives_being_saved_and_loaded(pool: &PgPool) {
    // A saved search that fails to load is a broken bookmark whose owner cannot fix it. The stored form is a
    // wire format, so every shape has to round-trip exactly.
    let shapes: Vec<(&str, Query)> = vec![
        ("all", Query::All),
        ("text", Query::Text("beach holiday".to_owned())),
        (
            "equals text",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Equals(Literal::Text("Acme".to_owned())),
            },
        ),
        (
            "not equals int",
            Query::Field {
                key: "year".to_owned(),
                op: Comparison::NotEquals(Literal::Int(2026)),
            },
        ),
        (
            "bool",
            Query::Field {
                key: "live".to_owned(),
                op: Comparison::Equals(Literal::Bool(true)),
            },
        ),
        (
            "exists",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Exists,
            },
        ),
        (
            "missing",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Missing,
            },
        ),
        (
            "contains",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::Contains("cme".to_owned()),
            },
        ),
        (
            "starts with",
            Query::Field {
                key: "brand".to_owned(),
                op: Comparison::StartsWith("Ac".to_owned()),
            },
        ),
        (
            "date range",
            Query::Field {
                key: "shot_on".to_owned(),
                op: Comparison::Range {
                    lower: Endpoint::Inclusive(Literal::Date(
                        chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("date"),
                    )),
                    upper: Endpoint::Exclusive(Literal::Date(
                        chrono::NaiveDate::from_ymd_opt(2026, 12, 31).expect("date"),
                    )),
                },
            },
        ),
        (
            "half-open int range",
            Query::Field {
                key: "year".to_owned(),
                op: Comparison::Range {
                    lower: Endpoint::Exclusive(Literal::Int(2000)),
                    upper: Endpoint::Unbounded,
                },
            },
        ),
        (
            "term with descendants",
            Query::Term {
                term_id: Uuid::from_u128(42),
                include_descendants: true,
            },
        ),
        (
            "term without descendants",
            Query::Term {
                term_id: Uuid::from_u128(42),
                include_descendants: false,
            },
        ),
        ("collection", Query::InCollection(Uuid::from_u128(7))),
        (
            "rating range",
            Query::Rating(Comparison::Range {
                lower: Endpoint::Inclusive(Literal::Int(4)),
                upper: Endpoint::Unbounded,
            }),
        ),
        ("unrated", Query::Rating(Comparison::Missing)),
        // Q.15's clauses. A saved search written from the filter rail carries them, and the stored form is a
        // wire format — a row written today has to load after the enum gains its next variant.
        ("status", Query::Status("archived".to_owned())),
        (
            "orientation",
            Query::Orientation(dam_core::query::Orientation::Portrait),
        ),
        ("has attachment", Query::HasAttachment),
        (
            "mine favourite",
            Query::Mine(dam_core::query::Personal::Favourite),
        ),
        (
            "mine watched",
            Query::Mine(dam_core::query::Personal::Watched),
        ),
        ("mine rated", Query::Mine(dam_core::query::Personal::Rated)),
        (
            "nested boolean",
            Query::And(vec![
                Query::Or(vec![
                    Query::Text("a".to_owned()),
                    Query::Field {
                        key: "year".to_owned(),
                        op: Comparison::Exists,
                    },
                ]),
                Query::Not(Box::new(Query::Field {
                    key: "live".to_owned(),
                    op: Comparison::Equals(Literal::Bool(false)),
                })),
            ]),
        ),
        ("empty and", Query::And(vec![])),
        ("empty or", Query::Or(vec![])),
    ];

    for (name, query) in shapes {
        let saved = saved_searches::save(pool, &spec(name, &query, None))
            .await
            .unwrap_or_else(|e| panic!("saving {name}: {e}"));
        let planned = saved_searches::plan(&saved, access(None), &defs())
            .unwrap_or_else(|e| panic!("loading {name}: {e}"));
        assert_eq!(
            planned.query(),
            &query,
            "{name} did not survive the round trip"
        );
    }
}

async fn a_literals_type_is_tagged_not_guessed(pool: &PgPool) {
    // `2026` could be an int, a decimal, or a year in a text field. Guessing from the JSON shape on load would
    // compare the wrong column type — which for a range means silently wrong results rather than an error.
    let as_int = Query::Field {
        key: "year".to_owned(),
        op: Comparison::Equals(Literal::Int(2026)),
    };
    let as_text = Query::Field {
        key: "brand".to_owned(),
        op: Comparison::Equals(Literal::Text("2026".to_owned())),
    };

    for (name, query) in [("int", as_int), ("text", as_text)] {
        let saved = saved_searches::save(pool, &spec(name, &query, None))
            .await
            .expect("save");
        let planned = saved_searches::plan(&saved, access(None), &defs()).expect("plan");
        assert_eq!(planned.query(), &query, "{name} lost its type");
    }
}

async fn a_search_referring_to_a_deleted_field_is_refused_not_silently_dropped(pool: &PgPool) {
    // Dropping the clause would **widen** the result set, which for a filter over a governed library is the
    // wrong direction — the same argument `dam_core::query` makes about unknown fields.
    let query = Query::Field {
        key: "brand".to_owned(),
        op: Comparison::Equals(Literal::Text("Acme".to_owned())),
    };
    let saved = saved_searches::save(pool, &spec("orphaned", &query, None))
        .await
        .expect("save");

    // The field has since been removed from the tenant's schema.
    let without_brand: Vec<FieldDef> = defs().into_iter().filter(|d| d.key != "brand").collect();
    let refused =
        saved_searches::plan(&saved, access(None), &without_brand).expect_err("must refuse");
    let message = refused.to_string();
    assert!(
        message.contains("brand") && message.contains("unknown_field"),
        "the error must name the clause its owner has to fix: {message}"
    );
}

async fn an_unreadable_stored_query_is_refused_rather_than_matching_everything(pool: &PgPool) {
    // The dangerous default. Treating an unknown stored shape as `Query::All` turns a corrupt bookmark into
    // "every asset", which is the widest possible answer to a query nobody can read.
    let query = Query::All;
    let saved = saved_searches::save(pool, &spec("corrupted", &query, None))
        .await
        .expect("save");
    sqlx::query(
        "UPDATE saved_searches SET query = '{\"kind\":\"from-the-future\"}'::jsonb WHERE id = $1",
    )
    .bind(saved.id)
    .execute(pool)
    .await
    .expect("corrupt");

    let reloaded = saved_searches::load(pool, saved.id)
        .await
        .expect("load")
        .expect("present");
    assert!(
        saved_searches::plan(&reloaded, access(None), &defs()).is_err(),
        "an unreadable query must refuse, not match everything"
    );

    // The same rule one level down, for a shape this build half-recognises: an orientation it has never heard
    // of is refused rather than defaulted to landscape, which would quietly show somebody a different library
    // than their bookmark named.
    sqlx::query(
        "UPDATE saved_searches SET query = '{\"kind\":\"orientation\",\"shape\":\"panoramic\"}'::jsonb \
          WHERE id = $1",
    )
    .bind(saved.id)
    .execute(pool)
    .await
    .expect("corrupt");
    let future = saved_searches::load(pool, saved.id)
        .await
        .expect("load")
        .expect("present");
    assert!(
        saved_searches::plan(&future, access(None), &defs()).is_err(),
        "an unknown orientation must refuse rather than pick one"
    );
}

// ─── sharing ────────────────────────────────────────────────────────────────

async fn a_private_search_is_visible_only_to_its_owner(pool: &PgPool) {
    let owner = Uuid::new_v4();
    let stranger = Uuid::new_v4();
    let query = Query::All;
    let saved = saved_searches::save(pool, &spec("private", &query, Some(owner)))
        .await
        .expect("save");

    let mine = saved_searches::visible_to(pool, Some(owner), &[], 100)
        .await
        .expect("visible");
    assert!(mine.iter().any(|s| s.id == saved.id));

    let theirs = saved_searches::visible_to(pool, Some(stranger), &[], 100)
        .await
        .expect("visible");
    assert!(
        theirs.iter().all(|s| s.id != saved.id),
        "a private search must not appear to anybody else"
    );
}

async fn a_caller_with_no_identity_owns_nothing(pool: &PgPool) {
    // `owner_id` is nullable with no defined meaning for NULL, and the ownership test used
    // `IS NOT DISTINCT FROM` — so a caller with no identity matched every ownerless search while an identified
    // caller matched none of them. Neither half is a coherent rule, and it was nobody's decision. A mutation
    // swapping the operator for plain `=` changed no test, which is how it surfaced.
    //
    // The rule now: an identity-less caller owns nothing, and an ownerless search is reachable only by being
    // shared. Narrowing, which is the safe direction for an access predicate.
    let query = Query::All;
    let ownerless_private = saved_searches::save(pool, &spec("ownerless private", &query, None))
        .await
        .expect("save");
    let ownerless_shared = saved_searches::save(
        pool,
        &SaveSpec {
            shared: true,
            ..spec("ownerless shared", &query, None)
        },
    )
    .await
    .expect("save");

    let anonymous = saved_searches::visible_to(pool, None, &[], 100)
        .await
        .expect("visible");
    assert!(
        anonymous.iter().all(|s| s.id != ownerless_private.id),
        "a caller with no identity must not own an ownerless search"
    );
    assert!(
        anonymous.iter().any(|s| s.id == ownerless_shared.id),
        "but a shared one is reachable, because that is what shared means"
    );

    // And an identified caller sees the shared one for the same reason, rather than being excluded from
    // ownerless searches as the old operator did.
    let somebody = saved_searches::visible_to(pool, Some(Uuid::new_v4()), &[], 100)
        .await
        .expect("visible");
    assert!(somebody.iter().any(|s| s.id == ownerless_shared.id));
    assert!(somebody.iter().all(|s| s.id != ownerless_private.id));
}

async fn a_shared_search_with_no_roles_reaches_the_whole_tenant(pool: &PgPool) {
    // What `shared` alone means. `shared_with_roles` narrows it, and an empty list is the unnarrowed case rather
    // than "shared with nobody" — which would make the flag do nothing.
    let owner = Uuid::new_v4();
    let query = Query::All;
    let saved = saved_searches::save(
        pool,
        &SaveSpec {
            shared: true,
            ..spec("shared with all", &query, Some(owner))
        },
    )
    .await
    .expect("save");

    let stranger = saved_searches::visible_to(pool, Some(Uuid::new_v4()), &[], 100)
        .await
        .expect("visible");
    assert!(stranger.iter().any(|s| s.id == saved.id));
}

async fn a_role_scoped_share_reaches_only_those_roles(pool: &PgPool) {
    let owner = Uuid::new_v4();
    let query = Query::All;
    let roles = vec!["editor".to_owned()];
    let saved = saved_searches::save(
        pool,
        &SaveSpec {
            shared: true,
            shared_with_roles: &roles,
            ..spec("editors only", &query, Some(owner))
        },
    )
    .await
    .expect("save");

    let editor =
        saved_searches::visible_to(pool, Some(Uuid::new_v4()), &["editor".to_owned()], 100)
            .await
            .expect("visible");
    assert!(editor.iter().any(|s| s.id == saved.id));

    let viewer =
        saved_searches::visible_to(pool, Some(Uuid::new_v4()), &["viewer".to_owned()], 100)
            .await
            .expect("visible");
    assert!(
        viewer.iter().all(|s| s.id != saved.id),
        "a role-scoped share must not reach other roles"
    );
}

async fn visibility_does_not_imply_the_savers_results(pool: &PgPool) {
    // The two questions the module keeps separate: *seeing* a shared search in a list, and *what it returns*.
    // Sharing a search shares the question, never the answer.
    let visible = group(pool, "vis-share").await;
    let hidden = group(pool, "hid-share").await;
    asset(pool, "vis-share-a", "Globex", &[visible]).await;
    let unseen = asset(pool, "hid-share-a", "Globex", &[hidden]).await;

    let query = Query::Field {
        key: "brand".to_owned(),
        op: Comparison::Equals(Literal::Text("Globex".to_owned())),
    };
    let saved = saved_searches::save(
        pool,
        &SaveSpec {
            shared: true,
            ..spec("shared globex", &query, None)
        },
    )
    .await
    .expect("save");

    let listed = saved_searches::visible_to(pool, Some(Uuid::new_v4()), &[], 100)
        .await
        .expect("visible");
    let found = listed
        .iter()
        .find(|s| s.id == saved.id)
        .expect("the share is visible");

    let planned = saved_searches::plan(found, access(Some(&[visible])), &defs()).expect("plan");
    assert!(
        !run(pool, &planned).await.contains(&unseen),
        "seeing a shared search must not mean seeing the saver's results"
    );
}

// ─── bookkeeping ────────────────────────────────────────────────────────────

async fn the_cached_count_is_not_presented_as_the_viewers_count(pool: &PgPool) {
    // The schema calls `result_count` "never trusted for access decisions". It is stored per search rather than
    // per viewer, so it is at best somebody else's number — and showing it as *the* count leaks how many assets
    // exist beyond a viewer's scope, which is §7's disclosure in a sidebar.
    let query = Query::All;
    let saved = saved_searches::save(pool, &spec("counted", &query, None))
        .await
        .expect("save");
    assert!(saved.result_count.is_none(), "nothing counted yet");

    saved_searches::record_count(pool, saved.id, 999, now())
        .await
        .expect("count");
    let reloaded = saved_searches::load(pool, saved.id)
        .await
        .expect("load")
        .expect("present");
    assert_eq!(reloaded.result_count, Some(999));
    assert_eq!(reloaded.counted_at, Some(now()));

    // And the count is unrelated to what a scoped viewer actually gets, which is the point.
    let planned =
        saved_searches::plan(&reloaded, access(Some(&[Uuid::new_v4()])), &defs()).expect("plan");
    let actual = run(pool, &planned).await;
    assert_ne!(
        actual.len() as i64,
        999,
        "the cached badge is not the viewer's result count"
    );
}

async fn last_used_is_written_at_most_hourly(pool: &PgPool) {
    // The column orders a "recently used" sidebar. Writing it on every open turns browsing into a write per
    // click, and an hour's resolution sorts that list identically.
    let query = Query::All;
    let saved = saved_searches::save(pool, &spec("used", &query, None))
        .await
        .expect("save");

    assert!(
        saved_searches::mark_used(pool, saved.id, now())
            .await
            .expect("mark")
    );
    assert!(
        !saved_searches::mark_used(pool, saved.id, now() + Duration::minutes(30))
            .await
            .expect("mark"),
        "a second open inside the window must not write"
    );
    assert!(
        saved_searches::mark_used(
            pool,
            saved.id,
            now() + saved_searches::USED_RESOLUTION + Duration::seconds(1)
        )
        .await
        .expect("mark")
    );
}

async fn a_smart_collection_appears_in_the_collections_list(pool: &PgPool) {
    let query = Query::Field {
        key: "live".to_owned(),
        op: Comparison::Equals(Literal::Bool(true)),
    };
    let smart = saved_searches::save(
        pool,
        &SaveSpec {
            is_smart_collection: true,
            ..spec("live assets", &query, None)
        },
    )
    .await
    .expect("save");
    let ordinary = saved_searches::save(pool, &spec("just a search", &query, None))
        .await
        .expect("save");

    let listed = saved_searches::smart_collections(pool, 100)
        .await
        .expect("list");
    assert!(listed.iter().any(|s| s.id == smart.id));
    assert!(
        listed.iter().all(|s| s.id != ordinary.id),
        "an ordinary saved search is not a collection"
    );
}

async fn deleting_removes_it(pool: &PgPool) {
    let query = Query::All;
    let saved = saved_searches::save(pool, &spec("doomed", &query, None))
        .await
        .expect("save");
    assert!(
        saved_searches::delete(pool, saved.id)
            .await
            .expect("delete")
    );
    assert!(
        !saved_searches::delete(pool, saved.id)
            .await
            .expect("delete"),
        "deleting twice reports that it did nothing"
    );
    assert!(
        saved_searches::load(pool, saved.id)
            .await
            .expect("load")
            .is_none()
    );
}

#[tokio::test]
async fn the_saved_search_invariants_hold() {
    let (_pg, pool) = db().await;

    a_search_saved_by_an_administrator_runs_as_the_viewer(&pool).await;
    the_stored_query_contains_no_access_filter_at_all(&pool).await;

    every_query_shape_survives_being_saved_and_loaded(&pool).await;
    a_literals_type_is_tagged_not_guessed(&pool).await;
    a_saved_personal_search_stores_no_identity(&pool).await;
    a_search_referring_to_a_deleted_field_is_refused_not_silently_dropped(&pool).await;
    an_unreadable_stored_query_is_refused_rather_than_matching_everything(&pool).await;

    a_private_search_is_visible_only_to_its_owner(&pool).await;
    a_caller_with_no_identity_owns_nothing(&pool).await;
    a_shared_search_with_no_roles_reaches_the_whole_tenant(&pool).await;
    a_role_scoped_share_reaches_only_those_roles(&pool).await;
    visibility_does_not_imply_the_savers_results(&pool).await;

    the_cached_count_is_not_presented_as_the_viewers_count(&pool).await;
    last_used_is_written_at_most_hourly(&pool).await;
    a_smart_collection_appears_in_the_collections_list(&pool).await;
    deleting_removes_it(&pool).await;
}

async fn a_saved_personal_search_stores_no_identity(pool: &PgPool) {
    // The property that decides whether `is:favourite` is safe to save at all. The stored JSON must name the
    // *state* and nobody in particular, so that a colleague opening a shared search sees their own favourites
    // rather than the author's — the leak wearing the shape of a bookmark this module opens with.
    let query = Query::Mine(dam_core::query::Personal::Favourite);
    let saved = saved_searches::save(pool, &spec("my favourites", &query, None))
        .await
        .expect("save");

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT query FROM saved_searches WHERE id = $1")
            .bind(saved.id)
            .fetch_one(pool)
            .await
            .expect("stored query");
    assert_eq!(stored["kind"], serde_json::json!("mine"), "{stored}");
    assert_eq!(stored["state"], serde_json::json!("favourite"), "{stored}");

    // No uuid anywhere in the stored form. Asserted over the whole serialised text rather than a named key,
    // because the failure this guards against is an identity appearing under *some* key nobody thought of.
    let text = stored.to_string();
    assert!(
        !text.contains("identity") && !text.contains("viewer"),
        "the stored query names a person: {text}"
    );
    let uuid_shaped = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|word| word.len() == 36 && word.matches('-').count() == 4);
    assert!(!uuid_shaped, "the stored query contains a uuid: {text}");

    // And two different viewers planning the same saved search get plans that differ only in who is asking.
    let ada = Uuid::new_v4();
    let grace = Uuid::new_v4();
    let for_ada = saved_searches::plan(&saved, access(None), &defs())
        .expect("plan")
        .viewed_by(ada);
    let for_grace = saved_searches::plan(&saved, access(None), &defs())
        .expect("plan")
        .viewed_by(grace);
    assert_eq!(for_ada.query(), for_grace.query(), "the same question");
    assert_ne!(
        for_ada.viewer(),
        for_grace.viewer(),
        "asked by different people"
    );
}
