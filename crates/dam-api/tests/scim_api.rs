//! SCIM 2.0 over HTTP (G10·2b).
//!
//! `scim_clients` has existed since migration 0002 with nothing reading it, and 0002 says why it matters: the
//! deprovisioning half is what a security questionnaire asks about. So the cases are weighted towards the
//! offboarding paths and towards the envelope details that decide whether a provider can talk to us at all:
//!
//! - **Both deprovisioning paths work.** Okta sends `DELETE`, Entra sends `PATCH active: false`. Implementing
//!   one leaves the other silently unable to offboard anybody.
//! - **Entra sends the string `"False"`.** A strict parse rejects it, the sync fails, and the symptom is an
//!   employee who has left and still has access.
//! - **Deprovisioning reaches the credential.** Otherwise `active: false` is a flag, which is exactly what
//!   0002 warns about.
//! - **A provider may only touch what it provisioned.** Otherwise one tenant's misconfigured provider disables
//!   somebody an administrator added by hand.
//! - **An unsupported filter or op is refused by name.** Silently ignoring either makes a sync a no-op that
//!   looks healthy — a provider told its change applied does not send it again.
//! - **The envelope is exact.** `status` a string, `Resources` capitalised, `startIndex` 1-based, and
//!   `application/scim+json` on the way out.
//! - **Provisioning mints no credential**, deliberately, and the account it creates cannot yet sign in.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use dam_api::scim::{ScimState, router};
use dam_db::{auth, migrate, testing::PostgresHarness};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

struct Fixture {
    _pg: PostgresHarness,
    global: PgPool,
    app: axum::Router,
    governance: axum::Router,
    /// A tenant administrator's key, for the client-registration endpoints.
    admin_key: String,
    /// The provisioning token.
    token: String,
    /// A second tenant's token, for the cross-tenant cases.
    other_token: String,
    tenant_id: Uuid,
    /// Somebody an administrator added by hand, whom no provider owns.
    by_hand: Uuid,
}

async fn fixture() -> Fixture {
    let pg = PostgresHarness::start().await.expect("start postgres");
    let url = pg.url();
    migrate::global(&url).await.expect("global");
    migrate::tenant(&url, "t_acme").await.expect("tenant");
    migrate::tenant(&url, "t_globex").await.expect("other");
    let global = pg.pool().clone();
    let acme = pg.pool_for_schema("t_acme").await.expect("tenant pool");

    let tenant_id = tenant(&global, "acme", "t_acme").await;
    let other_id = tenant(&global, "globex", "t_globex").await;

    let ada = identity(&global, "ada@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{}', true)",
    )
    .bind(tenant_id)
    .bind(ada)
    .execute(&global)
    .await
    .expect("membership");

    // Somebody a person added, so a provider trying to manage them is refused.
    let by_hand = identity(&global, "manual@example.com").await;
    sqlx::query(
        "INSERT INTO dam_global.tenant_members (tenant_id, identity_id, role_names, is_tenant_admin) \
         VALUES ($1, $2, '{viewer}', false)",
    )
    .bind(tenant_id)
    .bind(by_hand)
    .execute(&global)
    .await
    .expect("manual membership");

    for key in ["viewer", "editor"] {
        sqlx::query(
            "INSERT INTO roles (id, key, label, permissions, all_asset_groups) \
             VALUES (gen_random_uuid(), $1, $1, '{asset:read}', true)",
        )
        .bind(key)
        .execute(&acme)
        .await
        .expect("role");
    }

    let (_, token) = dam_db::scim::issue(&global, tenant_id, "Okta", &["Users".to_owned()])
        .await
        .expect("issue");
    let (_, other_token) =
        dam_db::scim::issue(&global, other_id, "Their Okta", &["Users".to_owned()])
            .await
            .expect("issue other");

    Fixture {
        _pg: pg,
        app: router(ScimState {
            global: global.clone(),
        }),
        governance: dam_api::governance::router(dam_api::governance::GovernanceState {
            global: global.clone(),
        }),
        admin_key: issue_key(&global, tenant_id, ada).await,
        token,
        other_token,
        global,
        tenant_id,
        by_hand,
    }
}

