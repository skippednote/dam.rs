//! Effective rights (2.8, GAPS G4).
//!
//! `0005_rights.sql` states the premise this suite defends: rights are enforced **at the point of
//! distribution**, not recorded in a spreadsheet and hoped for. Legacy systems keep stock licences, model
//! releases and territorial restrictions in separate tracking documents that nothing consults at download
//! time — so the interesting cases here are all ones where a naive implementation says yes.
//!
//! Four properties carry the rest:
//!
//! - **Intersection, not union.** Attaching a permissive licence must not launder the restrictions on
//!   another. The most restrictive term wins.
//! - **Unknown denies.** No licence is `unknown`, and unknown is not a soft yes — the cost of guessing
//!   wrong is a rights claim.
//! - **Exclusions beat inclusions.** "Worldwide except China" has `WORLD` in the inclusion list, so
//!   checking inclusions first grants China.
//! - **`Expiring` is a verdict.** A 30-day notice is the only thing that prevents a lapse; by the time it
//!   is `Denied` somebody has already pulled an asset off a live site.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use chrono::{DateTime, Duration, TimeZone, Utc};
use dam_core::rights::RightsState;
use dam_core::rights_eval::{self, Consumed, Evaluation, Inputs, License, Release, Scope, Usage};
use uuid::Uuid;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap()
}

fn days(n: i64) -> Duration {
    Duration::days(n)
}

/// A scope granting everything, everywhere, uncapped.
fn open_scope() -> Scope {
    Scope {
        id: Uuid::new_v4(),
        territories: vec!["WORLD".to_owned()],
        excluded_territories: vec![],
        channels: vec![],
        excluded_channels: vec![],
        starts_at: None,
        ends_at: None,
        max_impressions: None,
        max_downloads: None,
        allow_modification: true,
        allow_crop: true,
    }
}

/// A perpetual licence with one open scope.
fn open_license(name: &str) -> License {
    License {
        id: Uuid::new_v4(),
        name: name.to_owned(),
        starts_at: None,
        ends_at: None,
        perpetual: true,
        renewal_notice_days: 60,
        ai_training_allowed: false,
        ai_generation_allowed: false,
        ai_processing_allowed: true,
        scopes: vec![open_scope()],
    }
}

fn valid_release(kind: &str) -> Release {
    Release {
        id: Uuid::new_v4(),
        kind: kind.to_owned(),
        subject_name: Some("A. Model".to_owned()),
        starts_at: None,
        expires_at: None,
        territories: vec!["WORLD".to_owned()],
        channels: vec![],
        subject_is_minor: false,
        guardian_consent: false,
        status: "valid".to_owned(),
    }
}

fn inputs(licenses: Vec<License>, releases: Vec<Release>) -> Inputs {
    Inputs {
        licenses,
        releases,
        consumed: vec![],
        legal_hold: false,
    }
}

fn web_in(territory: &str) -> Usage {
    Usage {
        channel: "web".to_owned(),
        territory: territory.to_owned(),
    }
}

fn eval(inputs: &Inputs, usage: &Usage) -> Evaluation {
    rights_eval::evaluate(inputs, usage, now())
}

fn codes(e: &Evaluation) -> Vec<&str> {
    e.reasons.iter().map(|r| r.code).collect()
}

// ─── the four load-bearing properties ───────────────────────────────────────

#[test]
fn an_asset_with_no_licence_is_unknown_and_undistributable() {
    // Not `denied`. The distinction is operational: denied means somebody decided, unknown means nobody
    // has — one is a refusal to appeal, the other a queue to work through. Both stop distribution.
    let outcome = eval(&inputs(vec![], vec![]), &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Unknown);
    assert!(
        !outcome.permits_distribution(),
        "unknown rights must not permit distribution — the cost of guessing wrong is a rights claim"
    );
    assert_eq!(codes(&outcome), vec!["no_license"]);
    assert!(
        !outcome.ai_processing_allowed,
        "an unevaluated asset must not be sent to a model either"
    );
}

