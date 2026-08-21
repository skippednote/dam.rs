//! The MCP server: an agent's way in, over the same ABAC layer as the REST API (M5d·2, §8.5).
//!
//! ## The point of this crate is what it does *not* contain
//!
//! §8.5 says these tools run "over the **same ABAC layer** as the REST API, so an external agent can never see
//! more than the acting user". The strongest form of that is not a shared helper or a copied predicate — it is
//! calling the same functions the HTTP handlers call. So `search_assets` is `dam_api::search::run`,
//! `get_download_url` is `dam_api::downloads::issue`, and both were split out of their routes for exactly this.
//! A second implementation would be a second place where the predicate is composed, rights are evaluated and
//! the ledger is written, and the drift would be invisible until an agent saw something it should not.
//!
//! ## Authentication is per call, from the HTTP request
//!
//! An MCP session is long-lived; a DAM's authorisation is not. `rmcp` injects the request's
//! [`http::request::Parts`] into each tool call's context, so every call re-reads the `Authorization` header and
//! re-authorises through `dam_api::caller::authorize`. The consequences are worth stating: a key revoked
//! mid-session stops working on the next call rather than at the end of the session, and a session cannot
//! outlive the permissions it was opened with.
//!
//! ## Absence is the refusal
//!
//! An asset outside the caller's scope is "no such asset", exactly as over HTTP. The reason is §7's: the gap
//! between "you may not see it" and "it does not exist" is an existence oracle, and an agent is precisely the
//! sort of caller that would enumerate it.
//!
//! ## Tool errors, not protocol errors
//!
//! A refusal, a missing asset, a query that does not parse: all of these are `CallToolResult::error`, so the
//! agent reads the sentence and can act on it. A protocol error would tell the agent's *client* that the server
//! is broken, which is a different claim and almost never the true one.

use dam_api::assets::Failure;
use dam_api::caller::{self, Caller};
use dam_api::downloads::DownloadState;
use dam_api::search::{SearchParams, SearchState};
use rmcp::ErrorData;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::sync::Arc;
use uuid::Uuid;

/// Everything the tools need. The same state the HTTP routes hold, because they are the same code paths.
pub struct McpState {
    pub search: Arc<SearchState>,
    pub downloads: Arc<DownloadState>,
}

impl std::fmt::Debug for McpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpState").finish_non_exhaustive()
    }
}

/// The MCP server.
#[derive(Clone)]
pub struct Dam {
    state: Arc<McpState>,
}

impl std::fmt::Debug for Dam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dam").finish_non_exhaustive()
    }
}

impl Dam {
    pub fn new(state: Arc<McpState>) -> Self {
        Self { state }
    }

    /// Authorises one call from the HTTP request the transport carried.
    ///
    /// The header is re-read per call rather than captured at session start — see the module note on why that is
    /// the whole point rather than an inefficiency.
    async fn caller(
        &self,
        context: &RequestContext<RoleServer>,
        action: dam_core::policy::Action,
    ) -> Result<Caller, CallFailed> {
        let parts = context
            .extensions
            .get::<http::request::Parts>()
            .ok_or_else(|| {
                // No HTTP request behind this call. Refused rather than defaulted: the only transport this
                // server is mounted on is HTTP, and a call arriving without one is a call nobody authenticated.
                CallFailed(
                    "this server authenticates per request; no HTTP request was attached"
                        .to_owned(),
                )
            })?;

        let mut headers = http::HeaderMap::new();
        if let Some(value) = parts.headers.get(http::header::AUTHORIZATION) {
            headers.insert(http::header::AUTHORIZATION, value.clone());
        }
        caller::authorize(&self.state.search.global, &headers, action)
            .await
            .map_err(|refusal| {
                CallFailed(
                    match refusal {
                        // Kept apart because the fix differs: one is "your key is wrong", the other is "your key
                        // is right and does not carry this". Neither says more than that — the REST layer
                        // collapses every authentication failure into one answer on purpose, and telling an
                        // agent which guess had the right shape here would undo that.
                        caller::Refusal::Unauthorized => {
                            "this call was not authenticated: send the API key as `Authorization: Bearer <key>`"
                        }
                        caller::Refusal::Forbidden => {
                            "this key does not carry the permission this tool needs"
                        }
                        caller::Refusal::Unsupported(_) => {
                            "this key's configuration names something this build cannot honour yet"
                        }
                        caller::Refusal::Internal => "the server could not check this key",
                    }
                    .to_owned(),
                )
            })
    }
}