async fn tenant(global: &PgPool, slug: &str, schema: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.tenants \
         (id, slug, schema_name, display_name, storage_prefix, status) \
         VALUES (gen_random_uuid(), $1, $2, $1, $3, 'active') RETURNING id",
    )
    .bind(slug)
    .bind(schema)
    .bind(format!("{slug}/"))
    .fetch_one(global)
    .await
    .expect("tenant")
}

async fn identity(global: &PgPool, email: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO dam_global.identities (id, email, display_name) \
         VALUES (gen_random_uuid(), $1, $1) RETURNING id",
    )
    .bind(email)
    .fetch_one(global)
    .await
    .expect("identity")
}

async fn issue_key(global: &PgPool, tenant_id: Uuid, who: Uuid) -> String {
    let api_key = auth::ApiKey::generate();
    sqlx::query(
        "INSERT INTO dam_global.api_keys \
         (id, tenant_id, identity_id, name, key_prefix, key_hash, scopes) \
         VALUES (gen_random_uuid(), $1, $2, 'test', $3, $4, '{}')",
    )
    .bind(tenant_id)
    .bind(who)
    .bind(api_key.prefix())
    .bind(api_key.hash())
    .execute(global)
    .await
    .expect("key");
    api_key.into_plaintext()
}

async fn call(
    app: &axum::Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if body.is_some() {
        // The content type providers actually send. axum's `Json` treats any `+json` suffix as JSON, which is
        // asserted here rather than assumed: a 415 on every request is the whole integration.
        request = request.header(header::CONTENT_TYPE, "application/scim+json");
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(match &body {
                    Some(value) => Body::from(value.to_string()),
                    None => Body::empty(),
                })
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        content_type,
    )
}

async fn works(f: &Fixture, key: &str) -> bool {
    auth::authenticate(&f.global, key)
        .await
        .expect("query")
        .is_some()
}

#[tokio::test]
async fn the_scim_surface() {
    let f = fixture().await;

    a_token_is_required_and_every_failure_looks_the_same(&f).await;
    the_service_provider_config_is_honest_about_what_is_missing(&f).await;
    provisioning_creates_access_and_no_credential(&f).await;
    the_same_person_twice_is_a_uniqueness_conflict(&f).await;
    a_person_added_by_hand_is_not_the_providers_to_manage(&f).await;
    another_tenants_token_reaches_nothing(&f).await;
    the_list_envelope_is_exactly_what_a_provider_parses(&f).await;
    only_the_filters_providers_send_are_accepted(&f).await;
    entra_deprovisions_by_patching_the_string_false(&f).await;
    reactivation_brings_the_account_back(&f).await;
    okta_deprovisions_by_deleting(&f).await;
    an_unsupported_patch_is_refused_rather_than_ignored(&f).await;
    a_provider_cannot_mint_another_provisioning_token(&f).await;
    contact_is_recorded_so_a_stalled_sync_is_visible(&f).await;
    a_role_this_tenant_does_not_define_is_named(&f).await;
    a_rename_is_refused_rather_than_dropped(&f).await;
}

async fn a_role_this_tenant_does_not_define_is_named(f: &Fixture) {
    // The same trap the human path documents: `role_names` has no foreign key and `auth` ignores what it
    // cannot resolve, so a provider mapping a group onto `editors` when the role is `editor` provisions
    // somebody who can see nothing. A provider that receives a named 400 can fix its mapping; one that
    // receives a 201 cannot.
    let (status, body, _) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({ "userName": "mapped@example.com", "roles": ["editors"] })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["scimType"], "invalidValue");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("editors"), "{detail}");
    // And what it *does* define, so the provider's operator can correct the mapping without asking.
    assert!(detail.contains("editor, viewer"), "{detail}");
}