#[test]
fn a_permissive_licence_does_not_launder_a_restrictive_one() {
    // The intersection property, and the one that makes attaching a second licence safe. Under a union,
    // adding a blanket stock licence would silently lift a music sync restriction.
    let mut restricted = open_license("editorial only");
    restricted.scopes = vec![Scope {
        channels: vec!["editorial".to_owned()],
        ..open_scope()
    }];

    let both = inputs(vec![open_license("blanket"), restricted], vec![]);
    let outcome = eval(&both, &web_in("GB"));
    assert_eq!(
        outcome.verdict,
        RightsState::Denied,
        "the most restrictive term must win: {:?}",
        outcome.reasons
    );
    assert_eq!(codes(&outcome), vec!["out_of_scope"]);

    // And the restrictive licence's own channel still works, so the denial is about scope rather than the
    // second licence poisoning everything.
    let editorial = Usage {
        channel: "editorial".to_owned(),
        territory: "GB".to_owned(),
    };
    assert_eq!(eval(&both, &editorial).verdict, RightsState::Allowed);
}

#[test]
fn an_excluded_territory_is_denied_even_under_a_world_grant() {
    // Real contracts say "worldwide except China", which is why the schema keeps exclusions separately.
    // Checking the inclusion list first would grant China, and the mistake is invisible until somebody
    // ships a campaign there.
    let mut license = open_license("worldwide except CN");
    license.scopes = vec![Scope {
        territories: vec!["WORLD".to_owned()],
        excluded_territories: vec!["CN".to_owned()],
        ..open_scope()
    }];
    let inputs = inputs(vec![license], vec![]);

    assert_eq!(eval(&inputs, &web_in("GB")).verdict, RightsState::Allowed);
    let denied = eval(&inputs, &web_in("CN"));
    assert_eq!(denied.verdict, RightsState::Denied);
    assert_eq!(codes(&denied), vec!["out_of_scope"]);
}

#[test]
fn a_request_for_everywhere_is_refused_when_somewhere_is_carved_out() {
    // Asking for WORLD is asking for every territory, and a grant with an exclusion cannot satisfy that.
    // Answering yes would be the honest-looking bug: the inclusion list does say WORLD.
    let mut license = open_license("worldwide except CN");
    license.scopes = vec![Scope {
        territories: vec!["WORLD".to_owned()],
        excluded_territories: vec!["CN".to_owned()],
        ..open_scope()
    }];
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("WORLD"));
    assert_eq!(outcome.verdict, RightsState::Denied);
}

#[test]
fn a_term_inside_the_notice_window_is_expiring_and_still_distributable() {
    // The verdict that prevents a lapse. It must permit distribution — a warning that blocks is just a
    // denial with extra steps, and people route around it.
    let mut license = open_license("ends soon");
    license.perpetual = false;
    license.ends_at = Some(now() + days(30));
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("GB"));

    assert_eq!(outcome.verdict, RightsState::Expiring);
    assert!(
        outcome.permits_distribution(),
        "expiring must still permit distribution, or nobody heeds the warning"
    );
    assert_eq!(codes(&outcome), vec!["expiring"]);
    assert_eq!(outcome.expires_at, Some(now() + days(30)));
}

#[test]
fn a_term_outside_the_notice_window_is_plainly_allowed() {
    let mut license = open_license("ends later");
    license.perpetual = false;
    license.ends_at = Some(now() + days(400));
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Allowed);
    assert!(outcome.reasons.is_empty());
    assert_eq!(outcome.expires_at, Some(now() + days(400)));
}

#[test]
fn the_longest_notice_period_among_licences_wins() {
    // A licence whose renewal takes ninety days needs ninety days' warning, so the window is the longest
    // among the attached licences rather than the shortest. I had this the wrong way round first: taking
    // the minimum reports a licence as merely allowed until it is already too late to renew, which is the
    // single failure this verdict exists to prevent.
    let mut slow = open_license("ninety-day renewal");
    slow.renewal_notice_days = 90;
    slow.perpetual = false;
    slow.ends_at = Some(now() + days(75));

    let outcome = eval(&inputs(vec![slow], vec![]), &web_in("GB"));
    assert_eq!(
        outcome.verdict,
        RightsState::Expiring,
        "75 days out is inside a 90-day notice window"
    );
}

// ─── licence windows ────────────────────────────────────────────────────────

#[test]
fn an_expired_licence_denies_and_names_itself() {
    let mut license = open_license("lapsed");
    license.perpetual = false;
    license.ends_at = Some(now() - days(1));
    let outcome = eval(&inputs(vec![license.clone()], vec![]), &web_in("GB"));

    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(codes(&outcome), vec!["license_expired"]);
    assert_eq!(
        outcome.reasons[0].subject,
        Some(license.id),
        "the reason must name the licence, or an operator cannot act on it"
    );
}

#[test]
fn a_licence_that_has_not_started_denies() {
    let mut license = open_license("future");
    license.starts_at = Some(now() + days(7));
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("GB"));
    assert_eq!(codes(&outcome), vec!["license_not_started"]);
}

