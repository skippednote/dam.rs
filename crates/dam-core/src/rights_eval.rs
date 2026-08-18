//! Computing effective rights (2.8, closing GAPS G4).
//!
//! `0005_rights.sql` states the premise: rights are enforced **at the point of distribution**, not
//! recorded in a spreadsheet and hoped for. That is the failure mode of every legacy system — stock
//! licences, model releases and territorial restrictions living in separate tracking documents that
//! nothing checks at download time. This module is the calculation; 3.1's signed-URL chokepoint is where
//! it is enforced, and `rights_evaluations` caches it.
//!
//! ## The intersection, not the union
//!
//! An asset can carry a stock licence *and* a music sync licence *and* an internal brand approval. The
//! effective rights are the intersection: **the most restrictive term wins.** A union would mean attaching
//! one permissive licence launders every restriction on the others, which is the opposite of what an
//! administrator attaching a second licence intends.
//!
//! ## Unknown denies
//!
//! An asset with no licence is [`RightsState::Unknown`], and unknown is not a soft yes. The cost of
//! guessing wrong is a rights claim, so an unevaluated or unlicensed asset does not get distributed.
//! `licenses.ai_training_allowed` and `ai_generation_allowed` default to `false` in the schema for the
//! same reason.
//!
//! ## `Expiring` is a verdict, not a warning attached to `Allowed`
//!
//! Because a 30-day notice is the only thing that actually prevents a lapse. By the time a verdict is
//! `Denied`, somebody has already had to pull an asset off a live site.
//!
//! ## Exclusions beat inclusions
//!
//! Real contracts are written "worldwide except China", and the schema stores exclusions separately to
//! keep that intent. So an excluded territory is denied even when the inclusion list says `WORLD` — the
//! narrower statement is the one the contract meant.

use crate::rights::RightsState;
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// How close to expiry counts as [`RightsState::Expiring`].
///
/// Matches `licenses.renewal_notice_days`' default, and acts as a floor: the window used is the **longest**
/// among the attached licences. A licence that takes ninety days to renew needs ninety days' warning, and
/// a global constant would report it as merely allowed until renewing it was no longer possible.
pub const DEFAULT_NOTICE_DAYS: i64 = 60;

/// The literal territory meaning "everywhere", as stored.
pub const WORLD: &str = "WORLD";

/// What a caller is asking to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// A distribution channel: `web`, `social`, `print`, `ooh`, `broadcast`, …
    ///
    /// Free text rather than an enum because a tenant's channel vocabulary is theirs, and an enum would
    /// mean a migration every time a customer added one.
    pub channel: String,
    /// ISO 3166-1 alpha-2, or [`WORLD`].
    pub territory: String,
}

/// A licence, as far as the calculation cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    pub id: Uuid,
    pub name: String,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    pub perpetual: bool,
    pub renewal_notice_days: i64,
    pub ai_training_allowed: bool,
    pub ai_generation_allowed: bool,
    pub ai_processing_allowed: bool,
    /// The licence's scopes. **An empty list denies**: a licence with no scope grants nothing, which is
    /// different from a scope with empty channel and territory lists (that grants everywhere).
    pub scopes: Vec<Scope>,
}

/// One scope within a licence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub id: Uuid,
    pub territories: Vec<String>,
    pub excluded_territories: Vec<String>,
    /// Empty means all channels — matching the schema's comment.
    pub channels: Vec<String>,
    pub excluded_channels: Vec<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub ends_at: Option<DateTime<Utc>>,
    /// `None` is uncapped; `Some(0)` means none permitted. The distinction is why these are nullable in
    /// the schema rather than defaulting to zero.
    pub max_impressions: Option<i64>,
    pub max_downloads: Option<i64>,
    pub allow_modification: bool,
    pub allow_crop: bool,
}

impl Scope {
    /// Whether this scope covers `usage`.
    fn covers(&self, usage: &Usage) -> bool {
        self.covers_territory(&usage.territory) && self.covers_channel(&usage.channel)
    }

    fn covers_territory(&self, territory: &str) -> bool {
        // Exclusions first, and they win. "Worldwide except China" has `WORLD` in the inclusion list, so
        // checking inclusions first would grant China.
        if self
            .excluded_territories
            .iter()
            .any(|t| t.eq_ignore_ascii_case(territory))
        {
            return false;
        }
        // An excluded territory is also excluded from a WORLD grant when the caller asks for WORLD
        // itself: a request for "everywhere" cannot be satisfied by a grant that carves somewhere out.
        if territory.eq_ignore_ascii_case(WORLD) && !self.excluded_territories.is_empty() {
            return false;
        }
        self.territories
            .iter()
            .any(|t| t.eq_ignore_ascii_case(WORLD) || t.eq_ignore_ascii_case(territory))
    }

