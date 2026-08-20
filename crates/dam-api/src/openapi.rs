//! The OpenAPI document — one source of truth for the wire contract (§14.1, F.3).
//!
//! The document is generated from `utoipa` annotations, checked into the repository as
//! `openapi.json`, and consumed by `openapi-typescript` to produce the frontend's types. Three
//! properties make that a gate rather than a convention:
//!
//! - **Emission is deterministic.** The drift check regenerates and diffs, so a document whose key
//!   order varied between runs would fail CI at random and be disabled within a week.
//! - **The checked-in copy is asserted to match.** A stale `openapi.json` fails `mise run check`,
//!   before a push rather than after.
//! - **The frontend imports the generated types.** A variant removed here becomes a TypeScript error
//!   there, which is the only version of this gate that survives contact with a deadline.
//!
//! Every endpoint is listed here explicitly rather than discovered. `utoipa` cannot see a route the router
//! merges, so a handler annotated with `#[utoipa::path]` and left out of this list is absent from the
//! document, absent from the generated client, and therefore unreachable from the frontend in a way that
//! type-checks. The `openapi_contract` test is what catches it.

use utoipa::OpenApi;

/// The document root.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        crate::assets::list,
        crate::assets::detail,
        crate::assets::update_metadata,
        crate::assets::fields,
        crate::search::search,
        crate::search::facets,
        crate::bulk::preview,
        crate::bulk::create,
        crate::bulk::status,
        crate::attachments::list,
        crate::attachments::attach,
        crate::attachments::detach,
        crate::shares::portal_set,
        crate::shares::download_item,
        crate::orders::place,
        crate::orders::fulfil,
        crate::orders::mine,
        crate::orders::queue,
        crate::orders::one,
        crate::orders::approve,
        crate::orders::reject,
        crate::orders::cancel,
        crate::downloads::download,
        crate::downloads::ledger,
        crate::downloads::options,
        crate::conversions::list,
        crate::conversions::create,
        crate::conversions::redefine,
        crate::conversions::set_active,
        crate::conversions::options,
        crate::history::history,
        crate::versions::history,
        crate::versions::add,
        crate::versions::make_current,
        crate::dashboard::dashboard,
        crate::comments::list,
        crate::comments::post_comment,
        crate::comments::amend,
        crate::comments::remove,
        crate::comments::people,
        crate::comments::me,
        crate::engagement::set_rating,
        crate::engagement::clear_rating,
        crate::engagement::add_favourite,
        crate::engagement::remove_favourite,
        crate::engagement::add_watch,
        crate::engagement::remove_watch,
        crate::engagement::favourites,
        crate::engagement::watches,
        crate::auto_import::list,
        crate::auto_import::sources,
        crate::auto_import::create,
        crate::auto_import::amend,
        crate::auto_import::remove,
        crate::upload_profiles::list,
        crate::upload_profiles::create,
        crate::upload_profiles::amend,
        crate::upload_profiles::remove,
        crate::categories::list_trees,
        crate::categories::create_tree,
        crate::categories::read_tree,
        crate::categories::create_node,
        crate::categories::of_asset,
        crate::categories::file,
        crate::categories::unfile,
        crate::categories::uncategorised,
        crate::schema::list,
        crate::schema::define,
        crate::schema::amend,
        crate::schema::remove,
        crate::schema::reorder,
        crate::schema::list_types,
        crate::schema::define_type,
        crate::schema::amend_type,
        crate::schema::remove_type,
        crate::schema::read_asset_type,
        crate::schema::set_asset_type,
        crate::shares::create,
        crate::shares::list,
        crate::shares::revoke,
        crate::shares::portal,
        crate::shares::download,
    ),
    info(
        title = "damrs API",
        description = "Digital asset management: ingest, search, rights, provenance.",
        license(name = "AGPL-3.0-or-later")
    ),
    components(schemas(
        crate::dto::AssetSummary,
        crate::dto::AssetPage,
        crate::assets::AssetDetail,
        crate::assets::MetadataPatch,
        crate::assets::MetadataAccepted,
        crate::assets::ValidationProblem,
        crate::assets::SortOrder,
        crate::assets::FieldDefinition,
        crate::search::Facet,
        crate::search::Bucket,
        crate::search::QueryProblem,
        crate::bulk::BulkRequest,
        crate::bulk::BulkPreview,
        crate::bulk::BulkStatus,
        crate::bulk::BulkFailure,
        crate::attachments::AttachmentView,
        crate::attachments::AttachRequest,
        crate::orders::OrderView,
        crate::shares::PortalSetView,
        crate::shares::PortalItem,
        crate::orders::OrderItemView,
        crate::orders::PlaceOrderRequest,
        crate::orders::DecisionRequest,
        crate::downloads::DownloadRequest,
        crate::downloads::UsageOptions,
        crate::downloads::UsageRecord,
        crate::downloads::DownloadIssued,
        crate::conversions::ConversionView,
        crate::conversions::ConversionRequest,
        crate::conversions::ActiveRequest,
        crate::conversions::DownloadOptions,
        crate::versions::VersionView,
        crate::versions::AddVersionRequest,
        crate::dashboard::Dashboard,
        crate::dashboard::Counts,
        crate::dashboard::ActivityEntry,
        crate::dashboard::Spotlight,
        crate::comments::CommentView,
        crate::comments::PersonView,
        crate::comments::PostCommentRequest,
        crate::comments::AmendCommentRequest,
        crate::engagement::EngagementView,
        crate::engagement::RatingRequest,
        crate::engagement::ListPage,
        crate::auto_import::MappingRow,
        crate::auto_import::CreateMappingRequest,
        crate::auto_import::AmendMappingRequest,
        crate::upload_profiles::ProfileRow,
        crate::upload_profiles::CreateProfileRequest,
        crate::upload_profiles::AmendProfileRequest,
        crate::categories::TreeRow,
        crate::categories::NodeRow,
        crate::categories::CreateTreeRequest,
        crate::categories::CreateNodeRequest,
        crate::categories::Worklist,
        crate::schema::SchemaField,
        crate::schema::DefineRequest,
        crate::schema::AmendRequest,
        crate::schema::AmendedField,
        crate::schema::RemovedField,
        crate::schema::OrderRequest,
        crate::schema::MetadataTypeRow,
        crate::schema::DefineTypeRequest,
        crate::schema::AmendTypeRequest,
        crate::schema::AssetTypeView,
        crate::schema::SetAssetTypeRequest,
        crate::shares::ShareRequest,
        crate::shares::CreatedShare,
        crate::shares::ShareRow,
        crate::shares::PortalRequest,
        crate::shares::PortalView,
        crate::shares::PortalDownload,
        dam_core::AssetTier,
        dam_core::RightsState,
        dam_core::ProvenanceState,
        dam_core::StorageClass,
        dam_core::LatencyClass,
        dam_core::RestoreState,
        dam_core::RestoreTier,
        dam_core::PlacementState,
    ))
)]
pub struct ApiDoc;

/// The document as pretty-printed JSON, with a trailing newline.
///
/// Pretty-printed because it is reviewed in diffs: a single-line document turns every change into an
/// unreadable one. The trailing newline keeps it a well-formed text file, so a later change does not
/// touch the last line for no reason.
///
/// Fallible rather than panicking. Serialising a static document is not a realistic failure, but a
/// library that panics takes its host process with it — and this one is linked into `damd`, not just
/// into a build script.
pub fn document_json() -> Result<String, serde_json::Error> {
    let mut json = ApiDoc::openapi().to_pretty_json()?;
    if !json.ends_with('\n') {
        json.push('\n');
    }
    Ok(json)
}