#[test]
fn a_perpetual_licence_ignores_an_end_date() {
    // `perpetual` and `ends_at` can both be set — a contract that was perpetual from a certain date, or
    // sloppy data entry. Perpetual is the stronger statement.
    let mut license = open_license("perpetual with a stale end date");
    license.perpetual = true;
    license.ends_at = Some(now() - days(365));
    assert_eq!(
        eval(&inputs(vec![license], vec![]), &web_in("GB")).verdict,
        RightsState::Allowed
    );
}

#[test]
fn a_licence_with_no_scope_grants_nothing() {
    // Distinct from a scope whose channel and territory lists are empty, which grants everywhere.
    // Conflating them turns a half-configured licence into a blanket permission — the exact shape of
    // mistake that gets discovered by a rights claim.
    let mut license = open_license("unscoped");
    license.scopes = vec![];
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(codes(&outcome), vec!["license_unscoped"]);
}

#[test]
fn scopes_within_one_licence_are_alternatives() {
    // A contract with separate web and print terms grants both. Requiring every scope to match would
    // make a multi-scope licence grant nothing.
    let mut license = open_license("web and print");
    license.scopes = vec![
        Scope {
            channels: vec!["web".to_owned()],
            ..open_scope()
        },
        Scope {
            channels: vec!["print".to_owned()],
            ..open_scope()
        },
    ];
    let inputs = inputs(vec![license], vec![]);

    assert_eq!(eval(&inputs, &web_in("GB")).verdict, RightsState::Allowed);
    let print = Usage {
        channel: "print".to_owned(),
        territory: "GB".to_owned(),
    };
    assert_eq!(eval(&inputs, &print).verdict, RightsState::Allowed);
    let ooh = Usage {
        channel: "ooh".to_owned(),
        territory: "GB".to_owned(),
    };
    assert_eq!(eval(&inputs, &ooh).verdict, RightsState::Denied);
}

#[test]
fn an_excluded_channel_beats_an_empty_channel_list() {
    // Empty means "all channels", so an exclusion is the only way to say "all except social".
    let mut license = open_license("all but social");
    license.scopes = vec![Scope {
        channels: vec![],
        excluded_channels: vec!["social".to_owned()],
        ..open_scope()
    }];
    let inputs = inputs(vec![license], vec![]);

    assert_eq!(eval(&inputs, &web_in("GB")).verdict, RightsState::Allowed);
    let social = Usage {
        channel: "social".to_owned(),
        territory: "GB".to_owned(),
    };
    assert_eq!(eval(&inputs, &social).verdict, RightsState::Denied);
}

#[test]
fn territory_and_channel_matching_is_case_insensitive() {
    // ISO codes arrive in both cases from real imports, and a case-sensitive comparison would deny a
    // perfectly licensed use because a spreadsheet had lowercase country codes.
    let mut license = open_license("gb only");
    license.scopes = vec![Scope {
        territories: vec!["gb".to_owned()],
        channels: vec!["Web".to_owned()],
        ..open_scope()
    }];
    assert_eq!(
        eval(&inputs(vec![license], vec![]), &web_in("GB")).verdict,
        RightsState::Allowed
    );
}

// ─── releases ───────────────────────────────────────────────────────────────

#[test]
fn a_lapsed_model_release_denies_advertising_and_leaves_editorial_alone() {
    // The case `0005_rights.sql` calls out: a valid stock licence plus a lapsed model release makes an
    // asset unusable for advertising and fine for editorial. A release check that ignored the channel
    // would take the asset out of circulation entirely.
    let mut release = valid_release("model");
    release.channels = vec!["advertising".to_owned()];
    release.expires_at = Some(now() - days(1));
    let inputs = inputs(vec![open_license("stock")], vec![release]);

    let advertising = Usage {
        channel: "advertising".to_owned(),
        territory: "GB".to_owned(),
    };
    let denied = eval(&inputs, &advertising);
    assert_eq!(denied.verdict, RightsState::Denied);
    assert_eq!(codes(&denied), vec!["release_expired"]);

    let editorial = Usage {
        channel: "editorial".to_owned(),
        territory: "GB".to_owned(),
    };
    assert_eq!(eval(&inputs, &editorial).verdict, RightsState::Allowed);
}