    fn covers_channel(&self, channel: &str) -> bool {
        if self
            .excluded_channels
            .iter()
            .any(|c| c.eq_ignore_ascii_case(channel))
        {
            return false;
        }
        // Empty means all channels, per the schema.
        self.channels.is_empty()
            || self
                .channels
                .iter()
                .any(|c| c.eq_ignore_ascii_case(channel))
    }
}

/// A model, property or talent release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub id: Uuid,
    pub kind: String,
    pub subject_name: Option<String>,
    pub starts_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub territories: Vec<String>,
    /// Empty means all channels.
    pub channels: Vec<String>,
    pub subject_is_minor: bool,
    pub guardian_consent: bool,
    /// `valid` | `expired` | `missing` | `disputed` | `withdrawn`.
    pub status: String,
}

/// Consumption recorded against caps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Consumed {
    pub impressions: i64,
    pub downloads: i64,
}

/// Everything the calculation reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inputs {
    pub licenses: Vec<License>,
    pub releases: Vec<Release>,
    /// Consumption per scope id.
    pub consumed: Vec<(Uuid, Consumed)>,
    /// The asset's own legal hold. Blocks distribution as well as deletion.
    pub legal_hold: bool,
}

/// Why a verdict is not `allowed`.
///
/// Machine-readable so a UI and an API can *explain* a denial rather than merely refuse — the schema's
/// `reasons` column exists for this, and a refusal without a reason generates a support ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    pub detail: String,
    /// The licence, scope or release responsible, when one is.
    pub subject: Option<Uuid>,
}

impl Reason {
    fn new(code: &'static str, detail: impl Into<String>, subject: Option<Uuid>) -> Self {
        Self {
            code,
            detail: detail.into(),
            subject,
        }
    }
}

/// The verdict for one (asset, channel, territory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    pub verdict: RightsState,
    pub reasons: Vec<Reason>,
    /// Impressions left across the scopes that cover this usage, if any is capped.
    pub impressions_remaining: Option<i64>,
    pub downloads_remaining: Option<i64>,
    /// The earliest moment this verdict could change on its own.
    ///
    /// Exact rather than a polling guess: a licence window closing or a release lapsing is the only way an
    /// `allowed` becomes `denied` without an input changing, and the worker invalidates on that instant.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether damrs' own enrichment may process this asset.
    ///
    /// Separate from the verdict because it is a different question: an asset may be undistributable in a
    /// territory and still perfectly fine to caption internally.
    pub ai_processing_allowed: bool,
    pub ai_training_allowed: bool,
    pub ai_generation_allowed: bool,
}

impl Evaluation {
    /// Whether distribution is permitted.
    ///
    /// `Expiring` is permitted — that is the point of it being a distinct verdict rather than a denial.
    /// `Unknown` is **not**: see the module docs.
    pub fn permits_distribution(&self) -> bool {
        matches!(self.verdict, RightsState::Allowed | RightsState::Expiring)
    }
}

