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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(body)
	});
}

export async function amendUploadProfile(
	id: string,
	body: Record<string, unknown>
): Promise<UploadProfile> {
	return request<UploadProfile>(`/upload-profiles/${id}`, {
		method: 'PATCH',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(body)
	});
}

export async function removeUploadProfile(id: string): Promise<void> {
	await request<void>(`/upload-profiles/${id}`, { method: 'DELETE' });
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
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(body)
	});
}

export async function amendAutoImportMapping(
	id: string,
	body: { overwrite?: boolean; enabled?: boolean }
): Promise<AutoImportMapping> {
	return request<AutoImportMapping>(`/auto-import-mappings/${id}`, {
		method: 'PATCH',
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(body)
	});
}

export async function createCategory(
	treeId: string,
	body: { slug: string; label: string; parent_id?: string | null }
): Promise<CategoryNode> {
	return request<CategoryNode>(`/categories/${treeId}/nodes`, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
		headers: { 'Content-Type': 'application/json' },
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