#[test]
fn a_release_that_lapsed_on_the_clock_denies_even_if_its_status_still_says_valid() {
    // `status` is worker-maintained and a lapse happens on a clock. Trusting the column alone would
    // distribute on the strength of a job that had not run yet — which is precisely the "recorded and
    // hoped for" failure the whole design rejects.
    let mut release = valid_release("model");
    release.status = "valid".to_owned();
    release.expires_at = Some(now() - days(1));
    let outcome = eval(
        &inputs(vec![open_license("stock")], vec![release]),
        &web_in("GB"),
    );
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(codes(&outcome), vec!["release_expired"]);
}

#[test]
fn a_withdrawn_release_denies_immediately_regardless_of_its_term() {
    // Consent withdrawn is withdrawn now, whatever the document says about its dates. Honouring the term
    // over the withdrawal would keep distributing a person's likeness after they asked us to stop.
    let mut release = valid_release("model");
    release.status = "withdrawn".to_owned();
    release.expires_at = Some(now() + days(1000));
    let outcome = eval(
        &inputs(vec![open_license("stock")], vec![release]),
        &web_in("GB"),
    );
    assert_eq!(codes(&outcome), vec!["release_withdrawn"]);
}

#[test]
fn a_minor_without_recorded_guardian_consent_denies() {
    let mut release = valid_release("minor_guardian");
    release.subject_is_minor = true;
    release.guardian_consent = false;
    let outcome = eval(
        &inputs(vec![open_license("stock")], vec![release]),
        &web_in("GB"),
    );
    assert_eq!(codes(&outcome), vec!["guardian_consent_missing"]);
}

#[test]
fn a_minor_with_guardian_consent_is_allowed() {
    let mut release = valid_release("minor_guardian");
    release.subject_is_minor = true;
    release.guardian_consent = true;
    assert_eq!(
        eval(
            &inputs(vec![open_license("stock")], vec![release]),
            &web_in("GB")
        )
        .verdict,
        RightsState::Allowed
    );
}

#[test]
fn a_disputed_or_missing_release_denies() {
    for status in ["disputed", "missing"] {
        let mut release = valid_release("model");
        release.status = status.to_owned();
        let outcome = eval(
            &inputs(vec![open_license("stock")], vec![release]),
            &web_in("GB"),
        );
        assert_eq!(
            outcome.verdict,
            RightsState::Denied,
            "a {status} release must deny"
        );
    }
}

#[test]
fn a_release_expiring_soon_makes_the_verdict_expiring() {
    // A release lapse is as much a reason to warn as a licence ending, and the earliest of the two is
    // what an operator needs to see.
    let mut release = valid_release("model");
    release.expires_at = Some(now() + days(10));
    let outcome = eval(
        &inputs(vec![open_license("stock")], vec![release]),
        &web_in("GB"),
    );
    assert_eq!(outcome.verdict, RightsState::Expiring);
    assert_eq!(outcome.expires_at, Some(now() + days(10)));
}

#[test]
fn a_release_for_another_territory_does_not_bear_on_this_one() {
    let mut release = valid_release("property");
    release.territories = vec!["US".to_owned()];
    release.expires_at = Some(now() - days(1));
    assert_eq!(
        eval(
            &inputs(vec![open_license("stock")], vec![release]),
            &web_in("GB")
        )
        .verdict,
        RightsState::Allowed
    );
}

// ─── caps ───────────────────────────────────────────────────────────────────

#[test]
fn an_exhausted_impression_cap_denies_and_a_partial_one_reports_what_is_left() {
    // Without this, `max_impressions` is decoration — which is what the schema says about a running
    // counter, and equally true of a cap nothing checks.
    let scope = Scope {
        max_impressions: Some(1_000),
        ..open_scope()
    };
    let scope_id = scope.id;
    let mut license = open_license("capped");
    license.scopes = vec![scope];

    let partly_used = Inputs {
        consumed: vec![(
            scope_id,
            Consumed {
                impressions: 400,
                downloads: 0,
            },
        )],
        ..inputs(vec![license.clone()], vec![])
    };
    let outcome = eval(&partly_used, &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Allowed);
    assert_eq!(
        outcome.impressions_remaining,
        Some(600),
        "a UI needs to warn before the last impression is spent"
    );

    let spent = Inputs {
        consumed: vec![(
            scope_id,
            Consumed {
                impressions: 1_000,
                downloads: 0,
            },
        )],
        ..inputs(vec![license], vec![])
    };
    let denied = eval(&spent, &web_in("GB"));
    assert_eq!(denied.verdict, RightsState::Denied);
    assert_eq!(codes(&denied), vec!["impressions_exhausted"]);
}

