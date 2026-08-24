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

	// Read the body, then decide — rather than deciding from the status code. The first version special-cased
	// 204 alone, so the first endpoint to answer 202 with no content (the webhook retry) threw a JSON parse
	// error inside a successful call, which surfaced as "could not retry" for a retry that had worked. Any
	// no-content response is now handled, whatever status it carries.
	const text = await response.text();
	if (text.length === 0) return undefined as T;
	return JSON.parse(text) as T;
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
 * A file from an authenticated endpoint.
 *
 * A `fetch` rather than a link, because the export is authenticated and an `<a href>` carries no header. The
 * blob is handed back rather than saved here: what to do with a file is the page's decision, and a helper that
 * clicked an invisible anchor would be a side effect hidden in a data function.
 *
 * One function rather than one per export, for the reason the Rust `csv_export` module gives about itself:
 * written twice they drift, and the person who notices is the one whose download silently returns a JSON error
 * body with a `.csv` name on it. The server's own sentence is kept — the 422 for an oversized set carries the
 * count, and "too many" without a number is not something anybody can act on.
 */
async function requestBlob(path: string): Promise<Blob> {
	if (!session.connected) {
		throw new ApiError(401, 'Not connected. Add an API key in Settings.');
	}
	const response = await fetch(`${session.base}${path}`, {
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

/** The current search as a CSV file (Q.18). */
export async function exportSearchCsv(q: string): Promise<Blob> {
	return requestBlob(`/search/export.csv?${new URLSearchParams({ q })}`);
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
export type RailEntry = components['schemas']['RailEntry'];

/**
 * The refine-search rail's configuration (Q.19).
 *
 * Every entry the rail can show, in the order it will show them, with the disabled ones at the end — you
 * cannot re-enable what you cannot see.
 */
export async function listRail(): Promise<RailEntry[]> {
	return request<RailEntry[]>('/schema/facets');
}

/** The whole ordered list of enabled entries. A partial write would be an order nobody asked for. */
export async function setRail(enabled: string[]): Promise<void> {
	await request<void>('/schema/facets', {
		method: 'PUT',
		body: JSON.stringify({ enabled })
	});
}

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
		/**
		 * Pin the comment to a region: `[x, y, w, h]` as **fractions** of the image, origin top-left (M6).
		 *
		 * Fractions rather than pixels, because the element somebody drew the box on and the element that
		 * renders it later are different sizes — a preview, a thumbnail, the original. Divide by the rendered
		 * box, never by the natural size, or a letterboxed image puts the mark in the wrong place.
		 */
		region?: [number, number, number, number] | null;
		/** Pin it to a moment in a video or audio track, in milliseconds. */
		at_ms?: number | null;
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

// ─── archival (§6.5) ────────────────────────────────────────────────────────

export type RestoreQuote = components['schemas']['QuoteView'];
export type RestoreQuoteOption = components['schemas']['QuoteOption'];
export type Restore = components['schemas']['RestoreView'];

/**
 * What restoring an asset would cost at each tier, and when each would land.
 *
 * Asks for nothing. The distinction matters: this is the endpoint that lets a screen show a price *before*
 * the user commits, which §6.5 requires and which the request endpoint cannot do — it records as it prices.
 */
export async function restoreQuote(assetId: string): Promise<RestoreQuote> {
	return request<RestoreQuote>(`/assets/${assetId}/restore/quote`);
}

/** The most recent restore for an asset, in any state, or `null` if nobody has ever asked. */
export async function currentRestore(assetId: string): Promise<Restore | null> {
	return request<Restore | null>(`/assets/${assetId}/restore`);
}

/**
 * Asks for an asset's original to be brought back.
 *
 * `joined_existing` on the response means somebody else had already asked and this caller is now waiting on
 * the same retrieval — one charge, one ETA, which is what the coalescing in `restore_requests` is for.
 */
export async function requestRestore(assetId: string, tier: string): Promise<Restore> {
	return request<Restore>(`/assets/${assetId}/restore?tier=${encodeURIComponent(tier)}`, {
		method: 'POST'
	});
}

export type LifecyclePolicy = components['schemas']['PolicyView'];
export type LifecyclePlan = components['schemas']['PlanView'];
export type RunQueued = components['schemas']['RunQueued'];

/** The tiering rules, in the order the engine applies them. */
export async function listLifecyclePolicies(): Promise<LifecyclePolicy[]> {
	return request<LifecyclePolicy[]>('/lifecycle/policies');
}

/**
 * What one policy would do, without doing it.
 *
 * A `POST` for a read, because it is a full scan of the tenant's placements and because caching it would be
 * actively wrong: the answer changes as objects age into eligibility.
 */
export async function planLifecyclePolicy(id: string): Promise<LifecyclePlan> {
	return request<LifecyclePlan>(`/lifecycle/policies/${id}/plan`, { method: 'POST' });
}

/** Queues a sweep. Whether anything moves is each policy's own `dry_run` to decide. */
export async function runLifecycleSweep(): Promise<RunQueued> {
	return request<RunQueued>('/lifecycle/runs', { method: 'POST' });
}

// ─── collections (Q.14b) ────────────────────────────────────────────────────

export type Collection = components['schemas']['CollectionView'];
export type CollectionItem = components['schemas']['ItemView'];
export type Added = components['schemas']['AddedView'];

/** Every collection, with its size. Administration, so this needs Manage. */
export async function listCollections(): Promise<Collection[]> {
	return request<Collection[]>('/collections');
}

/**
 * Creates a collection.
 *
 * `key` is what a portal will reference and cannot be changed afterwards, which is why the form asks for it
 * separately from the label rather than deriving one.
 */
export async function createCollection(body: {
	key: string;
	label: string;
	description?: string;
	visibility?: string;
	pin_hot?: boolean;
}): Promise<Collection> {
	return request<Collection>('/collections', { method: 'POST', body: JSON.stringify(body) });
}

/** Changes everything except the key. */
export async function amendCollection(
	id: string,
	body: { label: string; description?: string; visibility: string; pin_hot: boolean }
): Promise<Collection> {
	return request<Collection>(`/collections/${id}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

/** Deletes a collection. Refused with a 409 while a portal publishes it. */
export async function deleteCollection(id: string): Promise<void> {
	await request<void>(`/collections/${id}`, { method: 'DELETE' });
}

export async function collectionItems(id: string): Promise<CollectionItem[]> {
	return request<CollectionItem[]>(`/collections/${id}/items`);
}

/**
 * Adds assets to a collection.
 *
 * `out_of_scope` on the response counts ids the caller cannot see. Shown rather than hidden, because
 * "I selected forty and thirty-eight arrived" needs an answer — and counted rather than named, because
 * naming them would confirm assets exist that this caller may not know about.
 */
export async function addToCollection(id: string, assetIds: string[]): Promise<Added> {
	return request<Added>(`/collections/${id}/items`, {
		method: 'POST',
		body: JSON.stringify({ asset_ids: assetIds })
	});
}

export async function removeFromCollection(id: string, assetId: string): Promise<void> {
	await request<void>(`/collections/${id}/items/${assetId}`, { method: 'DELETE' });
}

/** Moves an asset within a collection, returning the whole new order. */
export async function moveInCollection(
	id: string,
	assetId: string,
	position: number
): Promise<CollectionItem[]> {
	return request<CollectionItem[]>(`/collections/${id}/items/${assetId}/position`, {
		method: 'POST',
		body: JSON.stringify({ position })
	});
}

// ─── worklists (Q.20) ───────────────────────────────────────────────────────

export type Worklist = components['schemas']['WorklistView'];

/**
 * Every worklist with this caller's own count.
 *
 * The counts are per-caller, so two people legitimately see different numbers — a worklist that counted work
 * its reader cannot reach would send them looking for an asset that 404s.
 */
export async function listWorklists(): Promise<Worklist[]> {
	return request<Worklist[]>('/worklists');
}

/** One worklist as a page of assets, in the same shape the grid draws. */
export async function worklistPage(key: string, offset = 0, limit = 60): Promise<AssetPage> {
	return request<AssetPage>(`/worklists/${key}?offset=${offset}&limit=${limit}`);
}

// ─── tag vocabularies (Q.20b) ───────────────────────────────────────────────

export type Vocabulary = components['schemas']['VocabularyView'];
export type VocabularyTerm = components['schemas']['TermView'];

export async function listVocabularies(): Promise<Vocabulary[]> {
	return request<Vocabulary[]>('/vocabularies');
}

export async function createVocabulary(key: string, label: string): Promise<Vocabulary> {
	return request<Vocabulary>('/vocabularies', {
		method: 'POST',
		body: JSON.stringify({ key, label })
	});
}

/**
 * Opens a vocabulary to machine tagging, or closes it.
 *
 * Its own call, not a field on an update: this decides what an LLM is told about the library, and it should
 * not be possible to change it while editing a label.
 */
export async function setVocabularyAi(id: string, aiTaggable: boolean): Promise<Vocabulary> {
	return request<Vocabulary>(`/vocabularies/${id}/ai`, {
		method: 'POST',
		body: JSON.stringify({ ai_taggable: aiTaggable })
	});
}

/** Every term, retired ones included — an administrator has to see what was retired and where it went. */
export async function vocabularyTerms(id: string): Promise<VocabularyTerm[]> {
	return request<VocabularyTerm[]>(`/vocabularies/${id}/terms`);
}

export async function addVocabularyTerm(
	id: string,
	body: { slug: string; label: string; synonyms?: string[]; parent_id?: string }
): Promise<VocabularyTerm> {
	return request<VocabularyTerm>(`/vocabularies/${id}/terms`, {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

/** Changes the label, synonyms and threshold. Never the slug — a model answers with it. */
export async function amendVocabularyTerm(
	id: string,
	termId: string,
	body: { label: string; synonyms: string[]; ai_threshold: number }
): Promise<VocabularyTerm> {
	return request<VocabularyTerm>(`/vocabularies/${id}/terms/${termId}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

/** Retires a term from new assignment. Its assets keep it and its id still resolves. */
export async function retireVocabularyTerm(id: string, termId: string): Promise<VocabularyTerm> {
	return request<VocabularyTerm>(`/vocabularies/${id}/terms/${termId}/retire`, {
		method: 'POST'
	});
}

/** Merges a term into another: the assets move and this one retires pointing at it. */
export async function mergeVocabularyTerm(
	id: string,
	termId: string,
	into: string
): Promise<VocabularyTerm> {
	return request<VocabularyTerm>(`/vocabularies/${id}/terms/${termId}/merge`, {
		method: 'POST',
		body: JSON.stringify({ into })
	});
}

// ─── webhooks (Q.20c) ───────────────────────────────────────────────────────

export type Webhook = components['schemas']['SubscriptionView'];
export type WebhookCreated = components['schemas']['CreatedView'];
export type WebhookDelivery = components['schemas']['DeliveryView'];

export async function listWebhooks(): Promise<Webhook[]> {
	return request<Webhook[]>('/webhooks');
}

/**
 * Registers an endpoint.
 *
 * The response is the only place the signing secret ever appears — a receiver cannot verify a delivery
 * without it, and returning it on every read would put it in the response of an endpoint an integration
 * polls. So the screen has to show it once and say so.
 */
export async function createWebhook(url: string, eventKinds: string[]): Promise<WebhookCreated> {
	return request<WebhookCreated>('/webhooks', {
		method: 'POST',
		body: JSON.stringify({ url, event_kinds: eventKinds })
	});
}

export async function deleteWebhook(id: string): Promise<void> {
	await request<void>(`/webhooks/${id}`, { method: 'DELETE' });
}

/** Re-enables a subscription the system disabled, forgiving its failure count. */
export async function enableWebhook(id: string): Promise<Webhook> {
	return request<Webhook>(`/webhooks/${id}/enable`, { method: 'POST' });
}

/** Recent deliveries, newest first. Carries no payloads. */
export async function webhookDeliveries(id: string): Promise<WebhookDelivery[]> {
	return request<WebhookDelivery[]>(`/webhooks/${id}/deliveries`);
}

/** Re-queues an abandoned delivery. Only a dead one. */
export async function retryWebhookDelivery(id: string, deliveryId: string): Promise<void> {
	await request<void>(`/webhooks/${id}/deliveries/${deliveryId}/retry`, { method: 'POST' });
}

// ─── site branding (Q.20d) ──────────────────────────────────────────────────

export type Branding = components['schemas']['BrandingView'];

/**
 * The tenant's own name, logo and accent.
 *
 * Read, not Manage: the shell renders this on every page, so a curator must be able to see their own library's
 * name. The `site_name` is already resolved — an unset one comes back as the tenant's display name — and
 * `site_name_is_default` says which it was, because a form must not pre-fill a fallback and make it look chosen.
 */
export async function loadBranding(): Promise<Branding> {
	return request<Branding>('/branding');
}

export async function saveBranding(body: {
	site_name: string;
	logo_asset_id?: string | null;
	accent: string;
	support_email?: string | null;
}): Promise<Branding> {
	return request<Branding>('/branding', { method: 'PUT', body: JSON.stringify(body) });
}

// ─── near-duplicates and colour (M4) ────────────────────────────────────────

export type DuplicateCandidate = components['schemas']['CandidateView'];
export type ColourBucket = components['schemas']['ColourBucket'];

/**
 * Open near-duplicate pairs, most alike first.
 *
 * Only pairs whose *both* halves the caller can see — a pair with one visible side would disclose that the
 * other exists. So two people legitimately see different queues.
 */
export async function listDuplicates(): Promise<DuplicateCandidate[]> {
	return request<DuplicateCandidate[]>('/duplicates');
}

/**
 * Records a verdict on a pair.
 *
 * `merged` records a decision and merges nothing — which asset survives, and what happens to the other's
 * rights and references, is not something this endpoint decides.
 */
export async function resolveDuplicate(
	id: string,
	state: 'confirmed' | 'dismissed' | 'merged'
): Promise<void> {
	await request<void>(`/duplicates/${id}`, {
		method: 'POST',
		body: JSON.stringify({ state })
	});
}

/** Colour buckets present in the library, most common first. Counts primary colours only. */
export async function listColours(): Promise<ColourBucket[]> {
	return request<ColourBucket[]>('/colours');
}

// ─── proofing rounds (M6b) ──────────────────────────────────────────────────

export type Round = components['schemas']['RoundView'];
export type Reviewer = components['schemas']['ReviewerView'];
export type RoundAsset = components['schemas']['RoundAssetView'];

/**
 * Rounds this caller can see, newest first.
 *
 * "Can see" is per-round and whole: a round is visible only when *all* of its assets are, so two people
 * legitimately see different lists. That is the same rule the server applies to a single round, which is why a
 * round missing from this list also 404s when addressed directly.
 */
export async function listRounds(limit = 50): Promise<Round[]> {
	return request<Round[]>(`/proofing?limit=${limit}`);
}

/**
 * The rounds waiting on *this* caller's verdict.
 *
 * Not "rounds I can see" and not "rounds I am on" — rounds where my own verdict is still pending. Answering
 * removes a round from here while leaving it open for everybody else who has not.
 */
export async function myRounds(): Promise<Round[]> {
	return request<Round[]>('/proofing/mine');
}

export async function readRound(id: string): Promise<Round> {
	return request<Round>(`/proofing/${id}`);
}

/** The assets a round is about, in the order they were put in. */
export async function roundAssets(id: string): Promise<RoundAsset[]> {
	return request<RoundAsset[]>(`/proofing/${id}/assets`);
}

/**
 * Opens a round.
 *
 * The asset list is snapshotted: a round cannot be widened afterwards, because a reviewer who approved eleven
 * pictures did not approve a twelfth added later. A second pass is a new round with `supersedes` set.
 */
export async function openRound(body: {
	title: string;
	brief?: string;
	asset_ids: string[];
	reviewer_ids: string[];
	due_at?: string | null;
	supersedes?: string | null;
}): Promise<Round> {
	return request<Round>('/proofing', { method: 'POST', body: JSON.stringify(body) });
}

/**
 * Records this caller's verdict, and returns the round's new outcome.
 *
 * `pending` is not offered: it is a starting state, not an answer. The note is a covering remark — anything
 * about a particular picture belongs in a comment on it, where it can be pinned to a region.
 */
export async function decideRound(
	id: string,
	verdict: 'approved' | 'changes_requested',
	note = ''
): Promise<Round> {
	return request<Round>(`/proofing/${id}/verdict`, {
		method: 'POST',
		body: JSON.stringify({ verdict, note })
	});
}

/** Withdraws a round. Verdicts already given are kept and still shown. */
export async function cancelRound(id: string): Promise<Round> {
	return request<Round>(`/proofing/${id}/cancel`, { method: 'POST' });
}

// ─── insights (M6c) ─────────────────────────────────────────────────────────

export type Insights = components['schemas']['Insights'];
export type InsightsDay = components['schemas']['DayView'];
export type InsightsAsset = components['schemas']['AssetCountView'];
export type InsightsClass = components['schemas']['ClassView'];
export type InsightsContributor = components['schemas']['ContributorView'];
export type InsightsReport = components['schemas']['Report'];

/**
 * Everything the Insights screen draws, in one call.
 *
 * Every number is narrowed to what this caller can see, so two people legitimately see different charts — the
 * same rule as the dashboard, and the reason there is no tenant-wide total anywhere on this surface. The
 * response echoes the window it actually used, so a chart can be labelled honestly: a request for ten years
 * comes back as 366 days.
 */
export async function loadInsights(days = 30): Promise<Insights> {
	return request<Insights>(`/insights?days=${days}`);
}

/**
 * One report as a CSV, from the same query the screen used.
 *
 * A blob rather than a URL because the request needs the API key header; a plain link would be an
 * unauthenticated fetch. The caller creates and revokes the object URL, as the search export does.
 */
export async function exportInsights(report: InsightsReport, days = 30): Promise<Blob> {
	return requestBlob(`/insights/export.csv?report=${report}&days=${days}`);
}

// ─── connected sites (M3d) ──────────────────────────────────────────────────

export type Connector = components['schemas']['ConnectorView'];
export type Registered = components['schemas']['RegisteredView'];

export async function listConnectors(): Promise<Connector[]> {
	return request<Connector[]>('/connectors');
}

/**
 * Registers a site.
 *
 * The response carries the API key and the signing secret, and it is the only time either exists in readable
 * form — the key is stored as a hash and the secret encrypted at rest. So the screen has to show them once and
 * say so; a UI that quietly discards them produces a support ticket a week later.
 */
export async function registerConnector(body: {
	kind: string;
	label: string;
	site_url: string;
	asset_group_ids?: string[];
	all_asset_groups?: boolean;
	allow_original?: boolean;
	allow_restore?: boolean;
}): Promise<Registered> {
	return request<Registered>('/connectors', { method: 'POST', body: JSON.stringify(body) });
}

/**
 * Replaces the signing secret.
 *
 * `keepPrevious` is not a default anywhere, because the two situations want opposite answers: a scheduled
 * rotation keeps the old secret verifying for a week while the site's own deploy lands, and a leak must stop it
 * now. The screen asks.
 */
export async function rotateConnector(id: string, keepPrevious: boolean): Promise<Registered> {
	return request<Registered>(`/connectors/${id}/rotate`, {
		method: 'POST',
		body: JSON.stringify({ keep_previous: keepPrevious })
	});
}

/** Pauses, resumes, or revokes. Revoking is terminal and clears both secrets. */
export async function setConnectorStatus(
	id: string,
	status: 'active' | 'paused' | 'revoked'
): Promise<Connector> {
	return request<Connector>(`/connectors/${id}/status`, {
		method: 'POST',
		body: JSON.stringify({ status })
	});
}

// ─── the usage index (M3d·4) ────────────────────────────────────────────────

export type AssetReference = components['schemas']['ReferenceView'];
export type ReferenceImpact = components['schemas']['ImpactView'];

/**
 * Where an asset is used on connected sites.
 *
 * The three counts describe only what is *live* — linked, in use, on an active site, reported recently — while
 * the list carries every reference including the dead ones. That asymmetry is the point: the counts are what
 * pulling the asset would break, and the list is what explains them, including "one site stopped reporting
 * three weeks ago".
 *
 * `pages` is the softest of the three because it is the site's own count. Named separately rather than folded
 * into a total, so a screen can say so.
 */
export async function assetReferences(assetId: string): Promise<ReferenceImpact> {
	return request<ReferenceImpact>(`/assets/${assetId}/references`);
}

/** Every reference one connected site holds, most-used first. */
export async function connectorReferences(id: string, limit = 100): Promise<AssetReference[]> {
	return request<AssetReference[]>(`/connectors/${id}/refs?limit=${limit}`);
}

// ─── caps (G19) ─────────────────────────────────────────────────────────────

export type Quota = components['schemas']['QuotaView'];
export type Quotas = components['schemas']['QuotasView'];

/**
 * Every configured cap and where this tenant stands against it.
 *
 * Only *configured* caps come back. An absent one is not a cap of zero — the server allows work against a key
 * with no row — so listing them all at zero would contradict the enforcement.
 *
 * `is_level` is the field a screen must not ignore: "1.2 TB" means what exists for `storage_bytes` and what
 * happened this month for a flow, and the same bar drawn for both would be misleading about the more alarming
 * one.
 *
 * There is deliberately no setter. A tenant raising its own limit is not a feature; that is an operator's
 * `damctl` command.
 */
export async function loadQuotas(): Promise<Quotas> {
	return request<Quotas>('/quotas');
}

// ─── governance (G10) ───────────────────────────────────────────────────────

export type AuditEntry = components['schemas']['EntryView'];
export type AuditPage = components['schemas']['PageView'];
export type AuditVerification = components['schemas']['VerificationView'];
export type AuditExtract = components['schemas']['ExtractView'];
export type LegalHoldResult = components['schemas']['LegalHoldView'];

/**
 * Places or lifts a legal hold.
 *
 * `changed` is `false` when the asset was already in the asked-for state, and in that case nothing was
 * recorded. A screen that reported "hold placed" either way would be claiming an audit entry that does not
 * exist.
 *
 * The reason is required in both directions. Lifting needs it more than placing does: "somebody lifted the
 * litigation hold" with no sentence attached is the row that makes an auditor distrust the rest of the log.
 */
export async function setLegalHold(
	assetId: string,
	held: boolean,
	reason: string
): Promise<LegalHoldResult> {
	return request<LegalHoldResult>(`/assets/${assetId}/legal-hold`, {
		method: 'PUT',
		body: JSON.stringify({ held, reason })
	});
}

/**
 * Reads the governance record, newest first.
 *
 * Not filtered by the caller's access predicate, because the log is not asset-scoped — which is why the
 * endpoint takes the strongest gate the role model has. A caller who cannot manage gets a 403 rather than an
 * empty list.
 */
export async function auditLog(
	params: {
		action?: string;
		target_kind?: string;
		target_id?: string;
		before_seq?: number;
	} = {}
): Promise<AuditPage> {
	const query = new URLSearchParams();
	for (const [key, value] of Object.entries(params)) {
		if (value !== undefined && value !== '') query.set(key, String(value));
	}
	const suffix = query.size > 0 ? `?${query}` : '';
	return request<AuditPage>(`/audit${suffix}`);
}

/**
 * Walks the hash chain and reports the first inconsistency.
 *
 * A broken chain comes back as a 200 with `intact: false`. That is deliberate on the server's part and the
 * screen has to honour it: rendering an error state would say "we could not check", which is the opposite of
 * what happened.
 */
export async function verifyAudit(): Promise<AuditVerification> {
	return request<AuditVerification>('/audit/verify');
}

/**
 * Takes a re-verifiable extract, and records that it was taken.
 *
 * A POST because it writes: the `audit.exported` entry says a copy was taken, and behind GET that entry would
 * be written by every link preview and uptime probe.
 */
export async function exportAudit(fromSeq = 0): Promise<AuditExtract> {
	return request<AuditExtract>(`/audit/export?from_seq=${fromSeq}`, { method: 'POST' });
}

// ─── members (G10·2a) ───────────────────────────────────────────────────────

export type Member = components['schemas']['MemberView'];
export type MemberAdded = components['schemas']['MemberInvitedView'];
export type MemberRemoved = components['schemas']['MemberRemovedView'];

/**
 * Everyone with access to this tenant.
 *
 * Not the same list as `people()`, which answers "who can I mention" and is readable by anybody writing a
 * comment. This one carries roles and status and takes the administration gate.
 */
export async function listMembers(): Promise<Member[]> {
	return request<Member[]>('/members');
}

/** The role keys this tenant defines, so a form can offer them rather than ask for a string. */
export async function listRoles(): Promise<string[]> {
	return request<string[]>('/roles');
}

/**
 * Gives somebody access, and returns their credential once.
 *
 * There is no login flow — the application authenticates with an API key — so the key *is* the invitation and
 * cannot be read back afterwards. A screen that discards it has locked somebody out before they arrived.
 *
 * A 409 means they are already a member of *this* tenant. It deliberately says nothing about whether the
 * address exists elsewhere in the deployment.
 */
export async function addMember(body: {
	email: string;
	display_name?: string | null;
	role_names: string[];
	is_tenant_admin: boolean;
}): Promise<MemberAdded> {
	return request<MemberAdded>('/members', { method: 'POST', body: JSON.stringify(body) });
}

/**
 * Replaces somebody's roles and administrator flag.
 *
 * The complete set, not a patch: it is what the screen shows, and a partial update of an array is two round
 * trips racing each other. A 409 is either the tenant's only administrator or an account the identity provider
 * owns; the message says which.
 */
export async function updateMember(
	identityId: string,
	body: { role_names: string[]; is_tenant_admin: boolean }
): Promise<Member> {
	return request<Member>(`/members/${identityId}`, {
		method: 'PATCH',
		body: JSON.stringify(body)
	});
}

/**
 * Takes away somebody's access, and their credentials with it.
 *
 * `keys_revoked` is the number worth showing: an account marked removed that keeps working is a flag rather
 * than a removal, and the count is how a person can see the difference.
 */
export async function removeMember(identityId: string): Promise<MemberRemoved> {
	return request<MemberRemoved>(`/members/${identityId}`, { method: 'DELETE' });
}

// ─── SCIM provisioning (G10·2b) ─────────────────────────────────────────────

export type ScimClient = components['schemas']['ClientView'];
export type ScimRegistered = components['schemas']['RegisteredClient'];

/**
 * The identity providers wired up to this tenant.
 *
 * `last_sync_status` is what makes a stalled integration visible: a provider that has stopped calling looks
 * exactly like one that never started unless something records the contact. `null` means it has never called.
 */
export async function listScimClients(): Promise<ScimClient[]> {
	return request<ScimClient[]>('/scim/clients');
}

/**
 * Registers a provider and returns its token once.
 *
 * Stored as a hash, so it cannot be read back. Deliberately not reachable with a provisioning token: one that
 * could mint another would be a credential that cannot be revoked.
 */
export async function registerScimClient(body: {
	label: string;
	scopes: string[];
}): Promise<ScimRegistered> {
	return request<ScimRegistered>('/scim/clients', {
		method: 'POST',
		body: JSON.stringify(body)
	});
}

/** Revokes a provider. Terminal — a leaked provisioning token can create and remove accounts. */
export async function revokeScimClient(id: string): Promise<void> {
	await request<unknown>(`/scim/clients/${id}/revoke`, { method: 'POST' });
}
