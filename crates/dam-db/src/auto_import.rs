//! Auto-import mappings: embedded metadata into the tenant's own fields (Q.4).
//!
//! A camera writes `exif.artist`; this tenant calls it `photographer`. The extractor
//! ([`dam_media::embedded`]) produces the left-hand side and the tenant's schema is the right; a mapping is the
//! translation, and it is configuration because only one of those two sides is fixed.
//!
//! ## Three rules, each because the alternative loses somebody's work
//!
//! **Priority decides between sources.** Two mappings can feed one field — `xmp.creator` when an editor set it,
//! `exif.artist` when only the camera did — and "prefer what a person typed" cannot be expressed without an
//! order. Lower fires first.
//!
//! **`overwrite` defaults to false.** Re-running an import over a library somebody has curated would otherwise
//! replace their corrections with whatever the file says: invisibly, automatically, and for every asset at once.
//! Turning it on is a deliberate statement that the file is the source of truth for that field.
//!
//! **An imported value goes through the validator.** It is metadata like any other, so a caption that does not
//! fit an `int` field is *reported* rather than stored — and reported rather than dropped, because a mapping
//! that never produces anything should be visible to whoever configured it.
//!
//! ## Planning is separate from writing
//!
//! [`plan`] returns what *would* change and what would not. Ingest applies it inside the transaction that writes
//! the asset, and the import screen can show it without touching anything — the same computation serving a
//! preview and a write, which is the only way the two can be guaranteed to agree.

use crate::Error;
use dam_core::fields::{Mode, Rejection, Writer};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Why a mapping was refused.
#[derive(Debug, thiserror::Error)]
pub enum MappingRefusal {
    #[error("no field is defined with the key `{0}`")]
    UnknownField(String),

    #[error(
        "`{0}` is not a usable source: a source is `namespace.name`, lower-case, like `exif.artist`"
    )]
    MalformedSource(String),

    #[error("`{0}` is read-only: it describes the file, so an import must not write it")]
    ReadOnlyTarget(String),

    // `source` is deliberately not the field name: thiserror treats a field called `source` as the error's
    // *cause* and demands it implement `Error`. Renamed rather than annotated, because a struct field whose name
    // means something to a derive macro is a trap for the next person.
    #[error("`{from}` already maps to `{field_key}`")]
    Duplicate { from: String, field_key: String },

    #[error("no mapping {0} exists")]
    UnknownMapping(Uuid),

    #[error(transparent)]
    Database(#[from] Error),
}

/// A mapping to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMapping {
    /// The embedded name, as `dam_media::embedded` reports it.
    pub source: String,
    pub field_key: String,
    /// Lower fires first when several sources feed one field.
    pub priority: i32,
    pub overwrite: bool,
    pub enabled: bool,
}

/// A mapping as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub id: Uuid,
    pub source: String,
    pub field_key: String,
    pub priority: i32,
    pub overwrite: bool,
    pub enabled: bool,
}

/// What an import would do, without doing it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Values to write, validated and normalised — ready to merge into the asset's metadata.
    pub values: Map<String, Value>,
    /// Fields left alone because they already hold a value and the mapping does not overwrite.
    ///
    /// Reported rather than silent: "the import did nothing" and "the import respected what was there" look
    /// identical from outside, and only one of them is worth investigating.
    pub skipped: Vec<String>,
    /// Sources that matched but whose value the field would not accept.
    ///
    /// Also reported: a mapping that never produces anything is a configuration mistake, and one that fails
    /// quietly is a mistake nobody finds.
    pub rejected: Vec<Rejection>,
}

type MappingRow = (Uuid, String, String, i32, bool, bool);

fn mapping(row: MappingRow) -> Mapping {
    let (id, source, field_key, priority, overwrite, enabled) = row;
    Mapping {
        id,
        source,
        field_key,
        priority,
        overwrite,
        enabled,
    }
}