/// A tool-level failure, carrying the sentence the agent should read.
struct CallFailed(String);

impl From<CallFailed> for CallToolResult {
    fn from(CallFailed(message): CallFailed) -> Self {
        CallToolResult::error(vec![ContentBlock::text(message)])
    }
}

impl From<dam_api::search::Failure> for CallFailed {
    /// The search route's own failure kind.
    ///
    /// A separate enum from the asset one, and the interesting arm is `Unsupported`: a clause the index cannot
    /// answer is a gap in the server rather than a mistake by the agent, and saying so stops it rewriting a
    /// query that was already right.
    fn from(failure: dam_api::search::Failure) -> Self {
        use dam_api::search::Failure as SearchFailure;
        match failure {
            SearchFailure::Refused(refusal) => CallFailed(match refusal {
                caller::Refusal::Unauthorized => {
                    "this call was not authenticated: send the API key as `Authorization: Bearer <key>`"
                        .to_owned()
                }
                caller::Refusal::Forbidden => {
                    "this key does not carry the permission this tool needs".to_owned()
                }
                caller::Refusal::Unsupported(_) => {
                    "this key's configuration names something this build cannot honour yet".to_owned()
                }
                caller::Refusal::Internal => "the server could not check this key".to_owned(),
            }),
            SearchFailure::BadQuery(problem) => CallFailed(match problem.at {
                Some(column) => format!("that query does not parse: {} (at character {column})", problem.message),
                None => format!("that query does not parse: {}", problem.message),
            }),
            SearchFailure::Unsupported(problem) => CallFailed(format!(
                "this library cannot answer that clause: {}",
                problem.message
            )),
            // Q.18's export cap. Unreachable through the MCP tools — none of them exports a CSV — and mapped
            // rather than defaulted, because the sentence an agent gets when a limit is hit has to say what to
            // do next, and a wildcard arm here would answer "the search failed" to something that is not a
            // failure.
            SearchFailure::TooLarge(message) => CallFailed(message),
            SearchFailure::Internal => CallFailed("the search failed".to_owned()),
        }
    }
}

impl From<Failure> for CallFailed {
    /// Maps an HTTP-shaped refusal onto a sentence.
    ///
    /// `NotFound` becomes "no such asset, or not one this key may see" — the hidden/absent collapse, in words.
    /// Reporting the two differently here would reintroduce through MCP the existence oracle the REST layer
    /// closes.
    fn from(failure: Failure) -> Self {
        CallFailed(match failure {
            Failure::NotFound => "no such asset, or not one this key may see".to_owned(),
            Failure::Forbidden(reason) => reason,
            Failure::Conflict(reason) | Failure::Unprocessable(reason) => reason,
            Failure::Throttled(reason) => reason,
            Failure::Invalid(problems) => problems
                .iter()
                .map(|problem| format!("{}: {}", problem.key, problem.detail))
                .collect::<Vec<String>>()
                .join("; "),
            // The asset failures do not include a query problem — that is the search enum's, above — so this
            // arm set is deliberately smaller than it looks like it should be.
            Failure::Refused(caller::Refusal::Unauthorized) => {
                "this call was not authenticated: send the API key as `Authorization: Bearer <key>`"
                    .to_owned()
            }
            Failure::Refused(caller::Refusal::Forbidden) => {
                "this key does not carry the permission this tool needs".to_owned()
            }
            Failure::Refused(caller::Refusal::Unsupported(_)) => {
                "this key's configuration names something this build cannot honour yet".to_owned()
            }
            Failure::Refused(caller::Refusal::Internal) => {
                "the server could not check this key".to_owned()
            }
            Failure::Internal => "the server failed to answer".to_owned(),
        })
    }
}

/// A JSON object schema, from properties and a required list.
fn schema(properties: Value, required: &[&str]) -> Arc<Map<String, Value>> {
    let mut object = Map::new();
    object.insert("type".to_owned(), json!("object"));
    object.insert("properties".to_owned(), properties);
    object.insert(
        "required".to_owned(),
        Value::Array(required.iter().map(|name| json!(name)).collect()),
    );
    // Closed, so an agent inventing a parameter is told rather than silently ignored.
    object.insert("additionalProperties".to_owned(), json!(false));
    Arc::new(object)
}

