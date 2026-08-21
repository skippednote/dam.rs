/**
 * The typed client.
 *
 * Every shape here comes from `schema.d.ts`, which `damctl openapi --write` generates from the Rust
 * annotations — so the chain F.3 established runs unbroken from a database CHECK constraint to this file.
 * Nothing below re-declares a wire type; a field renamed in Rust becomes a TypeScript error here.
 *
 * ## Errors carry the server's own explanation
 *
 * A 422 from the metadata endpoint names the field and a stable code; a 400 from search names the column
 * the parser stopped at. Collapsing those into "request failed" throws away the only part a user can act
 * on, so [`ApiError`] keeps the parsed body and the callers render it.
 */
import type { components } from './schema';
import { session } from './session.svelte';

export type AssetSummary = components['schemas']['AssetSummary'];
export type AssetPage = components['schemas']['AssetPage'];
export type AssetDetail = components['schemas']['AssetDetail'];
export type Facet = components['schemas']['Facet'];
export type Bucket = components['schemas']['Bucket'];
export type ValidationProblem = components['schemas']['ValidationProblem'];
export type QueryProblem = components['schemas']['QueryProblem'];
export type SortOrder = components['schemas']['SortOrder'];
export type FieldDefinition = components['schemas']['FieldDefinition'];
export type Suggestion = components['schemas']['Suggestion'];
export type BulkPreview = components['schemas']['BulkPreview'];
export type BulkStatus = components['schemas']['BulkStatus'];
export type CreatedShare = components['schemas']['CreatedShare'];
export type ShareRow = components['schemas']['ShareRow'];
export type PortalView = components['schemas']['PortalView'];
export type PortalDownload = components['schemas']['PortalDownload'];
export type SchemaField = components['schemas']['SchemaField'];
export type AmendedField = components['schemas']['AmendedField'];
export type RemovedField = components['schemas']['RemovedField'];
export type MetadataTypeRow = components['schemas']['MetadataTypeRow'];
export type AssetTypeView = components['schemas']['AssetTypeView'];
export type CategoryTree = components['schemas']['TreeRow'];
export type CategoryNode = components['schemas']['NodeRow'];
export type CategoryWorklist = components['schemas']['Worklist'];
export type UploadProfile = components['schemas']['ProfileRow'];
export type AutoImportMapping = components['schemas']['MappingRow'];
export type AiCredential = components['schemas']['CredentialView'];
export type AiBudget = components['schemas']['BudgetView'];
export type AiVerifyResult = components['schemas']['VerifyResult'];
export type EnrichmentSettings = components['schemas']['EnrichmentSettings'];
export type ReviewRow = components['schemas']['ReviewRow'];
export type SuggestedTag = components['schemas']['SuggestedTagView'];
export type MachineField = components['schemas']['MachineFieldView'];
export type BackfillView = components['schemas']['BackfillView'];
export type AskResult = components['schemas']['AskResult'];
export type PortalPage = components['schemas']['PortalPage'];
export type PortalRow = components['schemas']['PortalView'];
export type CreatedPortal = components['schemas']['CreatedPortal'];

/** A failed request, with whatever the server said about it. */
export class ApiError extends Error {
	readonly status: number;
	/** The parsed body, when there was one. `null` for a status-only refusal. */
	readonly body: unknown;

	constructor(status: number, message: string, body: unknown = null) {
		super(message);
		this.name = 'ApiError';
		this.status = status;
		this.body = body;
	}

	/** Field problems from a 422, or an empty list. */
	get problems(): ValidationProblem[] {
		return this.status === 422 && Array.isArray(this.body)
			? (this.body as ValidationProblem[])
			: [];
	}

	/** The query problem from a 400 or 501, when the body carries one. */
	get query(): QueryProblem | null {
		if (this.status !== 400 && this.status !== 501) return null;
		const body = this.body as QueryProblem | null;
		return body && typeof body.message === 'string' ? body : null;
	}

	/** Whether the credential is the problem, so a caller can send the user to reconnect. */
	get unauthenticated(): boolean {
		return this.status === 401;
	}
}

async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
	if (!session.connected) {
		// Thrown rather than attempted: an unauthenticated request produces a 401 that looks like a bad
		// key, and the user would go looking for one they never entered.
		throw new ApiError(401, 'Not connected. Add an API key in Settings.');
	}

	const response = await fetch(`${session.base}${path}`, {
		...init,
		headers: {
			// Set here, once, for anything with a body. Every call site used to declare it, which meant one
			// omission was one 415: axum's JSON extractor refuses a request without it, and the mocked e2e
			// suites never checked a header they were fulfilling themselves. A real click against the real
			// server is what found it — the download endpoint answered 415 while every test passed.
			...(init.body ? { 'Content-Type': 'application/json' } : {}),
			...(init.headers ?? {}),
			Authorization: `Bearer ${session.key}`
		}
	});

	if (!response.ok) {
		// Best-effort: a 401 has no body at all, and a failure to parse must not turn a 401 into a
		// different error than the one the server sent.
		let body: unknown;
		try {
			body = await response.json();
		} catch {
			body = null;
		}
		throw new ApiError(response.status, describe(response.status, body), body);
	}

	if (response.status === 204) return undefined as T;
	return (await response.json()) as T;
}