/// Computes the effective rights for `usage`.
pub fn evaluate(inputs: &Inputs, usage: &Usage, now: DateTime<Utc>) -> Evaluation {
    let mut reasons = Vec::new();

    // The AI gates are an intersection over every attached licence, computed regardless of the
    // distribution verdict — the two questions are independent.
    let ai_processing_allowed =
        inputs.licenses.iter().all(|l| l.ai_processing_allowed) && !inputs.licenses.is_empty();
    let ai_training_allowed =
        !inputs.licenses.is_empty() && inputs.licenses.iter().all(|l| l.ai_training_allowed);
    let ai_generation_allowed =
        !inputs.licenses.is_empty() && inputs.licenses.iter().all(|l| l.ai_generation_allowed);

    let deny = |reasons: Vec<Reason>| Evaluation {
        verdict: RightsState::Denied,
        reasons,
        impressions_remaining: None,
        downloads_remaining: None,
        expires_at: None,
        ai_processing_allowed,
        ai_training_allowed,
        ai_generation_allowed,
    };

    if inputs.legal_hold {
        // Checked first and unconditionally. A legal hold is a legal fact, not a preference, and it
        // outranks every licence that might otherwise permit the use.
        return deny(vec![Reason::new(
            "legal_hold",
            "the asset is under legal hold, which blocks distribution as well as deletion",
            None,
        )]);
    }

    if inputs.licenses.is_empty() {
        // Not denied — *unknown*. The distinction matters operationally: denied means somebody decided,
        // unknown means nobody has, and the second is a queue to work through rather than a refusal to
        // appeal. Both stop distribution.
        return Evaluation {
            verdict: RightsState::Unknown,
            reasons: vec![Reason::new(
                "no_license",
                "no licence is attached, so there is nothing that permits this use; unknown rights are \
                 not permissive",
                None,
            )],
            impressions_remaining: None,
            downloads_remaining: None,
            expires_at: None,
            ai_processing_allowed: false,
            ai_training_allowed: false,
            ai_generation_allowed: false,
        };
    }

    // ── releases ────────────────────────────────────────────────────────────
    // A release that covers this usage and is not valid denies it. A photo can hold a valid stock licence
    // and a lapsed model release, which makes it unusable for advertising and fine for editorial — so the
    // check is scoped to the channel and territory being asked about.
    for release in &inputs.releases {
        if !release_covers(release, usage) {
            continue;
        }
        if let Some(reason) = release_problem(release, now) {
            reasons.push(reason);
        }
    }
    if !reasons.is_empty() {
        return deny(reasons);
    }

    // ── licences ────────────────────────────────────────────────────────────
    // Every attached licence must permit the use. One that does not is the most restrictive term, and the
    // most restrictive term wins.
    let mut earliest_change: Option<DateTime<Utc>> = None;
    let mut impressions_remaining: Option<i64> = None;
    let mut downloads_remaining: Option<i64> = None;
    // The **longest** notice period among the attached licences, not the shortest. A licence that takes
    // ninety days to renew needs ninety days' warning; taking the minimum would report it as merely
    // allowed until it was already too late to renew, which is the one failure this verdict exists to
    // prevent.
    let mut notice_days = DEFAULT_NOTICE_DAYS;

    for license in &inputs.licenses {
        notice_days = notice_days.max(license.renewal_notice_days.max(0));

        if let Some(starts) = license.starts_at
            && now < starts
        {
            reasons.push(Reason::new(
                "license_not_started",
                format!("licence {:?} begins at {starts}", license.name),
                Some(license.id),
            ));
            continue;
        }
        if !license.perpetual
            && let Some(ends) = license.ends_at
        {
            if now >= ends {
                reasons.push(Reason::new(
                    "license_expired",
                    format!("licence {:?} ended at {ends}", license.name),
                    Some(license.id),
                ));
                continue;
            }
            earliest_change = earlier(earliest_change, Some(ends));
        }

        if license.scopes.is_empty() {
            // A licence with no scope grants nothing. Distinct from a scope whose channel and territory
            // lists are empty, which grants everywhere — and conflating the two would turn an
            // incompletely-configured licence into a blanket permission.
            reasons.push(Reason::new(
                "license_unscoped",
                format!(
                    "licence {:?} has no scope, so it grants nothing; a scope with empty channel and \
                     territory lists is how 'everywhere' is expressed",
                    license.name
                ),
                Some(license.id),
            ));
            continue;
        }

        // Within a licence the scopes are alternatives: any one covering the usage permits it.
        let mut covered = false;
        let mut scope_reasons = Vec::new();
        for scope in &license.scopes {
            if !scope.covers(usage) {
                continue;
            }
            if let Some(starts) = scope.starts_at
                && now < starts
            {
                scope_reasons.push(Reason::new(
                    "scope_not_started",
                    format!("scope begins at {starts}"),
                    Some(scope.id),
                ));
                continue;
            }
            if let Some(ends) = scope.ends_at {
                if now >= ends {
                    scope_reasons.push(Reason::new(
                        "scope_expired",
                        format!("scope ended at {ends}"),
                        Some(scope.id),
                    ));
                    continue;
                }
                earliest_change = earlier(earliest_change, Some(ends));
            }

            let consumed = inputs
                .consumed
                .iter()
                .find(|(id, _)| *id == scope.id)
                .map(|(_, c)| *c)
                .unwrap_or_default();

            if let Some(cap) = scope.max_impressions {
                let left = cap - consumed.impressions;
                if left <= 0 {
                    scope_reasons.push(Reason::new(
                        "impressions_exhausted",
                        format!("{consumed:?} impressions used of a {cap} cap"),
                        Some(scope.id),
                    ));
                    continue;
                }
                impressions_remaining =
                    Some(impressions_remaining.map_or(left, |l: i64| l.max(left)));
            }
            if let Some(cap) = scope.max_downloads {
                let left = cap - consumed.downloads;
                if left <= 0 {
                    scope_reasons.push(Reason::new(
                        "downloads_exhausted",
                        format!("{} downloads used of a {cap} cap", consumed.downloads),
                        Some(scope.id),
                    ));
                    continue;
                }
                downloads_remaining = Some(downloads_remaining.map_or(left, |l: i64| l.max(left)));
            }

            covered = true;
        }

        if !covered {
            if scope_reasons.is_empty() {
                reasons.push(Reason::new(
                    "out_of_scope",
                    format!(
                        "licence {:?} does not cover channel {:?} in territory {:?}",
                        license.name, usage.channel, usage.territory
                    ),
                    Some(license.id),
                ));
            } else {
                reasons.extend(scope_reasons);
            }
        }
    }

    if !reasons.is_empty() {
        return deny(reasons);
    }

    // The earliest release expiry also moves the verdict, so it counts toward `expires_at`.
    for release in &inputs.releases {
        if release_covers(release, usage) {
            earliest_change = earlier(earliest_change, release.expires_at);
        }
    }

    let verdict = match earliest_change {
        Some(at) if at - now <= Duration::days(notice_days) => RightsState::Expiring,
        _ => RightsState::Allowed,
    };
    let reasons = if verdict == RightsState::Expiring {
        vec![Reason::new(
            "expiring",
            format!(
                "the earliest term ends at {}, inside the {notice_days}-day notice window",
                earliest_change
                    .map(|at| at.to_rfc3339())
                    .unwrap_or_default()
            ),
            None,
        )]
    } else {
        Vec::new()
    };

    Evaluation {
        verdict,
        reasons,
        impressions_remaining,
        downloads_remaining,
        expires_at: earliest_change,
        ai_processing_allowed,
        ai_training_allowed,
        ai_generation_allowed,
    }
}