/// A string argument, or `None` when absent or empty.
fn text_arg(arguments: Option<&Map<String, Value>>, name: &str) -> Option<String> {
    arguments
        .and_then(|map| map.get(name))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A required asset id argument.
fn asset_arg(arguments: Option<&Map<String, Value>>) -> Result<Uuid, CallFailed> {
    let raw = text_arg(arguments, "asset_id")
        .ok_or_else(|| CallFailed("asset_id is required".to_owned()))?;
    raw.parse().map_err(|_| {
        CallFailed(format!(
            "`{raw}` is not an asset id; they are UUIDs as returned by search_assets"
        ))
    })
}

/// The tools, described for an agent that has never seen this library.
///
/// The descriptions are prompts, not documentation: they are what an agent reads to decide whether to call. So
/// they say what the tool is *for* and what it will refuse, which is the part that stops an agent guessing.
pub fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "search_assets",
            "Search the library. The query uses the DAM's own syntax: bare words search text, `field:value` \
             filters, `in:path` filters by category, `-term` excludes, `OR` alternates, and quotes make a \
             phrase. An empty query matches everything this key may see. Results are only ever assets this key \
             is allowed to see, and the count reflects that.",
            schema(
                json!({
                    "query": {
                        "type": "string",
                        "description": "The query. Empty matches everything visible.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "How many results, 1 to 200. 20 by default.",
                    },
                }),
                &[],
            ),
        ),
        Tool::new(
            "get_asset",
            "Everything recorded about one asset: its file facts, its metadata, its rights state and whether \
             its bytes are immediately available. Answers \"no such asset\" for anything this key may not see, \
             which is deliberate — it is the same answer for a deleted asset and for one out of scope.",
            schema(
                json!({
                    "asset_id": {"type": "string", "description": "The asset's id, from search_assets."},
                }),
                &["asset_id"],
            ),
        ),
        Tool::new(
            "get_brand_guidelines",
            "The library's own guidance and vocabulary: how this organisation wants its assets described, and \
             the controlled terms it files them under. Read this before writing anything that will be stored — \
             it is what makes a description belong to this library rather than to a model's idea of one.",
            schema(json!({}), &[]),
        ),
        Tool::new(
            "check_rights",
            "Whether an asset may be used in a given channel and territory, and why not when it may not. This \
             is a licence question, not a permission question: an asset this key can see may still be refused, \
             and the reasons say which clause refused it. Ask before promising a client an asset.",
            schema(
                json!({
                    "asset_id": {"type": "string", "description": "The asset's id."},
                    "channel": {
                        "type": "string",
                        "description": "Where it would be used: `web`, `print`, `social`, `internal`, and so \
                                        on. `internal` by default.",
                    },
                    "territory": {
                        "type": "string",
                        "description": "An ISO country code, or `WORLD`. `WORLD` by default.",
                    },
                }),
                &["asset_id"],
            ),
        ),
        Tool::new(
            "get_download_url",
            "A time-limited URL for an asset's bytes. Every one of these is recorded against the licence it was \
             taken under, so the channel and territory are part of the request rather than an afterthought. \
             Refuses when rights refuse, when the key may not download, and when the format has to be rendered \
             first — in which case it says so and the render is queued.",
            schema(
                json!({
                    "asset_id": {"type": "string", "description": "The asset's id."},
                    "format": {
                        "type": "string",
                        "description": "A named format from the library's own list, or `original` for the \
                                        untransformed file. `original` by default.",
                    },
                    "channel": {"type": "string", "description": "Where it will be used."},
                    "territory": {"type": "string", "description": "Where it will be used."},
                }),
                &["asset_id"],
            ),
        ),
    ]
}