/** A sentence a user can act on, from a status and whatever body came with it. */
function describe(status: number, body: unknown): string {
	// `reason` first, and it is the one the API actually sends: every refusal that has something specific
	// to say — a schema key already taken, a kind locked by stored values, a share link revoked — puts its
	// sentence there, count included. Falling through to the generic status text discards exactly the part
	// that made the refusal actionable, which is what an e2e case caught: a 409 that knew "12 assets
	// already carry a value" was reaching the user as "Request failed (409)".
	const problem = body as { reason?: string; message?: string } | null;
	if (problem && typeof problem.reason === 'string' && problem.reason) return problem.reason;
	if (problem && typeof problem.message === 'string') return problem.message;
	switch (status) {
		case 401:
			return 'The API key was not accepted. Check it in Settings.';
		case 403:
			return 'That key does not have permission for this.';
		case 404:
			return 'Not found.';
		case 422:
			return 'Some fields were refused.';
		case 501:
			return 'The search index cannot answer that query.';
		case 504:
			return 'The server took too long.';
		default:
			return `Request failed (${status}).`;
	}
}

export async function listAssets(params: {
	offset?: number;
	limit?: number;
	order?: SortOrder;
}): Promise<AssetPage> {
	const query = new URLSearchParams();
	if (params.offset !== undefined) query.set('offset', String(params.offset));
	if (params.limit !== undefined) query.set('limit', String(params.limit));
	if (params.order) query.set('order', params.order);
	return request<AssetPage>(`/assets?${query}`);
}

export async function searchAssets(params: {
	q: string;
	offset?: number;
	limit?: number;
}): Promise<AssetPage> {
	const query = new URLSearchParams({ q: params.q });
	if (params.offset !== undefined) query.set('offset', String(params.offset));
	if (params.limit !== undefined) query.set('limit', String(params.limit));
	return request<AssetPage>(`/search?${query}`);
}

export async function loadFacets(q: string): Promise<Facet[]> {
	return request<Facet[]>(`/search/facets?${new URLSearchParams({ q })}`);
}

/**
 * The current search as a CSV file (Q.18).
 *
 * A `fetch` rather than a link, because the export is authenticated and an `<a href>` carries no header. The
 * blob is handed back rather than saved here: what to do with a file is the page's decision, and a helper that
 * clicked an invisible anchor would be a side effect hidden in a data function.
 *
 * The 422 the server answers for an oversized set arrives as an `ApiError` with the count in its message, which
 * is the sentence worth showing — "too many" without a number is not something anybody can act on.
 */
export async function exportSearchCsv(q: string): Promise<Blob> {
	if (!session.connected) {
		throw new ApiError(401, 'Not connected. Add an API key in Settings.');
	}
	const response = await fetch(`${session.base}/search/export.csv?${new URLSearchParams({ q })}`, {
		headers: { Authorization: `Bearer ${session.key}` }
	});
	if (!response.ok) {
		const body = await response.json().catch(() => null);
		const message =
			body && typeof body === 'object' && 'message' in body
				? String((body as { message: unknown }).message)
				: `Export failed (${response.status}).`;
		throw new ApiError(response.status, message, body);
	}
	return response.blob();
}

/**
 * What somebody is probably about to type (Q.17).
 *
 * `q` is the query already in the box, so suggestions narrow as the search narrows — the same reason the facet
 * rail counts over the current query rather than over the library.
 */
export async function loadSuggestions(typed: string, q: string): Promise<Suggestion[]> {
	return request<Suggestion[]>(`/search/suggest?${new URLSearchParams({ typed, q })}`);
}

/**
 * The tenant's field definitions.
 *
 * Fetched rather than inferred. An earlier version guessed the field list from the facet keys, which meant
 * the editor did not know whether a field was multivalued — so it sent `"blue, red"` to a field that takes an
 * array and the server refused it with a message about delimiters the user could do nothing with. Found by
 * editing a multivalued field in a real browser.
 */
export async function loadFields(): Promise<FieldDefinition[]> {
	return request<FieldDefinition[]>('/fields');
}

export async function getAsset(id: string): Promise<AssetDetail> {
	return request<AssetDetail>(`/assets/${id}`);
}