/// Whether a release bears on this usage.
fn release_covers(release: &Release, usage: &Usage) -> bool {
    let territory = release
        .territories
        .iter()
        .any(|t| t.eq_ignore_ascii_case(WORLD) || t.eq_ignore_ascii_case(&usage.territory));
    let channel = release.channels.is_empty()
        || release
            .channels
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&usage.channel));
    territory && channel
}

/// The problem with a release, if any.
fn release_problem(release: &Release, now: DateTime<Utc>) -> Option<Reason> {
    let subject = release
        .subject_name
        .clone()
        .unwrap_or_else(|| "an unnamed subject".to_owned());

    // Withdrawn is checked before the date window: a subject who has withdrawn consent has withdrawn it
    // now, whatever the document says about its term.
    match release.status.as_str() {
        "withdrawn" => {
            return Some(Reason::new(
                "release_withdrawn",
                format!("{} withdrew consent", subject),
                Some(release.id),
            ));
        }
        "disputed" => {
            return Some(Reason::new(
                "release_disputed",
                format!("the {} release for {} is disputed", release.kind, subject),
                Some(release.id),
            ));
        }
        "missing" => {
            return Some(Reason::new(
                "release_missing",
                format!("no {} release on file for {}", release.kind, subject),
                Some(release.id),
            ));
        }
        "expired" => {
            return Some(Reason::new(
                "release_expired",
                format!("the {} release for {} has expired", release.kind, subject),
                Some(release.id),
            ));
        }
        _ => {}
    }

    if release.subject_is_minor && !release.guardian_consent {
        return Some(Reason::new(
            "guardian_consent_missing",
            format!("{} is a minor and no guardian consent is recorded", subject),
            Some(release.id),
        ));
    }
    if let Some(starts) = release.starts_at
        && now < starts
    {
        return Some(Reason::new(
            "release_not_started",
            format!("the release for {} begins at {starts}", subject),
            Some(release.id),
        ));
    }
    if let Some(expires) = release.expires_at
        && now >= expires
    {
        // Reached even when `status` still says valid: a status column is worker-maintained and a lapse
        // happens on a clock, so trusting the column alone would distribute on the strength of a job
        // that had not run yet.
        return Some(Reason::new(
            "release_expired",
            format!("the release for {} lapsed at {expires}", subject),
            Some(release.id),
        ));
    }
    None
}

fn earlier(
    current: Option<DateTime<Utc>>,
    candidate: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match (current, candidate) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}