#[test]
fn a_cap_of_zero_permits_nothing_and_is_not_the_same_as_uncapped() {
    // The reason these columns are nullable rather than defaulting to zero. Reading `Some(0)` as "no cap"
    // would turn "none permitted" into "unlimited", which is the worst possible direction for the error.
    let mut license = open_license("zero cap");
    license.scopes = vec![Scope {
        max_downloads: Some(0),
        ..open_scope()
    }];
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(codes(&outcome), vec!["downloads_exhausted"]);

    let mut uncapped = open_license("uncapped");
    uncapped.scopes = vec![Scope {
        max_downloads: None,
        ..open_scope()
    }];
    assert_eq!(
        eval(&inputs(vec![uncapped], vec![]), &web_in("GB")).verdict,
        RightsState::Allowed
    );
}

// ─── legal hold ─────────────────────────────────────────────────────────────

#[test]
fn a_legal_hold_denies_before_any_licence_is_considered() {
    // A legal fact, not a preference. It outranks every licence that might otherwise permit the use, and
    // it is checked first so no licence evaluation can produce a permissive answer alongside it.
    let held = Inputs {
        legal_hold: true,
        ..inputs(vec![open_license("blanket")], vec![])
    };
    let outcome = eval(&held, &web_in("GB"));
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert_eq!(codes(&outcome), vec!["legal_hold"]);
}

// ─── the AI gates ───────────────────────────────────────────────────────────

#[test]
fn ai_training_and_generation_default_to_denied() {
    // The schema defaults them to false, and the reason is asymmetric cost: a missing feature is an
    // inconvenience, a rights claim over training data is not.
    let outcome = eval(
        &inputs(vec![open_license("ordinary")], vec![]),
        &web_in("GB"),
    );
    assert!(!outcome.ai_training_allowed);
    assert!(!outcome.ai_generation_allowed);
    assert!(
        outcome.ai_processing_allowed,
        "internal cataloguing defaults to allowed — it is not redistribution"
    );
}

#[test]
fn one_licence_forbidding_processing_forbids_it_for_the_asset() {
    // The gate the enrichment DAG reads. Without the intersection, attaching a permissive second licence
    // would send a restricted asset to a vision model as a matter of routine.
    let mut strict = open_license("no machine processing");
    strict.ai_processing_allowed = false;
    let outcome = eval(
        &inputs(vec![open_license("permissive"), strict], vec![]),
        &web_in("GB"),
    );
    assert!(
        !outcome.ai_processing_allowed,
        "the most restrictive term must win here too"
    );
}

#[test]
fn the_ai_gates_are_answered_even_when_distribution_is_denied() {
    // Different questions. An asset may be undistributable in a territory and still perfectly fine to
    // caption internally, and collapsing the two would stop enrichment on a merely regional restriction.
    let mut license = open_license("gb only");
    license.scopes = vec![Scope {
        territories: vec!["GB".to_owned()],
        ..open_scope()
    }];
    let outcome = eval(&inputs(vec![license], vec![]), &web_in("US"));
    assert_eq!(outcome.verdict, RightsState::Denied);
    assert!(
        outcome.ai_processing_allowed,
        "a territorial restriction on distribution says nothing about internal cataloguing"
    );
}

// ─── invalidation ───────────────────────────────────────────────────────────

#[test]
fn expires_at_is_the_earliest_moment_the_verdict_could_change() {
    // What makes cache invalidation exact rather than a polling guess. Reporting the licence end when a
    // release lapses sooner would leave a stale `allowed` in the cache across the lapse.
    let mut license = open_license("ends in 400 days");
    license.perpetual = false;
    license.ends_at = Some(now() + days(400));
    let mut scope_ends = open_scope();
    scope_ends.ends_at = Some(now() + days(300));
    license.scopes = vec![scope_ends];

    let mut release = valid_release("model");
    release.expires_at = Some(now() + days(200));

    let outcome = eval(&inputs(vec![license], vec![release]), &web_in("GB"));
    assert_eq!(
        outcome.expires_at,
        Some(now() + days(200)),
        "the soonest of licence end, scope end and release expiry"
    );
}

#[test]
fn an_uncapped_perpetual_licence_has_no_expiry() {
    let outcome = eval(
        &inputs(vec![open_license("forever")], vec![]),
        &web_in("GB"),
    );
    assert_eq!(outcome.expires_at, None);
    assert_eq!(outcome.verdict, RightsState::Allowed);
}