/// Whether a source name is shaped like one the extractor could produce.
///
/// Checked in Rust as well as by the column's constraint, so the refusal names the problem instead of surfacing
/// a constraint violation. A mapping whose left-hand side can never be produced is the worst kind of
/// configuration: it looks correct on the screen and silently never fires.
fn source_is_shaped(source: &str) -> bool {
    let Some((namespace, name)) = source.split_once('.') else {
        return false;
    };
    let ok = |part: &str| {
        !part.is_empty()
            && part.starts_with(|c: char| c.is_ascii_lowercase())
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    ok(namespace) && ok(name)
}

/// Creates a mapping.
pub async fn create(pool: &sqlx::PgPool, spec: NewMapping) -> Result<Mapping, MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    create_on(&mut conn, spec).await
}

/// [`create`] on a caller's connection.
pub async fn create_on(
    conn: &mut sqlx::PgConnection,
    spec: NewMapping,
) -> Result<Mapping, MappingRefusal> {
    if !source_is_shaped(&spec.source) {
        return Err(MappingRefusal::MalformedSource(spec.source));
    }

    // The target has to exist and has to be writable. Both are checked here rather than left to the foreign key
    // and to a later validation failure, because both refusals reach an administrator writing an import rule.
    let target: Option<(bool,)> = sqlx::query_as("SELECT read_only FROM field_defs WHERE key = $1")
        .bind(&spec.field_key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
    match target {
        None => return Err(MappingRefusal::UnknownField(spec.field_key)),
        Some((true,)) => return Err(MappingRefusal::ReadOnlyTarget(spec.field_key)),
        Some((false,)) => {}
    }

    let clash: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM auto_import_mappings WHERE source = $1 AND field_key = $2",
    )
    .bind(&spec.source)
    .bind(&spec.field_key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    if clash.is_some() {
        return Err(MappingRefusal::Duplicate {
            from: spec.source,
            field_key: spec.field_key,
        });
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO auto_import_mappings (id, source, field_key, priority, overwrite, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(&spec.source)
    .bind(&spec.field_key)
    .bind(spec.priority)
    .bind(spec.overwrite)
    .bind(spec.enabled)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;

    Ok(Mapping {
        id,
        source: spec.source,
        field_key: spec.field_key,
        priority: spec.priority,
        overwrite: spec.overwrite,
        enabled: spec.enabled,
    })
}

/// Every mapping, best-first within each field.
pub async fn list(pool: &sqlx::PgPool) -> Result<Vec<Mapping>, MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    list_on(&mut conn).await
}

/// [`list`] on a caller's connection.
pub async fn list_on(conn: &mut sqlx::PgConnection) -> Result<Vec<Mapping>, MappingRefusal> {
    let rows: Vec<MappingRow> = sqlx::query_as(
        "SELECT id, source, field_key, priority, overwrite, enabled FROM auto_import_mappings \
         ORDER BY field_key, priority, source",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)?;
    Ok(rows.into_iter().map(mapping).collect())
}

/// One mapping by id.
pub async fn get_on(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<Mapping, MappingRefusal> {
    let row: Option<MappingRow> = sqlx::query_as(
        "SELECT id, source, field_key, priority, overwrite, enabled FROM auto_import_mappings \
         WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(Error::from)?;
    row.map(mapping).ok_or(MappingRefusal::UnknownMapping(id))
}

/// [`set_enabled`] on a caller's connection.
pub async fn set_enabled_on(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    enabled: bool,
) -> Result<(), MappingRefusal> {
    let touched = sqlx::query(
        "UPDATE auto_import_mappings SET enabled = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(enabled)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(MappingRefusal::UnknownMapping(id));
    }
    Ok(())
}

/// [`set_overwrite`] on a caller's connection.
pub async fn set_overwrite_on(
    conn: &mut sqlx::PgConnection,
    id: Uuid,
    overwrite: bool,
) -> Result<(), MappingRefusal> {
    let touched = sqlx::query(
        "UPDATE auto_import_mappings SET overwrite = $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(overwrite)
    .execute(&mut *conn)
    .await
    .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(MappingRefusal::UnknownMapping(id));
    }
    Ok(())
}

/// [`remove`] on a caller's connection.
pub async fn remove_on(conn: &mut sqlx::PgConnection, id: Uuid) -> Result<(), MappingRefusal> {
    let touched = sqlx::query("DELETE FROM auto_import_mappings WHERE id = $1")
        .bind(id)
        .execute(&mut *conn)
        .await
        .map_err(Error::from)?;
    if touched.rows_affected() == 0 {
        return Err(MappingRefusal::UnknownMapping(id));
    }
    Ok(())
}

/// Turns a mapping on or off.
pub async fn set_enabled(
    pool: &sqlx::PgPool,
    id: Uuid,
    enabled: bool,
) -> Result<(), MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    set_enabled_on(&mut conn, id, enabled).await
}

/// Sets whether a mapping may replace a value the asset already has.
pub async fn set_overwrite(
    pool: &sqlx::PgPool,
    id: Uuid,
    overwrite: bool,
) -> Result<(), MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    set_overwrite_on(&mut conn, id, overwrite).await
}

/// Removes a mapping.
pub async fn remove(pool: &sqlx::PgPool, id: Uuid) -> Result<(), MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    remove_on(&mut conn, id).await
}

/// What an import of `embedded` would do to an asset that currently holds `existing`.
pub async fn plan(
    pool: &sqlx::PgPool,
    embedded: &std::collections::BTreeMap<String, String>,
    existing: &Map<String, Value>,
) -> Result<Plan, MappingRefusal> {
    let mut conn = pool.acquire().await.map_err(Error::from)?;
    plan_on(&mut conn, embedded, existing).await
}

/// [`plan`] on a caller's connection, so ingest computes it inside the transaction that writes the asset.
pub async fn plan_on(
    conn: &mut sqlx::PgConnection,
    embedded: &std::collections::BTreeMap<String, String>,
    existing: &Map<String, Value>,
) -> Result<Plan, MappingRefusal> {
    let mut plan = Plan::default();
    if embedded.is_empty() {
        // The common case by a wide margin. Returning early keeps an upload of a file with no metadata from
        // reading the mapping table at all.
        return Ok(plan);
    }

    let mappings = list_on(&mut *conn).await?;

    // One candidate per field: mappings arrive ordered by `(field_key, priority)`, so the first match for a
    // field is the winning one and later ones are alternatives that did not apply.
    let mut candidates: Vec<(String, String, bool)> = Vec::new();
    for mapping in mappings.iter().filter(|m| m.enabled) {
        if candidates
            .iter()
            .any(|(field, _, _)| field == &mapping.field_key)
        {
            continue;
        }
        if let Some(value) = embedded.get(&mapping.source) {
            candidates.push((mapping.field_key.clone(), value.clone(), mapping.overwrite));
        }
    }

    // The target kinds, so a text value can be coerced into the shape its field actually takes.
    //
    // This is where typing belongs, and the extractor's refusal to guess is what makes it safe here: the
    // extractor has only bytes and would be guessing, while this layer knows the field was *declared* an int.
    // Without it a mapping into a numeric field could never fire, which rules out ISO, aperture and dates —
    // exactly the fields people import.
    let defs = crate::fields::load(&mut *conn).await?;

    let mut proposed = Map::new();
    for (field, value, overwrite) in candidates {
        // Present *and not null*: a cleared field is not a value, and refusing to import into one would make
        // "cleared on purpose" permanently unfillable by an import.
        let held = existing.get(&field).is_some_and(|held| !held.is_null());
        if held && !overwrite {
            plan.skipped.push(field);
            continue;
        }
        let kind = defs.iter().find(|def| def.key == field).map(|def| def.kind);
        proposed.insert(field, coerce(&value, kind));
    }

    if proposed.is_empty() {
        return Ok(plan);
    }

    // Through the tenant's own validator, as a human patch: an imported value is metadata, and `Writer::Human`
    // is right because a mapping is an administrator's instruction rather than a model's guess — so `read_only`
    // binds and `ai_writable` does not. `Mode::Patch` because an import fills fields rather than claiming to be
    // a complete record.
    match crate::fields::validate_on(&mut *conn, &proposed, Mode::Patch, Writer::Human).await {
        Ok(accepted) => {
            plan.values = accepted.values.into_iter().collect();
        }
        Err(crate::fields::ValidationOutcome::Rejected(rejections)) => {
            // Per-field rather than all-or-nothing: one unmappable tag must not stop the others, or a single
            // odd camera would make the whole import do nothing. Rejected keys are reported and the rest retried.
            plan.rejected = rejections;
            let refused: Vec<String> = plan.rejected.iter().map(|r| r.key.clone()).collect();
            let survivors: Map<String, Value> = proposed
                .into_iter()
                .filter(|(key, _)| !refused.contains(key))
                .collect();
            if !survivors.is_empty() {
                match crate::fields::validate_on(&mut *conn, &survivors, Mode::Patch, Writer::Human)
                    .await
                {
                    Ok(accepted) => plan.values = accepted.values.into_iter().collect(),
                    // A second refusal means the survivors were not survivors. Reported, not retried again:
                    // one more round would be a loop with no obvious end.
                    Err(crate::fields::ValidationOutcome::Rejected(more)) => {
                        plan.rejected.extend(more);
                    }
                    Err(crate::fields::ValidationOutcome::Failed(error)) => {
                        return Err(MappingRefusal::Database(error));
                    }
                }
            }
        }
        Err(crate::fields::ValidationOutcome::Failed(error)) => {
            return Err(MappingRefusal::Database(error));
        }
    }

    Ok(plan)
}

/// A text value as the shape its field takes.
///
/// Informed rather than guessed: the kind comes from the field's own definition, so this is a conversion and not
/// a heuristic. A value that will not convert is left as text on purpose — the validator then refuses it and the
/// rejection is reported, which is a better outcome than this function inventing a number from `"ISO 400"`.
fn coerce(value: &str, kind: Option<dam_core::fields::FieldKind>) -> Value {
    use dam_core::fields::FieldKind as K;
    let Some(kind) = kind else {
        return Value::String(value.to_owned());
    };
    match kind {
        K::Int => value
            .trim()
            .parse::<i64>()
            .map_or_else(|_| Value::String(value.to_owned()), Value::from),
        K::Decimal => value
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| Value::String(value.to_owned()), Value::Number),
        K::Bool => match value.trim().to_ascii_lowercase().as_str() {
            // The spellings that actually appear in embedded metadata. Anything else stays text and is refused,
            // rather than being read as `false` — which would turn an unrecognised value into a *claim*.
            "true" | "yes" | "1" => Value::Bool(true),
            "false" | "no" | "0" => Value::Bool(false),
            _ => Value::String(value.to_owned()),
        },
        // A `date` field wants a day, and a timestamp names one. Taking the date part is what the field means
        // rather than a reinterpretation of it — and the alternative is that `exif.taken_at`, the most obvious
        // mapping anybody writes, is refused by every date field it is pointed at.
        //
        // Only the split is done here; whether `2026-03-14` is a real date is still the validator's answer, so
        // there is no second date parser.
        // A coordinate arrives as `lat,lon` in decimal degrees, because that is the only shape the extractor
        // could produce without knowing the target: EXIF stores degrees, minutes, seconds and a hemisphere
        // letter, and a `geo` field wants an object. The conversion belongs here for the same reason the int
        // one does — this layer knows the field was *declared* geo.
        K::Geo => {
            let (lat, lon) = match value.trim().split_once(',') {
                Some((lat, lon)) => (lat.trim(), lon.trim()),
                // Left as text, which the validator refuses with a message naming the field. Silently dropping
                // it would make a mapping that never fires and never says why.
                None => return Value::String(value.to_owned()),
            };
            match (lat.parse::<f64>(), lon.parse::<f64>()) {
                (Ok(lat), Ok(lon)) => serde_json::json!({"lat": lat, "lon": lon}),
                _ => Value::String(value.to_owned()),
            }
        }
        K::Date => Value::String(
            value
                .trim()
                .split_once('T')
                .map_or_else(|| value.trim().to_owned(), |(day, _)| day.to_owned()),
        ),
        // Everything else is text on the wire. A `datetime` is deliberately not touched: without an offset there
        // is no instant, so a refusal is the honest outcome — see `dam_media::embedded::iso_timestamp`.
        _ => Value::String(value.to_owned()),
    }
}