async fn a_rename_is_refused_rather_than_dropped(f: &Fixture) {
    let (_, created, _) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({ "userName": "renameable@example.com", "externalId": "okta-0003" })),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // A PUT carrying a different userName. Silently keeping the old address would be a provider told its
    // rename applied, never sending it again, and two directories disagreeing from then on.
    let (status, body, _) = call(
        &f.app,
        "PUT",
        &format!("/scim/v2/Users/{id}"),
        Some(&f.token),
        Some(json!({ "userName": "renamed@example.com", "externalId": "okta-0003" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let detail = body["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("renameable@example.com"), "{detail}");
    assert!(
        detail.contains("Remove it and provision"),
        "the refusal has to say what to do instead: {detail}"
    );

    // The same address is not a rename, and goes through.
    let (status, _, _) = call(
        &f.app,
        "PUT",
        &format!("/scim/v2/Users/{id}"),
        Some(&f.token),
        Some(json!({ "userName": "renameable@example.com", "displayName": "Renamed Display" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

async fn a_token_is_required_and_every_failure_looks_the_same(f: &Fixture) {
    for token in [None, Some("damrs_scim_nonsense")] {
        let (status, body, content_type) = call(&f.app, "GET", "/scim/v2/Users", token, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        // The envelope, even on the refusal: a provider parses this before it parses anything else.
        assert_eq!(
            body["schemas"],
            json!(["urn:ietf:params:scim:api:messages:2.0:Error"])
        );
        assert_eq!(
            body["status"],
            json!("401"),
            "status is a string per RFC 7644 §3.12: {body}"
        );
        assert_eq!(content_type.as_deref(), Some("application/scim+json"));
    }

    // A revoked token stops working, and says nothing more than an unknown one.
    let (_, registered) = register(f, "To be revoked").await;
    let condemned = registered["token"].as_str().expect("token").to_owned();
    let id = registered["client"]["id"].as_str().expect("id").to_owned();
    let (status, _, _) = call(&f.app, "GET", "/scim/v2/Users", Some(&condemned), None).await;
    assert_eq!(status, StatusCode::OK, "the premise");
    let (status, _, _) = call(
        &f.app,
        "POST",
        &format!("/scim/clients/{id}/revoke"),
        Some(&f.admin_key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body, _) = call(&f.app, "GET", "/scim/v2/Users", Some(&condemned), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["status"], json!("401"));
}

async fn the_service_provider_config_is_honest_about_what_is_missing(f: &Fixture) {
    let (status, body, content_type) = call(
        &f.app,
        "GET",
        "/scim/v2/ServiceProviderConfig",
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(content_type.as_deref(), Some("application/scim+json"));
    assert_eq!(body["patch"]["supported"], json!(true), "Entra needs PATCH");
    assert_eq!(body["filter"]["supported"], json!(true));
    // Claiming bulk would make a provider send a bulk request and receive a 404 it cannot explain.
    assert_eq!(body["bulk"]["supported"], json!(false));
    assert_eq!(body["changePassword"]["supported"], json!(false));
}

async fn provisioning_creates_access_and_no_credential(f: &Fixture) {
    let (status, body, content_type) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "userName": "grace@example.com",
            "externalId": "okta-0001",
            "displayName": "Grace Hopper",
            "roles": ["viewer"]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(content_type.as_deref(), Some("application/scim+json"));
    assert_eq!(body["userName"], "grace@example.com");
    assert_eq!(body["externalId"], "okta-0001");
    assert_eq!(body["active"], json!(true));
    assert_eq!(body["roles"], json!(["viewer"]));
    assert_eq!(body["meta"]["resourceType"], "User");
    assert_eq!(
        body["meta"]["location"],
        format!("/scim/v2/Users/{}", body["id"].as_str().unwrap_or_default())
    );
    // The primary email is the userName, which is what a provider reconciles against.
    assert_eq!(body["emails"][0]["value"], "grace@example.com");
    assert_eq!(body["emails"][0]["primary"], json!(true));

    // Deliberately no credential. Putting an API key in a SCIM response would hand the provider a bearer
    // token for a person, into its own logs, for an account it does not authenticate with.
    assert_eq!(body.get("api_key"), None);
    assert_eq!(body.get("token"), None);
    let keys: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.api_keys k \
         JOIN dam_global.identities i ON i.id = k.identity_id \
         WHERE i.email = 'grace@example.com'",
    )
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert_eq!(keys, 0, "provisioning issues nothing to sign in with");

    // The access is real, though: the membership and the roles exist.
    let roles: Vec<String> = sqlx::query_scalar(
        "SELECT m.role_names FROM dam_global.tenant_members m \
         JOIN dam_global.identities i ON i.id = m.identity_id \
         WHERE i.email = 'grace@example.com' AND m.tenant_id = $1",
    )
    .bind(f.tenant_id)
    .fetch_one(&f.global)
    .await
    .expect("roles");
    assert_eq!(roles, vec!["viewer".to_owned()]);

    // And it is in the governance record, attributed to the system rather than to a person who was asleep.
    let (_, log) = govern(f, "/audit?action=identity.provisioned").await;
    assert_eq!(log["entries"][0]["actor_kind"], "system");
    assert_eq!(log["entries"][0]["actor_id"], Value::Null);
    assert_eq!(log["entries"][0]["payload"]["provider"], "Okta");
    assert_eq!(
        log["entries"][0]["payload"]["user_name"],
        "grace@example.com"
    );
}

async fn the_same_person_twice_is_a_uniqueness_conflict(f: &Fixture) {
    let (status, body, _) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({ "userName": "grace@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // The specification's own type, which is what makes a provider update instead of retrying forever.
    assert_eq!(body["scimType"], "uniqueness", "{body}");
    assert_eq!(body["status"], json!("409"));
}

async fn a_person_added_by_hand_is_not_the_providers_to_manage(f: &Fixture) {
    // Otherwise a misconfigured provider disables somebody an administrator added, and the audit trail shows
    // the provider's own token doing it.
    let (status, body, _) = call(
        &f.app,
        "PATCH",
        &format!("/scim/v2/Users/{}", f.by_hand),
        Some(&f.token),
        Some(json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [{ "op": "replace", "path": "active", "value": false }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["scimType"], "mutability");

    // And creating them is refused too, rather than taking the account over.
    let (status, body, _) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({ "userName": "manual@example.com" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["scimType"], "mutability");
}

async fn another_tenants_token_reaches_nothing(f: &Fixture) {
    let grace = user_id(f, "grace@example.com").await;
    let (status, body, _) = call(
        &f.app,
        "GET",
        &format!("/scim/v2/Users/{grace}"),
        Some(&f.other_token),
        None,
    )
    .await;
    // 404 rather than 403: a provider learning that an id exists in another tenant is learning about another
    // customer's directory.
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (_, listing, _) = call(&f.app, "GET", "/scim/v2/Users", Some(&f.other_token), None).await;
    assert_eq!(listing["totalResults"], json!(0), "{listing}");
}

async fn the_list_envelope_is_exactly_what_a_provider_parses(f: &Fixture) {
    let (status, body, _) = call(&f.app, "GET", "/scim/v2/Users", Some(&f.token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["schemas"],
        json!(["urn:ietf:params:scim:api:messages:2.0:ListResponse"])
    );
    // Capitalised. A lowercase `resources` is a provider reading zero users, concluding the directory is
    // empty, and creating everybody again.
    assert!(body["Resources"].is_array(), "{body}");
    assert_eq!(body["startIndex"], json!(1), "1-based, per §3.4.2");
    assert!(body["totalResults"].as_i64().unwrap_or(0) >= 2);
    assert_eq!(
        body["itemsPerPage"].as_i64(),
        body["Resources"].as_array().map(|rows| rows.len() as i64)
    );

    // `count=0` is how a provider asks for a total without paging.
    let (_, counted, _) = call(
        &f.app,
        "GET",
        "/scim/v2/Users?count=0",
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(counted["Resources"], json!([]));
    assert!(counted["totalResults"].as_i64().unwrap_or(0) >= 2);
}

async fn only_the_filters_providers_send_are_accepted(f: &Fixture) {
    let (status, found, _) = call(
        &f.app,
        "GET",
        "/scim/v2/Users?filter=userName%20eq%20%22grace@example.com%22",
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["totalResults"], json!(1), "{found}");
    assert_eq!(found["Resources"][0]["userName"], "grace@example.com");

    let (_, by_external, _) = call(
        &f.app,
        "GET",
        "/scim/v2/Users?filter=externalId%20eq%20%22okta-0001%22",
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(by_external["totalResults"], json!(1), "{by_external}");

    // Refused by name. A filter we drop is a provider receiving the whole directory and concluding every user
    // already matches, which makes a sync a no-op that looks healthy.
    let (status, refused, _) = call(
        &f.app,
        "GET",
        "/scim/v2/Users?filter=displayName%20co%20%22Grace%22",
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["scimType"], "invalidFilter");
}

async fn entra_deprovisions_by_patching_the_string_false(f: &Fixture) {
    let grace = user_id(f, "grace@example.com").await;
    // Give her a credential first, so the revocation is observable — an administrator would have issued one,
    // since provisioning does not.
    let key = issue_key(&f.global, f.tenant_id, grace).await;
    assert!(works(f, &key).await, "the premise");

    // The string, capitalised, exactly as Entra sends it. A strict parse rejects this and the symptom is an
    // employee who has left and still has access.
    let (status, body, _) = call(
        &f.app,
        "PATCH",
        &format!("/scim/v2/Users/{grace}"),
        Some(&f.token),
        Some(json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [{ "op": "Replace", "path": "active", "value": "False" }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], json!(false), "{body}");

    // A flag is not a removal. The credential has to stop working.
    assert!(
        !works(f, &key).await,
        "`active: false` must reach the credential, or it is the flag 0002 warns about"
    );

    // The membership survives, which is the difference from a DELETE — and the record says so.
    let still: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM dam_global.tenant_members WHERE tenant_id = $1 AND identity_id = $2)",
    )
    .bind(f.tenant_id)
    .bind(grace)
    .fetch_one(&f.global)
    .await
    .expect("membership");
    assert!(still);
    let (_, log) = govern(f, "/audit?action=identity.deprovisioned").await;
    assert_eq!(log["entries"][0]["payload"]["via"], "patch");
    assert_eq!(log["entries"][0]["payload"]["membership_kept"], json!(true));
}

async fn reactivation_brings_the_account_back(f: &Fixture) {
    let grace = user_id(f, "grace@example.com").await;
    let (status, body, _) = call(
        &f.app,
        "PATCH",
        &format!("/scim/v2/Users/{grace}"),
        Some(&f.token),
        // The pathless form, which Entra also sends.
        Some(json!({
            "Operations": [{ "op": "replace", "value": { "active": true } }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active"], json!(true));

    // Reversible, like a tenant suspension: a key issued after reactivation works.
    let key = issue_key(&f.global, f.tenant_id, grace).await;
    assert!(works(f, &key).await);
    let (_, log) = govern(f, "/audit?action=identity.reactivated").await;
    assert_eq!(log["entries"][0]["payload"]["provider"], "Okta");
}

async fn okta_deprovisions_by_deleting(f: &Fixture) {
    let grace = user_id(f, "grace@example.com").await;
    let key = issue_key(&f.global, f.tenant_id, grace).await;
    assert!(works(f, &key).await, "the premise");

    // Counted rather than assumed. She has picked up more than one credential over the earlier cases — a
    // reactivation does not bring the revoked ones back, so each issue adds a live key — and asserting "1"
    // would be asserting the order these sub-cases happen to run in rather than what removal does.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dam_global.api_keys \
         WHERE tenant_id = $1 AND identity_id = $2 AND revoked_at IS NULL",
    )
    .bind(f.tenant_id)
    .bind(grace)
    .fetch_one(&f.global)
    .await
    .expect("count");
    assert!(live >= 1);

    let (status, _, _) = call(
        &f.app,
        "DELETE",
        &format!("/scim/v2/Users/{grace}"),
        Some(&f.token),
        None,
    )
    .await;
    // 204, per §3.6. A body describing a user who no longer has access is something a provider has to
    // reconcile.
    assert_eq!(status, StatusCode::NO_CONTENT);

    assert!(
        !works(f, &key).await,
        "delete has to reach the credential too"
    );
    let gone: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM dam_global.tenant_members WHERE tenant_id = $1 AND identity_id = $2)",
    )
    .bind(f.tenant_id)
    .bind(grace)
    .fetch_one(&f.global)
    .await
    .expect("membership");
    assert!(gone, "and the membership, unlike `active: false`");

    let (_, log) = govern(f, "/audit?action=identity.deprovisioned").await;
    assert_eq!(log["entries"][0]["payload"]["via"], "delete");
    assert_eq!(
        log["entries"][0]["payload"]["keys_revoked"],
        json!(live),
        "every credential that worked, and the record says how many"
    );

    // Gone from the provider's view as well, so it does not keep trying.
    let (status, _, _) = call(
        &f.app,
        "GET",
        &format!("/scim/v2/Users/{grace}"),
        Some(&f.token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn an_unsupported_patch_is_refused_rather_than_ignored(f: &Fixture) {
    // A provider told its change applied does not send it again, so silently dropping a PATCH is how a
    // directory and a library disagree permanently.
    let (_, created, _) = call(
        &f.app,
        "POST",
        "/scim/v2/Users",
        Some(&f.token),
        Some(json!({ "userName": "patchee@example.com", "externalId": "okta-0002" })),
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    for (operations, expected) in [
        (
            json!([{ "op": "replace", "path": "displayName", "value": "Nope" }]),
            "invalidValue",
        ),
        (
            json!([{ "op": "remove", "path": "active" }]),
            "invalidValue",
        ),
        (json!([]), "invalidValue"),
    ] {
        let (status, body, _) = call(
            &f.app,
            "PATCH",
            &format!("/scim/v2/Users/{id}"),
            Some(&f.token),
            Some(json!({ "Operations": operations })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["scimType"], expected, "{body}");
    }

    // A PatchOp declaring the wrong schema is a provider sending something else entirely.
    let (status, _, _) = call(
        &f.app,
        "PATCH",
        &format!("/scim/v2/Users/{id}"),
        Some(&f.token),
        Some(json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "Operations": [{ "op": "replace", "path": "active", "value": false }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn a_provider_cannot_mint_another_provisioning_token(f: &Fixture) {
    // Registration takes the ordinary `Manage` gate, not a SCIM token: a provisioning credential that could
    // issue another is a credential that cannot be revoked.
    let (status, _, _) = call(
        &f.app,
        "POST",
        "/scim/clients",
        Some(&f.token),
        Some(json!({ "label": "A second me" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = call(&f.app, "GET", "/scim/clients", Some(&f.token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn contact_is_recorded_so_a_stalled_sync_is_visible(f: &Fixture) {
    // `last_sync_at` and `last_sync_status` are two more columns 0002 declared and nothing filled. Recorded
    // on reads as well as writes, because the most common provider request is a GET and a healthy integration
    // that only ever lists would otherwise look dead.
    let (status, listed, _) = call(&f.app, "GET", "/scim/clients", Some(&f.admin_key), None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let okta = listed
        .as_array()
        .expect("array")
        .iter()
        .find(|client| client["label"] == "Okta")
        .expect("the provider");
    assert_ne!(okta["last_sync_at"], Value::Null, "{okta}");
    assert_ne!(okta["last_sync_status"], Value::Null, "{okta}");
}

// ─── helpers ────────────────────────────────────────────────────────────────

async fn register(f: &Fixture, label: &str) -> (StatusCode, Value) {
    let (status, body, _) = call(
        &f.app,
        "POST",
        "/scim/clients",
        Some(&f.admin_key),
        Some(json!({ "label": label })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        body["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("shown only here")
    );
    (status, body)
}

async fn govern(f: &Fixture, path: &str) -> (StatusCode, Value) {
    let (status, body, _) = call(&f.governance, "GET", path, Some(&f.admin_key), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (status, body)
}

async fn user_id(f: &Fixture, email: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM dam_global.identities WHERE email = $1")
        .bind(email)
        .fetch_one(&f.global)
        .await
        .expect("identity")
}