impl rmcp::ServerHandler for Dam {
    async fn initialize(
        &self,
        _request: rmcp::model::InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // Said once, in the instructions, rather than repeated in every tool description: an agent that
        // knows these two rules asks better questions and is refused less often.
        Ok(
                InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                    .with_server_info(
                        Implementation::new("damrs", env!("CARGO_PKG_VERSION"))
                            .with_title("damrs digital asset management"),
                    )
                    .with_instructions(
                        "This is a digital asset library. Two rules shape every answer. First, you see only \
                         what the API key you are using is allowed to see — an asset outside its scope is \
                         reported as not existing, so an empty result is not evidence that something is absent \
                         from the library. Second, being able to see an asset is not permission to use it: ask \
                         check_rights before promising anything, and take bytes only through \
                         get_download_url, which records the use against the licence.",
                    ),
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = request.arguments.as_ref();
        let result = match request.name.as_ref() {
            "search_assets" => self.search_assets(&context, arguments).await,
            "get_asset" => self.get_asset(&context, arguments).await,
            "get_brand_guidelines" => self.get_brand_guidelines(&context).await,
            "check_rights" => self.check_rights(&context, arguments).await,
            "get_download_url" => self.get_download_url(&context, arguments).await,
            // A name this build does not have is a *protocol* error: the agent's client asked for something
            // that is not in `tools/list`, which is a fault in the conversation rather than in the library.
            other => {
                return Err(ErrorData::invalid_params(
                    format!("no such tool: {other}"),
                    None,
                ));
            }
        };
        Ok(CallToolResponse::Complete(match result {
            Ok(value) => structured(value),
            Err(failed) => failed.into(),
        }))
    }
}

/// A successful answer, as both JSON and text.
///
/// Both, deliberately: `structured_content` is what a program reads, and the text block is what a model reads.
/// Sending only the former makes the tool useless to the clients that do not support it yet; only the latter
/// makes an agent parse prose.
fn structured(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(value);
    result
}

impl Dam {
    async fn search_assets(
        &self,
        context: &RequestContext<RoleServer>,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Value, CallFailed> {
        let caller = self.caller(context, dam_core::policy::Action::Read).await?;
        let limit = arguments
            .and_then(|map| map.get("limit"))
            .and_then(Value::as_i64)
            .unwrap_or(20)
            .clamp(1, 200);
        let params = SearchParams {
            q: text_arg(arguments, "query").unwrap_or_default(),
            offset: 0,
            limit,
        };

        // The route's own function. Everything §7 requires — the predicate in the query, the index results
        // re-checked against Postgres — happens because this is that code and not a copy of it.
        let page = dam_api::search::run(&self.state.search, &caller, &params)
            .await
            .map_err(CallFailed::from)?;

        Ok(json!({
            "total": page.total,
            "ranked": page.ranked,
            "assets": page
                .items
                .iter()
                .map(|item| json!({
                    "asset_id": item.id,
                    "filename": item.filename,
                    "mime": item.mime,
                    "width": item.width,
                    "height": item.height,
                    "rights_state": item.rights_state,
                    "tier": item.tier,
                }))
                .collect::<Vec<Value>>(),
        }))
    }

    async fn get_asset(
        &self,
        context: &RequestContext<RoleServer>,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Value, CallFailed> {
        let caller = self.caller(context, dam_core::policy::Action::Read).await?;
        let asset_id = asset_arg(arguments)?;

        let mut conn = dam_db::TenantConn::begin(&self.state.search.global, &caller.tenant_slug)
            .await
            .map_err(|_| CallFailed("the tenant's data could not be read".to_owned()))?;
        let found = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id).await;
        let values: Option<Value> =
            sqlx::query_scalar("SELECT values FROM asset_metadata WHERE asset_id = $1")
                .bind(asset_id)
                .fetch_optional(conn.executor())
                .await
                .ok()
                .flatten();
        let _ = conn.commit().await;

        let Some(detail) =
            found.map_err(|_| CallFailed("the asset could not be read".to_owned()))?
        else {
            // The collapse, in words. See `From<Failure> for CallFailed`.
            return Err(CallFailed(
                "no such asset, or not one this key may see".to_owned(),
            ));
        };

        Ok(json!({
            "asset_id": detail.summary.id,
            "filename": detail.summary.filename,
            "mime": detail.summary.mime,
            "bytes": detail.summary.bytes,
            "width": detail.summary.width,
            "height": detail.summary.height,
            "rights_state": detail.summary.rights_state,
            "provenance_state": detail.summary.provenance_state,
            "tier": detail.summary.tier,
            "metadata": values.unwrap_or_else(|| json!({})),
        }))
    }

    async fn get_brand_guidelines(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<Value, CallFailed> {
        let caller = self.caller(context, dam_core::policy::Action::Read).await?;
        let mut conn = dam_db::TenantConn::begin(&self.state.search.global, &caller.tenant_slug)
            .await
            .map_err(|_| CallFailed("the tenant's data could not be read".to_owned()))?;
        let settings = dam_db::enrichment::settings(conn.executor())
            .await
            .map_err(|_| CallFailed("the guidance could not be read".to_owned()))?;
        // The same vocabulary the enrichment pipeline offers a model, and the same bound: past a few hundred
        // terms a list stops being useful to anybody, human or otherwise.
        let (vocabulary, total) = dam_db::enrichment::vocabulary(conn.executor(), 400)
            .await
            .map_err(|_| CallFailed("the vocabulary could not be read".to_owned()))?;
        let fields = dam_db::fields::load(conn.executor())
            .await
            .map_err(|_| CallFailed("the schema could not be read".to_owned()))?;
        let _ = conn.commit().await;

        Ok(json!({
            "guidance": settings.guidance,
            "language": settings.language,
            "vocabulary": vocabulary
                .iter()
                .map(|(slug, label, synonyms)| json!({
                    "slug": slug,
                    "label": label,
                    "synonyms": synonyms,
                }))
                .collect::<Vec<Value>>(),
            "vocabulary_total": total,
            "vocabulary_truncated": total > vocabulary.len() as i64,
            "fields": fields
                .iter()
                .map(|def| json!({
                    "key": def.key,
                    "kind": def.kind.as_str(),
                    "ai_writable": def.ai_writable,
                }))
                .collect::<Vec<Value>>(),
        }))
    }

    async fn check_rights(
        &self,
        context: &RequestContext<RoleServer>,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Value, CallFailed> {
        // Read, not Download: asking whether an asset *could* be used is not using it, and an agent that had to
        // hold Download to ask would be an agent that finds out by trying.
        let caller = self.caller(context, dam_core::policy::Action::Read).await?;
        let asset_id = asset_arg(arguments)?;
        let usage = dam_core::rights_eval::Usage {
            channel: text_arg(arguments, "channel").unwrap_or_else(|| "internal".to_owned()),
            territory: text_arg(arguments, "territory").unwrap_or_else(|| "WORLD".to_owned()),
        };

        let mut conn = dam_db::TenantConn::begin(&self.state.search.global, &caller.tenant_slug)
            .await
            .map_err(|_| CallFailed("the tenant's data could not be read".to_owned()))?;
        // The asset gate first: an asset out of scope must not be answerable through a rights question either.
        let visible = dam_db::assets::detail(conn.executor(), &caller.predicate, asset_id)
            .await
            .map_err(|_| CallFailed("the asset could not be read".to_owned()))?;
        if visible.is_none() {
            let _ = conn.commit().await;
            return Err(CallFailed(
                "no such asset, or not one this key may see".to_owned(),
            ));
        }
        let evaluation =
            dam_db::rights::evaluate_on(conn.executor(), asset_id, &usage, chrono::Utc::now())
                .await
                .map_err(|_| CallFailed("the rights could not be evaluated".to_owned()))?;
        conn.commit()
            .await
            .map_err(|_| CallFailed("the evaluation could not be recorded".to_owned()))?;

        Ok(json!({
            "asset_id": asset_id,
            "channel": usage.channel,
            "territory": usage.territory,
            "verdict": evaluation.verdict.as_str(),
            "may_distribute": evaluation.permits_distribution(),
            "reasons": evaluation
                .reasons
                .iter()
                .map(|reason| json!({
                    "code": reason.code,
                    "detail": reason.detail,
                    "subject": reason.subject,
                }))
                .collect::<Vec<Value>>(),
        }))
    }

    async fn get_download_url(
        &self,
        context: &RequestContext<RoleServer>,
        arguments: Option<&Map<String, Value>>,
    ) -> Result<Value, CallFailed> {
        let caller = self
            .caller(context, dam_core::policy::Action::Download)
            .await?;
        let asset_id = asset_arg(arguments)?;
        let request = dam_api::downloads::DownloadRequest {
            // `original` spelled here rather than pulled from `dam_media`: this crate has no other reason to
            // depend on the renderer, and the name is part of the download API's own vocabulary.
            format: text_arg(arguments, "format").unwrap_or_else(|| "original".to_owned()),
            channel: text_arg(arguments, "channel"),
            territory: text_arg(arguments, "territory"),
        };

        // The route's own function: rights evaluated, the use recorded against the licence, the token minted at
        // the one chokepoint. An agent taking bytes leaves the same trail a person does.
        let (_, issued) =
            dam_api::downloads::issue(&self.state.downloads, &caller, asset_id, &request)
                .await
                .map_err(CallFailed::from)?;

        Ok(json!({
            "asset_id": asset_id,
            "format": issued.format,
            "status": issued.status,
            "url": issued.url,
        }))
    }
}

/// Names the tools this build offers, for a test that wants to assert the set rather than the wire format.
pub fn tool_names() -> Vec<Cow<'static, str>> {
    tools().into_iter().map(|tool| tool.name).collect()
}

/// The HTTP surface: one route, mounted by `damd`.
///
/// ## Why the transport is rmcp's rather than hand-rolled JSON-RPC
///
/// Because the protocol has more in it than `tools/call`: session negotiation, protocol-version handling, SSE
/// framing for servers that stream, `Host` and `Origin` validation against DNS rebinding. A hand-rolled endpoint
/// would answer the happy path and quietly fail every client that used the rest.
///
/// ## Stateless, JSON responses
///
/// `legacy_session_mode` off and `json_response` on: none of these tools stream or send server-initiated
/// messages, so a session buys nothing and costs a per-session handler that would outlive the permissions it was
/// created with. Statelessly, every POST carries its own credential — which is what makes "the same ABAC layer"
/// true per call rather than per connection.
///
/// ## Hosts and origins are a deployment fact
///
/// rmcp defaults to loopback-only `Host` validation, which is right for a laptop and wrong for a deployment. The
/// public origin the rest of the API already knows is added to the list, so the same configuration that makes
/// delivery URLs work makes this reachable — and nothing wider is opened by accident.
pub fn router(state: Arc<McpState>, public_url: Option<&str>) -> axum::Router {
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::tower::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let mut allowed_hosts = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
    ];
    if let Some(host) = public_url.and_then(host_of) {
        allowed_hosts.push(host);
    }

    let service = StreamableHttpService::new(
        {
            let state = Arc::clone(&state);
            move || Ok(Dam::new(Arc::clone(&state)))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_allowed_hosts(allowed_hosts),
    );

    axum::Router::new().route_service("/mcp", service)
}

/// The host — with its port, when there is one — of a configured public URL.
///
/// Written by hand rather than pulled from a URL crate: this is the only place the server parses one, and the
/// answer needed is "what would a client send in `Host`", which is exactly the authority.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let authority = authority.split('@').next_back().unwrap_or(authority);
    (!authority.is_empty()).then(|| authority.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_tools_the_architecture_names_are_the_five_that_exist() {
        // §8.5 names them, and an agent's prompt is built from this list — a tool quietly renamed is a tool the
        // agent stops calling.
        assert_eq!(
            tool_names(),
            vec![
                "search_assets",
                "get_asset",
                "get_brand_guidelines",
                "check_rights",
                "get_download_url",
            ]
        );
    }

    #[test]
    fn every_tool_has_a_closed_schema_and_a_description() {
        for tool in tools() {
            let schema = &tool.input_schema;
            assert_eq!(schema["type"], json!("object"), "{}", tool.name);
            // Closed, so an agent inventing a parameter is told rather than silently ignored.
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{} accepts anything",
                tool.name
            );
            let description = tool.description.as_deref().unwrap_or("");
            assert!(
                description.len() > 80,
                "{} has no description worth reading",
                tool.name
            );
        }
    }

    #[test]
    fn the_asset_tools_require_an_asset() {
        // A required list that drifted from the parameters is how an agent gets a "missing field" it cannot
        // diagnose — the schema says required, the handler never reads it, or the reverse.
        for tool in tools() {
            let required = tool.input_schema["required"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let takes_asset = tool.input_schema["properties"]
                .as_object()
                .is_some_and(|properties| properties.contains_key("asset_id"));
            assert_eq!(
                takes_asset,
                required.contains(&json!("asset_id")),
                "{} disagrees with itself about asset_id",
                tool.name
            );
        }
    }

    #[test]
    fn a_public_url_becomes_an_allowed_host() {
        assert_eq!(
            host_of("https://dam.example.com"),
            Some("dam.example.com".to_owned())
        );
        // With a port, because that is what a client sends in `Host`.
        assert_eq!(
            host_of("http://localhost:8080/api"),
            Some("localhost:8080".to_owned())
        );
        assert_eq!(
            host_of("https://user@dam.example.com/x"),
            Some("dam.example.com".to_owned())
        );
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn a_refused_call_reads_as_a_sentence_rather_than_a_status() {
        // What an agent sees. A tool error carries text it can act on; a bare code would make it retry.
        let result: CallToolResult = CallFailed::from(Failure::NotFound).into();
        assert_eq!(result.is_error, Some(true));
        let ContentBlock::Text(text) = &result.content[0] else {
            panic!("expected text, got {:?}", result.content[0]);
        };
        // The collapse: "not found" and "not yours" are one answer, in words.
        assert!(
            text.text
                .contains("no such asset, or not one this key may see"),
            "{}",
            text.text
        );
    }
}