export async function saveMetadata(
	id: string,
	values: Record<string, unknown>
): Promise<{ values: Record<string, unknown> }> {
	return request(`/assets/${id}/metadata`, {
		method: 'PATCH',
		body: JSON.stringify({ values })
	});
}

/**
 * A delivery URL the server may have sent root-relative, resolved against the API.
 *
 * The server sends an absolute URL only when the deployment configures `server.public_url`; otherwise it sends
 * `/d/<token>`, which a browser would resolve against *this* origin. In development the app is on a Vite port
 * and the API is on another, so that resolves to the wrong server and 404s — which is exactly how this was
 * found.
 */
export function deliveryUrl(url: string): string {
	if (/^https?:\/\//.test(url)) return url;
	return `${session.base}${url.startsWith('/') ? '' : '/'}${url}`;
}

/**
 * Schema administration.
 *
 * Separate from `listFields`, which is the *form's* view: this one carries the usage counts and the
 * `searchable` flag an administrator needs, and its writes need Manage where reading fields needs only Read.
 */
export async function listSchemaFields(): Promise<SchemaField[]> {
	return request<SchemaField[]>('/schema/fields');
}

export async function defineField(body: {
	key: string;
	label: string;
	kind: string;
	multivalued?: boolean;
	required?: boolean;
	read_only?: boolean;
	searchable?: boolean;
	facetable?: boolean;
	ai_writable?: boolean;
	search_alias?: string | null;
	taxonomy_id?: string | null;
	validation?: Record<string, unknown>;
}): Promise<SchemaField> {
	return request<SchemaField>('/schema/fields', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

/** Amends a field. Omitted members are left alone; `search_alias: null` clears it. */
export async function amendField(
	key: string,
	body: Record<string, unknown>
): Promise<AmendedField> {
	return request<AmendedField>(`/schema/fields/${encodeURIComponent(key)}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

/** Removes a definition. The stored values stay, which is what makes this reversible. */
export async function removeField(key: string): Promise<RemovedField> {
	return request<RemovedField>(`/schema/fields/${encodeURIComponent(key)}`, { method: 'DELETE' });
}

/** Sets the whole field order. The server refuses a partial list. */
export async function reorderFields(keys: string[]): Promise<void> {
	await request<void>('/schema/fields/order', {
		method: 'PUT',
		body: JSON.stringify({ keys })
	});
}

/**
 * Upload profiles: what an upload arrives already knowing.
 *
 * Listing needs only Read, deliberately — the uploader has to render the picker and honour the required-field
 * rule before it can upload anything.
 */
export async function listUploadProfiles(): Promise<UploadProfile[]> {
	return request<UploadProfile[]>('/upload-profiles');
}

export async function createUploadProfile(body: {
	key: string;
	label: string;
	metadata_type_id?: string | null;
	defaults?: Record<string, unknown>;
	require_complete?: boolean;
	ai_tags_enabled?: boolean;
	is_default?: boolean;
}): Promise<UploadProfile> {
	return request<UploadProfile>('/upload-profiles', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function amendUploadProfile(
	id: string,
	body: Record<string, unknown>
): Promise<UploadProfile> {
	return request<UploadProfile>(`/upload-profiles/${id}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

export async function removeUploadProfile(id: string): Promise<void> {
	await request<void>(`/upload-profiles/${id}`, { method: 'DELETE' });
}

export type AssetAttachment = components['schemas']['AttachmentView'];

/**
 * Attached documents (Q.9): the paperwork that goes with an asset.
 *
 * Attaching names an asset already uploaded through the ordinary route — the same shape as adding a version, and for
 * the same reason.
 */
export async function listAttachments(assetId: string): Promise<AssetAttachment[]> {
	return request<AssetAttachment[]>(`/assets/${assetId}/attachments`);
}

export async function attachDocument(
	assetId: string,
	documentId: string,
	kind: AssetAttachment['kind']
): Promise<AssetAttachment[]> {
	return request<AssetAttachment[]>(`/assets/${assetId}/attachments`, {
		method: 'POST',
		body: JSON.stringify({ document_id: documentId, kind })
	});
}

/** Detaches a document. Not a delete — it becomes an ordinary asset again. */
export async function detachDocument(assetId: string, documentId: string): Promise<void> {
	await request<void>(`/assets/${assetId}/attachments/${documentId}`, { method: 'DELETE' });
}

export type AssetVersion = components['schemas']['VersionView'];

/**
 * Versions of an asset (Q.8).
 *
 * A history is reachable from any version, not only the current one — somebody looking at an older cut needs to see
 * what replaced it.
 */
export async function listVersions(assetId: string): Promise<AssetVersion[]> {
	return request<AssetVersion[]>(`/assets/${assetId}/versions`);
}

/**
 * Supersedes `assetId` with an asset already uploaded through the ordinary route.
 *
 * Takes an id rather than a file on purpose: a version goes through the same ingest as anything else, so it gets
 * the same sniffing, probing and derivatives. A second upload path would diverge from the first.
 */
export async function addVersion(assetId: string, newAssetId: string): Promise<AssetVersion[]> {
	return request<AssetVersion[]>(`/assets/${assetId}/versions`, {
		method: 'POST',
		body: JSON.stringify({ new_asset_id: newAssetId })
	});
}

/** Makes an earlier version current again. A promotion, so its number does not change. */
export async function makeVersionCurrent(assetId: string): Promise<AssetVersion[]> {
	return request<AssetVersion[]>(`/assets/${assetId}/versions/current`, { method: 'POST' });
}

export type DownloadOptions = components['schemas']['DownloadOptions'];
export type ConversionFormat = components['schemas']['ConversionView'];
export type DownloadIssued = components['schemas']['DownloadIssued'];

/**
 * What an asset can be had as (Q.11).
 *
 * Download rather than Read, on the server: what formats an asset comes in is a question about taking a copy of
 * it. So a reader who may only look gets a 403 here, and the panel says nothing rather than showing an empty list.
 */
export async function loadDownloadOptions(assetId: string): Promise<DownloadOptions> {
	return request<DownloadOptions>(`/assets/${assetId}/download-options`);
}

/**
 * Asks for a copy, in the original or a named format.
 *
 * Returns `{ status: 'rendering' }` the first time somebody asks for a format nobody has asked for yet — the bytes
 * are being made, and the caller asks again. That is a 202 on the wire, which `request` does not distinguish from
 * a 200; the status field is what a client reads, because the difference matters to the person waiting.
 */
export async function requestDownload(
	assetId: string,
	format: string,
	use?: { channel: string; territory: string }
): Promise<DownloadIssued> {
	// The declaration is the *presence* of the fields: sending them means somebody answered, and the ledger row
	// says so. Sending a channel the person did not choose would make the record claim more than was asked.
	return request<DownloadIssued>(`/assets/${assetId}/download`, {
		method: 'POST',
		body: JSON.stringify(use ? { format, ...use } : { format })
	});
}

export type Order = components['schemas']['OrderView'];

/** The private helper, under a name the order functions can use without shadowing their `request` argument. */
const request_ = request;

/**
 * Places an order (Q.13).
 *
 * Read scope on the server, deliberately: an order exists for somebody who may see assets and not take them, so
 * requiring download permission would restrict it to the people who do not need it.
 */
export async function placeOrder(request: {
	asset_ids: string[];
	purpose: string;
	channel?: string;
	territory?: string;
	conversion_key?: string;
	include_metadata?: boolean;
	recipients?: string[];
}): Promise<Order> {
	return request_<Order>('/orders', { method: 'POST', body: JSON.stringify(request) });
}

/** The caller's own orders, newest first. */
export async function loadMyOrders(): Promise<Order[]> {
	return request_<Order[]>('/orders');
}

/** Orders waiting for a decision, oldest first. Manage scope. */
export async function loadOrderQueue(): Promise<Order[]> {
	return request_<Order[]>('/orders/queue');
}

/**
 * Re-issues an order's pickup, returning a fresh link and revoking the previous one.
 *
 * The only way to recover a lost pickup URL: a share token is stored as a digest, so the response that mints it
 * is the one place it exists in readable form. Manage scope.
 */
export async function reissuePickup(id: string): Promise<Order> {
	return request_<Order>(`/orders/${id}/fulfil`, { method: 'POST' });
}

/** Approves, refuses or withdraws an order. */
export async function decideOrder(
	id: string,
	decision: 'approve' | 'reject' | 'cancel',
	note?: string
): Promise<Order> {
	return request_<Order>(`/orders/${id}/${decision}`, {
		method: 'POST',
		body: JSON.stringify(note === undefined ? {} : { note })
	});
}

export type UsageOptions = components['schemas']['UsageOptions'];
export type UsageRecord = components['schemas']['UsageRecord'];

/**
 * What a download may be declared as (Q.12).
 *
 * Derived from the tenant's own licences, so every option is one that can change a rights answer. Read scope: a
 * person filling in the form has not asked for bytes yet.
 */
export async function loadUsageOptions(): Promise<UsageOptions> {
	return request<UsageOptions>('/usage-options');
}

/** What one asset has been taken for, newest first. */
export async function loadUsageLedger(assetId: string): Promise<UsageRecord[]> {
	return request<UsageRecord[]>(`/assets/${assetId}/usage`);
}

export type Dashboard = components['schemas']['Dashboard'];
export type ActivityEntry = components['schemas']['ActivityEntry'];

/**
 * The landing page's data, in one request.
 *
 * One call rather than three, because the page cannot render usefully without all of it — and three would let the
 * counts disagree with the list beneath them.
 */
export async function loadDashboard(): Promise<Dashboard> {
	return request<Dashboard>('/dashboard');
}

/**
 * Everything that has happened to one asset, newest first (Q.10).
 *
 * The same line shape as the dashboard feed, deliberately: one renderer serves both. Covers the whole version group,
 * so a superseded version's history shows what replaced it — the entry that explains all the others.
 */
export async function loadHistory(assetId: string, limit?: number): Promise<ActivityEntry[]> {
	const query = limit === undefined ? '' : `?limit=${limit}`;
	return request<ActivityEntry[]>(`/assets/${assetId}/history${query}`);
}

export type Comment = components['schemas']['CommentView'];
export type Person = components['schemas']['PersonView'];

/**
 * Comments on an asset (Q.6c).
 *
 * Names arrive resolved, so a thread never has to look a person up. Every call needs a person behind the key: a
 * comment is somebody's words.
 */
export async function listComments(assetId: string): Promise<Comment[]> {
	return request<Comment[]>(`/assets/${assetId}/comments`);
}

export async function postComment(
	assetId: string,
	body: {
		body: string;
		visibility?: 'public' | 'private';
		recipients?: string[];
		parent_id?: string | null;
	}
): Promise<Comment> {
	return request<Comment>(`/assets/${assetId}/comments`, {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

/**
 * Rewrites a comment's words, or moves its status — one or the other.
 *
 * The server refuses a request naming both, because they carry different rights: the words are their author's and
 * the status is any reader's. The two arguments are separate here so a caller cannot accidentally send both.
 */
export async function amendComment(
	commentId: string,
	change: { body: string } | { status: Comment['status'] }
): Promise<Comment> {
	return request<Comment>(`/comments/${commentId}`, {
		method: 'PATCH',
		body: JSON.stringify(change)
	});
}

export async function deleteComment(commentId: string): Promise<void> {
	await request<void>(`/comments/${commentId}`, { method: 'DELETE' });
}

/** Everyone in this tenant, for the recipient picker. */
export async function listPeople(): Promise<Person[]> {
	return request<Person[]>('/people');
}

/**
 * Who the caller is.
 *
 * A thread needs it to know which comments it may offer to edit. There is no id to pass — the answer is about the
 * credential — so it cannot be pointed at anybody else.
 */
export async function whoAmI(): Promise<Person> {
	return request<Person>('/me');
}

export type Engagement = components['schemas']['EngagementView'];
export type EngagementList = components['schemas']['ListPage'];

/**
 * Ratings, favourites and watches (Q.5c).
 *
 * Every call needs a person behind the key, and every one answers with the asset's engagement *afterwards* — so
 * a star widget redraws from the write rather than from a read that raced it.
 */
export async function setRating(assetId: string, stars: number): Promise<Engagement> {
	return request<Engagement>(`/assets/${assetId}/rating`, {
		method: 'PUT',
		body: JSON.stringify({ stars })
	});
}

/** Clears the caller's own rating. There is no zero star — see the API docs. */
export async function clearRating(assetId: string): Promise<Engagement> {
	return request<Engagement>(`/assets/${assetId}/rating`, { method: 'DELETE' });
}

export async function setFavourite(assetId: string, on: boolean): Promise<Engagement> {
	return request<Engagement>(`/assets/${assetId}/favourite`, {
		method: on ? 'PUT' : 'DELETE'
	});
}

export async function setWatch(assetId: string, on: boolean): Promise<Engagement> {
	return request<Engagement>(`/assets/${assetId}/watch`, { method: on ? 'PUT' : 'DELETE' });
}

/**
 * The caller's own favourites, in the order they added them.
 *
 * Whole assets rather than ids: there is no endpoint that fetches assets by id set, so ids would have meant one
 * request per row. The page shape matches browse and search, so the same grid renders it.
 */
export async function listFavourites(limit = 50, offset = 0): Promise<EngagementList> {
	return request<EngagementList>(`/favourites?limit=${limit}&offset=${offset}`);
}

export async function listWatches(limit = 50, offset = 0): Promise<EngagementList> {
	return request<EngagementList>(`/watches?limit=${limit}&offset=${offset}`);
}

/**
 * Auto-import mappings: what a file's own EXIF and XMP become.
 *
 * Every call here needs Manage, reading included — nothing a client does depends on knowing the mappings, since
 * they fire on the server during ingest.
 */
export async function listAutoImportMappings(): Promise<AutoImportMapping[]> {
	return request<AutoImportMapping[]>('/auto-import-mappings');
}

/**
 * The embedded names the extractor can actually produce.
 *
 * Fetched rather than written out here: a name this app offered that the server does not produce would create a
 * rule that looks right in the table and silently never fires.
 */
export async function listAutoImportSources(): Promise<string[]> {
	return request<string[]>('/auto-import-mappings/sources');
}

export async function createAutoImportMapping(body: {
	source: string;
	field_key: string;
	priority?: number;
	overwrite?: boolean;
	enabled?: boolean;
}): Promise<AutoImportMapping> {
	return request<AutoImportMapping>('/auto-import-mappings', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function amendAutoImportMapping(
	id: string,
	body: { overwrite?: boolean; enabled?: boolean }
): Promise<AutoImportMapping> {
	return request<AutoImportMapping>(`/auto-import-mappings/${id}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

export async function removeAutoImportMapping(id: string): Promise<void> {
	await request<void>(`/auto-import-mappings/${id}`, { method: 'DELETE' });
}

/**
 * Categories: the tree assets are filed in.
 *
 * The counts on each node are *this caller's* — a group-scoped reader legitimately sees smaller numbers than
 * an administrator, because a count that reported the true total would disclose the size of what they cannot
 * reach.
 */
export async function listCategoryTrees(): Promise<CategoryTree[]> {
	return request<CategoryTree[]>('/categories');
}

export async function readCategoryTree(treeId: string): Promise<CategoryNode[]> {
	return request<CategoryNode[]>(`/categories/${treeId}`);
}

export async function createCategoryTree(body: {
	key: string;
	label: string;
}): Promise<CategoryTree> {
	return request<CategoryTree>('/categories', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function createCategory(
	treeId: string,
	body: { slug: string; label: string; parent_id?: string | null }
): Promise<CategoryNode> {
	return request<CategoryNode>(`/categories/${treeId}/nodes`, {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function readAssetCategories(assetId: string): Promise<CategoryNode[]> {
	return request<CategoryNode[]>(`/assets/${assetId}/categories`);
}

/** Files an asset in a category. Returns the asset's categories afterwards, so chips redraw without a refetch. */
export async function fileInCategory(assetId: string, categoryId: string): Promise<CategoryNode[]> {
	return request<CategoryNode[]>(`/assets/${assetId}/categories/${categoryId}`, { method: 'PUT' });
}

export async function unfileFromCategory(
	assetId: string,
	categoryId: string
): Promise<CategoryNode[]> {
	return request<CategoryNode[]>(`/assets/${assetId}/categories/${categoryId}`, {
		method: 'DELETE'
	});
}

export async function readUncategorised(treeId: string): Promise<CategoryWorklist> {
	return request<CategoryWorklist>(`/categories/${treeId}/uncategorised`);
}

/**
 * Metadata types: which of the tenant's fields apply to which kind of asset.
 *
 * A type is a *selection* over the field vocabulary, not a second vocabulary — so these calls move keys
 * around rather than defining fields, and a field still belongs to `/schema/fields`.
 */
export async function listMetadataTypes(): Promise<MetadataTypeRow[]> {
	return request<MetadataTypeRow[]>('/schema/types');
}

export async function defineMetadataType(body: {
	key: string;
	label: string;
	applies_to?: string[];
	is_default?: boolean;
	field_keys?: string[];
}): Promise<MetadataTypeRow> {
	return request<MetadataTypeRow>('/schema/types', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

/** Amends a type. `field_keys` replaces the whole list, in order. */
export async function amendMetadataType(
	id: string,
	body: { label?: string; applies_to?: string[]; is_default?: boolean; field_keys?: string[] }
): Promise<MetadataTypeRow> {
	return request<MetadataTypeRow>(`/schema/types/${id}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

export async function removeMetadataType(id: string): Promise<void> {
	await request<void>(`/schema/types/${id}`, { method: 'DELETE' });
}

export async function readAssetType(assetId: string): Promise<AssetTypeView> {
	return request<AssetTypeView>(`/assets/${assetId}/metadata-type`);
}

/** Sets or clears an asset's type. `null` clears it, which falls back to the tenant default. */
export async function setAssetType(
	assetId: string,
	metadataTypeId: string | null
): Promise<AssetTypeView> {
	return request<AssetTypeView>(`/assets/${assetId}/metadata-type`, {
		method: 'PUT',
		body: JSON.stringify({ metadata_type_id: metadataTypeId })
	});
}

/** Shares an asset. The returned token appears once and is never retrievable again. */
export async function createShare(body: {
	asset_id: string;
	expires_in_hours?: number;
	max_downloads?: number;
	passcode?: string;
	allow_original?: boolean;
}): Promise<CreatedShare> {
	return request<CreatedShare>('/shares', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function listShares(): Promise<ShareRow[]> {
	return request<ShareRow[]>('/shares');
}

export async function revokeShare(id: string): Promise<void> {
	await request<void>(`/shares/${id}`, { method: 'DELETE' });
}

/**
 * The portal calls, used by the public share page.
 *
 * Unauthenticated by design — the token is the credential — so these bypass the session check that
 * `request` applies: a recipient has no key and must not be told to get one.
 */
export async function portalView(token: string, passcode?: string): Promise<PortalView> {
	return portalCall<PortalView>(`/share/${token}`, passcode);
}

export async function portalDownload(token: string, passcode?: string): Promise<PortalDownload> {
	return portalCall<PortalDownload>(`/share/${token}/download`, passcode);
}

async function portalCall<T>(path: string, passcode?: string): Promise<T> {
	const response = await fetch(`${session.base}${path}`, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(passcode ? { passcode } : {})
	});
	if (!response.ok) {
		let body: unknown;
		try {
			body = await response.json();
		} catch {
			body = null;
		}
		const reason = (body as { reason?: string } | null)?.reason;
		throw new ApiError(response.status, reason ?? `Request failed (${response.status}).`, body);
	}
	return (await response.json()) as T;
}

/**
 * Previews a bulk operation: how many of the selected assets the server will actually touch.
 *
 * The same scope filter creation applies, run server-side — so the number in the confirmation dialog is the
 * number the operation will act on, not the number the client selected.
 */
export async function previewBulk(
	kind: string,
	assetIds: string[],
	params: Record<string, unknown> = {}
): Promise<BulkPreview> {
	return request<BulkPreview>('/bulk/preview', {
		method: 'POST',
		body: JSON.stringify({ kind, asset_ids: assetIds, params })
	});
}

/** Starts a bulk operation. Returns immediately; poll [`bulkStatus`] until `terminal`. */
export async function createBulk(
	kind: string,
	assetIds: string[],
	params: Record<string, unknown> = {}
): Promise<BulkStatus> {
	return request<BulkStatus>('/bulk', {
		method: 'POST',
		body: JSON.stringify({ kind, asset_ids: assetIds, params })
	});
}

export async function bulkStatus(operationId: string): Promise<BulkStatus> {
	return request<BulkStatus>(`/bulk/${operationId}`);
}

/** Liveness, for the Settings page's connection check. Deliberately not authenticated. */
export async function health(base: string): Promise<boolean> {
	try {
		const response = await fetch(`${base.replace(/\/+$/, '')}/health`);
		return response.ok;
	} catch {
		return false;
	}
}

/**
 * The hosted-model credentials and the spend cap.
 *
 * `addAiCredential` is the only call in this client that sends a provider key. Nothing reads one back — there is
 * no endpoint that could — so a screen holding one has it only until this promise resolves.
 */
export async function listAiCredentials(): Promise<AiCredential[]> {
	return request<AiCredential[]>('/ai/credentials');
}

export async function addAiCredential(body: {
	provider: string;
	label: string;
	base_url?: string | null;
	default_model: string;
	api_key: string;
	make_default?: boolean;
}): Promise<AiCredential> {
	return request<AiCredential>('/ai/credentials', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function replaceAiCredentialKey(id: string, apiKey: string): Promise<AiCredential> {
	return request<AiCredential>(`/ai/credentials/${id}/key`, {
		method: 'PUT',
		body: JSON.stringify({ api_key: apiKey })
	});
}

export async function makeAiCredentialDefault(id: string): Promise<AiCredential> {
	return request<AiCredential>(`/ai/credentials/${id}/default`, { method: 'PATCH' });
}

export async function setAiCredentialActive(id: string, isActive: boolean): Promise<AiCredential> {
	return request<AiCredential>(`/ai/credentials/${id}/active`, {
		method: 'PATCH',
		body: JSON.stringify({ is_active: isActive })
	});
}

/** Asks the provider one short question. Costs a few tokens and is the only real call this app makes. */
export async function verifyAiCredential(id: string): Promise<AiVerifyResult> {
	return request<AiVerifyResult>(`/ai/credentials/${id}/verify`, { method: 'POST' });
}

export async function readAiBudget(): Promise<AiBudget> {
	return request<AiBudget>('/ai/budget');
}

export async function setAiBudget(body: {
	limit_cents: number;
	hard?: boolean;
	warn_at_fraction?: number;
}): Promise<AiBudget> {
	return request<AiBudget>('/ai/budget', {
		method: 'PUT',
		body: JSON.stringify(body)
	});
}

/**
 * The enrichment settings, the review queue, and what a model wrote.
 *
 * `readReviewQueue` is a *manage* surface and the server renders it under the caller's predicate, so what comes
 * back is already only what this person may see — the client does no filtering of its own, deliberately: a
 * filter here would be a second place for the rule to live.
 */
export async function readEnrichmentSettings(): Promise<EnrichmentSettings> {
	return request<EnrichmentSettings>('/ai/enrichment');
}

export async function saveEnrichmentSettings(
	body: EnrichmentSettings
): Promise<EnrichmentSettings> {
	return request<EnrichmentSettings>('/ai/enrichment', {
		method: 'PUT',
		body: JSON.stringify(body)
	});
}

export async function readReviewQueue(limit = 50): Promise<ReviewRow[]> {
	return request<ReviewRow[]>(`/ai/review?limit=${limit}`);
}

/** Confirms or rejects one suggested tag. Both are recorded; the rejections are the training signal. */
export async function decideTag(assetId: string, termId: string, accept: boolean): Promise<void> {
	await request<void>(`/assets/${assetId}/tags/${termId}`, {
		method: 'PATCH',
		body: JSON.stringify({ accept })
	});
}

/** Queues one asset for description. Costs money, which is why it is a deliberate click. */
export async function enrichAsset(assetId: string): Promise<{ asset_id: string; job_id: string }> {
	return request<{ asset_id: string; job_id: string }>(`/assets/${assetId}/enrich`, {
		method: 'POST'
	});
}

/** What a model wrote on one asset (the Article 50 disclosure). Readable by anybody who may see the asset. */
export async function readAssetAi(assetId: string): Promise<MachineField[]> {
	return request<MachineField[]>(`/assets/${assetId}/ai`);
}

/** How far a library-wide description has got. */
export async function readBackfill(): Promise<BackfillView> {
	return request<BackfillView>('/ai/backfill');
}

/**
 * Describes the whole library, a batch at a time.
 *
 * One slice at a time by design: the server dedupes on the tenant, so clicking twice does not put two batches in
 * flight. The screen still hides the button while it is running, because a button that appears to do nothing is
 * worse than one that is not there.
 */
export async function startBackfill(
	slice?: number
): Promise<{ job_id: string; outstanding: number }> {
	return request<{ job_id: string; outstanding: number }>('/ai/backfill', {
		method: 'POST',
		body: JSON.stringify(slice === undefined ? {} : { slice })
	});
}

/**
 * Turns a question into the library's own search syntax.
 *
 * Returns a *query*, not results — the answer goes in the search box, where somebody can see what was
 * understood, correct it and keep it, and where the results then come from the ordinary search path. Check
 * `parses` before using it: a query that does not parse is not a query, and the honest fallback is to search the
 * question as plain text, which is what the box would have done for free.
 */
export async function askQuery(question: string): Promise<AskResult> {
	return request<AskResult>('/search/ask', {
		method: 'POST',
		body: JSON.stringify({ question })
	});
}

/**
 * A public portal, by name.
 *
 * Unauthenticated, like the share portal's calls and for the same reason: the visitor has no account, and the
 * page is either public or it is not. `portalCall`'s passcode goes in the query here rather than a body, because
 * a public portal is a page a browser opens with a GET.
 */
export async function portalByKey(key: string, options: { q?: string; passcode?: string } = {}) {
	const url = new URL(`${session.base}/portal/${encodeURIComponent(key)}`);
	if (options.q) url.searchParams.set('q', options.q);
	if (options.passcode) url.searchParams.set('passcode', options.passcode);
	const response = await fetch(url);
	if (!response.ok) {
		let body: unknown;
		try {
			body = await response.json();
		} catch {
			body = null;
		}
		const reason = (body as { reason?: string } | null)?.reason;
		throw new ApiError(response.status, reason ?? 'This portal could not be opened.', body);
	}
	return (await response.json()) as PortalPage;
}

/** A portal by its share token — how a private one is reached. */
export async function portalByToken(
	token: string,
	options: { q?: string; passcode?: string } = {}
): Promise<PortalPage> {
	const response = await fetch(`${session.base}/share/${token}/portal`, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ passcode: options.passcode, q: options.q })
	});
	if (!response.ok) {
		let body: unknown;
		try {
			body = await response.json();
		} catch {
			body = null;
		}
		const reason = (body as { reason?: string } | null)?.reason;
		throw new ApiError(response.status, reason ?? 'This portal could not be opened.', body);
	}
	return (await response.json()) as PortalPage;
}

/** Every portal, retired ones included. Administration, so this one carries the key. */
export async function listPortals(): Promise<PortalRow[]> {
	return request<PortalRow[]>('/portals');
}

export async function createPortal(body: {
	key: string;
	title: string;
	intro?: string;
	kind?: string;
	collection_id: string;
	logo_asset_id?: string | null;
	accent?: string;
	is_public?: boolean;
	allow_search?: boolean;
	passcode?: string;
	expires_in_days?: number;
	max_downloads?: number;
	allow_original?: boolean;
}): Promise<CreatedPortal> {
	return request<CreatedPortal>('/portals', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

export async function retirePortal(id: string): Promise<PortalRow> {
	return request<PortalRow>(`/portals/${id}`, { method: 'DELETE' });
}
