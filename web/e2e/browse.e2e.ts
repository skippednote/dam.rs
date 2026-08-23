/**
 * The asset browser, through a real browser, against a mocked API.
 *
 * The API is mocked with `page.route` rather than a running `damd`, deliberately. The Rust gate already
 * proves the endpoints against a real Postgres — 16 HTTP cases for the asset routes alone — and the wire
 * shapes come from the same `openapi.json` both sides are generated from, so a divergence is a build error
 * rather than something an integration test would catch. What is *only* testable here is the browser half:
 * that a facet click rewrites the query and re-requests, that a 422 lands next to the field it names, that
 * the grid reports the collection's row count rather than its rendered one, and that none of it has an axe
 * violation. Keeping the web gate free of Docker is also what keeps it fast enough to run on every push.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

const TIERS = ['hot', 'cool', 'archive', 'restoring', 'restored'] as const;
const RIGHTS = ['allowed', 'expiring', 'denied', 'unknown'] as const;
const PROVENANCE = ['valid', 'none', 'invalid', 'untrusted'] as const;

/** A 1x1 WebP, so an `<img>` in the grid has real bytes to decode. */
const WEBP_1PX = 'UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==';

function summary(index: number) {
	return {
		id: `00000000-0000-4000-8000-${String(index).padStart(12, '0')}`,
		filename: `campaign-${String(index).padStart(4, '0')}.jpg`,
		mime: 'image/jpeg',
		bytes: 2_400_000 + index * 1024,
		width: 4000,
		height: 3000,
		tier: TIERS[index % TIERS.length],
		rights_state: RIGHTS[index % RIGHTS.length],
		provenance_state: PROVENANCE[index % PROVENANCE.length],
		// Two shapes, because the server emits both: absolute when `server.public_url` is configured, and
		// root-relative otherwise — and a root-relative URL resolved against the *frontend's* origin is the bug
		// this covers. Index 2 onwards has none at all, which is the normal state between an upload finishing
		// and the worker deriving it.
		thumbnail_url:
			index === 0
				? 'http://127.0.0.1:8099/d/absolute-token'
				: index === 1
					? '/d/relative-token'
					: null,
		tag_confidence: index % 3 === 0 ? null : 0.55 + (index % 40) / 100,
		// Engagement, as every summary now carries it (Q.5c).
		is_favourite: false,
		average_stars: null,
		// Paperwork flag, as every summary now carries it (Q.9).
		has_attachment: false,
		// Published, on every third asset (Q.14): the chip has to be visibly a per-asset fact rather than a
		// property of the page.
		published_at: index % 3 === 0 ? '2026-08-01T09:00:00Z' : null
	};
}

const FACETS = [
	{
		key: 'brand',
		truncated: true,
		buckets: [
			{ value: 'acme', id: null, count: 74 },
			{ value: 'Acme Corp', id: null, count: 12 }
		]
	},
	{
		key: 'colours',
		truncated: false,
		buckets: [{ value: 'blue', id: null, count: 31 }]
	},
	// The built-ins (Q.15). Their keys are query selectors rather than field keys, which is why the rail has
	// to name them: `has: attachment (9)` under a heading reading "Has" is a debug dump, not a filter.
	{
		key: 'status',
		truncated: false,
		buckets: [
			{ value: 'active', id: null, count: 120 },
			{ value: 'archived', id: null, count: 8 }
		]
	},
	{
		key: 'orientation',
		truncated: false,
		buckets: [
			{ value: 'landscape', id: null, count: 88 },
			{ value: 'portrait', id: null, count: 40 }
		]
	},
	{
		key: 'stars',
		truncated: false,
		buckets: [
			{ value: '5', id: null, count: 4 },
			{ value: '1', id: null, count: 1 }
		]
	},
	{
		key: 'has',
		truncated: false,
		buckets: [{ value: 'attachment', id: null, count: 9 }]
	}
];

/**
 * The tenant's definitions.
 *
 * `colours` is multivalued and `ingested_at` is read-only on purpose: those two flags are what the editor
 * cannot guess, and each has its own case below.
 */
const FIELDS = [
	{
		key: 'brand',
		label: 'Brand',
		kind: 'text',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: true,
		search_alias: 'bra',
		taxonomy_id: null
	},
	{
		key: 'caption',
		label: 'Caption',
		kind: 'text',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: true,
		facetable: false,
		search_alias: 'cap',
		taxonomy_id: null
	},
	{
		key: 'campaign',
		label: 'Campaign',
		kind: 'text',
		multivalued: false,
		required: true,
		read_only: false,
		ai_writable: false,
		facetable: false,
		search_alias: null,
		taxonomy_id: null
	},
	{
		key: 'colours',
		label: 'Colours',
		kind: 'text',
		multivalued: true,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: true,
		search_alias: 'col',
		taxonomy_id: null
	},
	// Q.19b. A dependent field and the parent it hangs off: the release reference applies only to a photograph
	// with people in it.
	{
		key: 'has_people',
		label: 'Has people',
		kind: 'bool',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: true,
		search_alias: null,
		taxonomy_id: null
	},
	{
		key: 'release_reference',
		label: 'Release reference',
		kind: 'text',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: false,
		search_alias: null,
		taxonomy_id: null,
		depends_on: { key: 'has_people', values: ['true'] }
	},
	{
		key: 'ingested_at',
		label: 'Ingested at',
		kind: 'datetime',
		multivalued: false,
		required: false,
		read_only: true,
		ai_writable: false,
		facetable: false,
		search_alias: null,
		taxonomy_id: null
	}
];

/** Requests the page made, so a test can assert what the server was actually asked. */
type Recorder = {
	urls: string[];
	patches: Record<string, unknown>[];
	bulk: {
		url: string;
		body: {
			kind: string;
			asset_ids: string[];
		};
	}[];
	/** What the Order action sent: how many assets, and the reason. */
	ordered: string[];
	/** What the "Add to collection" action sent, as `collection-key:count`. */
	added: string[];
};
let bulkPolls = 0;

async function connect(page: Page): Promise<Recorder> {
	const recorder: Recorder = { urls: [], patches: [], bulk: [], ordered: [], added: [] };
	bulkPolls = 0;

	// Before any navigation: the session reads `localStorage` at module load, so setting it afterwards
	// would leave the first render unconnected and the page would show the Settings prompt.
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = route.request().url();
		recorder.urls.push(url);
		const path = new URL(url);

		if (path.pathname === '/assets') {
			const total = 120;
			const limit = Number(path.searchParams.get('limit') ?? 60);
			return route.fulfill({
				json: {
					items: Array.from({ length: Math.min(limit, total) }, (_, i) => summary(i)),
					total,
					offset: 0
				}
			});
		}
		if (path.pathname === '/search') {
			// A ranked result is smaller than the library, which is what makes the "capped" note meaningful.
			return route.fulfill({
				json: { items: [summary(0), summary(1)], total: 2, offset: 0 }
			});
		}
		if (path.pathname === '/search/facets') {
			return route.fulfill({ json: FACETS });
		}
		if (path.pathname === '/search/export.csv') {
			// Q.18. A file, not JSON — and the query travels, so a test can assert the export describes the
			// search rather than the library.
			recorder.urls.push(url);
			const q = path.searchParams.get('q') ?? '';
			if (q === 'brand:everything') {
				return route.fulfill({
					status: 422,
					contentType: 'application/json',
					body: JSON.stringify({
						message:
							'that search matches 40000 assets and an export carries 10000; narrow the query'
					})
				});
			}
			return route.fulfill({
				contentType: 'text/csv; charset=utf-8',
				headers: { 'content-disposition': 'attachment; filename="search-results.csv"' },
				body: `filename,mime,bytes,width,height,brand\ncampaign-0000.jpg,image/jpeg,2400000,4000,3000,acme\n`
			});
		}
		if (path.pathname === '/search/suggest') {
			// Q.17. The server sends the fragment to insert; the client's job is to put a string in a box.
			const typed = path.searchParams.get('typed') ?? '';
			recorder.urls.push(url);
			const all = [
				{ source: 'field', label: 'acme', within: 'brand', fragment: 'brand:acme', count: 74 },
				{
					source: 'field',
					label: 'Acme Corp',
					within: 'brand',
					fragment: 'brand:"Acme Corp"',
					count: 12
				},
				{
					source: 'filename',
					label: 'acme-logo.png',
					within: null,
					fragment: 'filename:acme-logo.png',
					count: 1
				}
			];
			return route.fulfill({
				json:
					typed.length < 2
						? []
						: all.filter((one) => one.label.toLowerCase().startsWith(typed.toLowerCase()))
			});
		}
		if (path.pathname === '/fields') {
			return route.fulfill({ json: FIELDS });
		}
		if (path.pathname.endsWith('/usage-options') || path.pathname === '/usage-options') {
			// Q.12's intended-use vocabulary, asked alongside the formats. Empty here: a suite about
			// something else should not grow a form it never meant to test.
			return route.fulfill({
				json: {
					channels: [],
					territories: [],
					default_channel: 'internal',
					default_territory: 'WORLD'
				}
			});
		}
		if (path.pathname === '/collections' && route.request().method() === 'GET') {
			return route.fulfill({
				json: [
					{
						id: 'c-press',
						key: 'press-kit',
						label: 'Press kit',
						description: null,
						visibility: 'shared',
						pin_hot: false,
						item_count: 4
					}
				]
			});
		}
		if (path.pathname.endsWith('/items') && route.request().method() === 'POST') {
			const body = route.request().postDataJSON() as { asset_ids: string[] };
			recorder.added.push(`press-kit:${body.asset_ids.length}`);
			// One of the two is outside this caller's scope, which is the case the counted reply exists for —
			// and the reason it is counted rather than named.
			return route.fulfill({ json: { added: body.asset_ids.length - 1, out_of_scope: 1 } });
		}
		if (path.pathname === '/orders' && route.request().method() === 'POST') {
			const body = route.request().postDataJSON() as { asset_ids: string[]; purpose: string };
			recorder.ordered.push(`${body.asset_ids.length}:${body.purpose}`);
			// One fewer item than asked for, which is what the server does when part of a selection is outside the
			// requester's scope — and the case the count in the reply exists for.
			return route.fulfill({
				status: 201,
				json: {
					id: 'o-1',
					reference: 'ORD-000009',
					requested_by: { id: 'p-ada', name: 'Ada', email: 'a@x' },
					purpose: body.purpose,
					channel: null,
					territory: null,
					conversion_key: null,
					include_metadata: false,
					recipients: [],
					state: 'submitted',
					expired: false,
					decided_by: null,
					decided_at: null,
					decision_note: null,
					self_approved: false,
					expires_at: null,
					created_at: '2026-08-01T09:00:00Z',
					items: body.asset_ids.slice(0, -1).map((id) => ({ asset_id: id, filename: `${id}.jpg` }))
				}
			});
		}
		if (path.pathname.endsWith('/download-options')) {
			// Q.11's download panel asks this on every asset the detail view opens. Answered so a suite about
			// something else does not render an error banner it never intended to test.
			return route.fulfill({
				json: { original_available: true, media_class: 'image', conversions: [] }
			});
		}
		if (path.pathname.startsWith('/assets/') && path.pathname.endsWith('/metadata')) {
			const body = route.request().postDataJSON() as { values: Record<string, unknown> };
			recorder.patches.push(body.values);
			if ('campaign' in body.values && body.values.campaign === null) {
				return route.fulfill({
					status: 422,
					json: [{ key: 'campaign', code: 'required', detail: 'campaign is required' }]
				});
			}
			return route.fulfill({ json: { values: { caption: 'edited', ...body.values } } });
		}
		if (path.pathname.startsWith('/assets/')) {
			return route.fulfill({
				json: {
					...summary(0),
					values: {
						brand: 'acme',
						caption: 'a harbour at dawn',
						campaign: 'spring',
						colours: ['blue', 'red'],
						ingested_at: '2026-08-01T09:00:00Z'
					},
					technical: { camera: 'X-T5', iso: 200 },
					duration_ms: null,
					page_count: null,
					color_space: 'sRGB',
					has_alpha: false,
					content_hash: 'a'.repeat(64),
					status: 'active',
					enrichment_state: 'done',
					legal_hold: false,
					release_at: null,
					expires_at: null,
					version_no: 1,
					created_at: '2026-08-01T09:00:00Z',
					updated_at: '2026-08-01T09:00:00Z',
					// The full engagement, as the detail payload now carries it (Q.5c). Present rather than
					// omitted because the API always sends it, and a mock that lags the contract tests a shape
					// the server never produces.
					engagement: {
						asset_id: '00000000-0000-4000-8000-000000000000',
						average_stars: null,
						rating_count: 0,
						favourite_count: 0,
						my_stars: null,
						is_favourite: false,
						is_watched: false
					},
					preview_url: 'http://127.0.0.1:8099/d/preview-token'
				}
			});
		}
		if (path.pathname.startsWith('/d/')) {
			return route.fulfill({
				contentType: 'image/webp',
				body: Buffer.from(WEBP_1PX, 'base64')
			});
		}
		if (path.pathname === '/bulk/preview') {
			const body = route.request().postDataJSON() as { kind: string; asset_ids: string[] };
			recorder.bulk.push({ url: url, body });
			// One id is "out of scope", so the dialog's honesty about the difference is testable.
			const inScope = Math.max(body.asset_ids.length - 1, 1);
			return route.fulfill({
				json: {
					kind: body.kind,
					target_count: inScope,
					sample: body.asset_ids.slice(0, inScope),
					out_of_scope: body.asset_ids.length - inScope
				}
			});
		}
		if (path.pathname === '/bulk') {
			const body = route.request().postDataJSON() as { kind: string; asset_ids: string[] };
			recorder.bulk.push({ url: url, body });
			return route.fulfill({
				status: 202,
				json: {
					id: '00000000-0000-4000-8000-00000000b01c',
					kind: body.kind,
					state: 'queued',
					target_count: Math.max(body.asset_ids.length - 1, 1),
					done_count: 0,
					failed_count: 0,
					terminal: false,
					failures: []
				}
			});
		}
		if (path.pathname.startsWith('/bulk/')) {
			// First poll: running. Second: partial, with a named failure — the state a UI must not show as a
			// green tick.
			bulkPolls += 1;
			const finished = bulkPolls >= 2;
			return route.fulfill({
				json: {
					id: path.pathname.split('/').pop(),
					kind: 'delete',
					state: finished ? 'partial' : 'running',
					target_count: 2,
					done_count: finished ? 1 : 0,
					failed_count: finished ? 1 : 0,
					terminal: finished,
					failures: finished
						? [
								{
									asset_id: '00000000-0000-4000-8000-000000000001',
									reason: 'legal hold blocks deletion'
								}
							]
						: []
				}
			});
		}
		if (path.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the grid reports the collection row count, not the rendered one', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/assets');

	const grid = page.getByRole('grid');
	// 60 rows requested of a 120-asset library, 4 columns. The grid divides the *total* — a grid that
	// counted its rendered rows would announce a hundred-thousand-asset library as twenty items, with no
	// visual symptom at all.
	await expect(grid).toHaveAttribute('aria-rowcount', '30');
	await expect(grid).toHaveAttribute('aria-colcount', '4');
	expect(recorder.urls.some((url) => url.includes('/assets?'))).toBe(true);
});

test('an empty query lists and a typed query searches', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/assets');
	await expect(page.getByRole('grid')).toBeVisible();
	expect(recorder.urls.filter((url) => url.includes('/search?'))).toHaveLength(0);

	await page.getByLabel('Search assets').fill('harbour');
	await page.getByRole('button', { name: 'Search' }).click();

	// Two different endpoints, and the difference is stated to the user because one is exhaustive and the
	// other is a bounded ranking.
	await expect(page.getByText(/ranked by relevance/)).toBeVisible();
	expect(recorder.urls.some((url) => url.includes('/search?q=harbour'))).toBe(true);
});

test('a facet click writes the query and re-requests', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/assets');

	await page.getByRole('checkbox', { name: 'acme 74' }).check();
	await expect(page.getByLabel('Search assets')).toHaveValue('brand:acme');
	expect(recorder.urls.some((url) => url.includes('q=brand%3Aacme'))).toBe(true);

	// Unticking restores the previous query rather than clearing everything.
	await page.getByRole('checkbox', { name: 'acme 74' }).uncheck();
	await expect(page.getByLabel('Search assets')).toHaveValue('');
});

test('a facet value with a space is quoted, or it would become two terms', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');

	await page.getByRole('checkbox', { name: 'Acme Corp 12' }).check();
	// `brand:Acme Corp` parses as a brand filter plus the free text "Corp": the wrong assets, and it looks
	// like a search bug rather than a quoting one.
	await expect(page.getByLabel('Search assets')).toHaveValue('brand:"Acme Corp"');
});

test('the built-in facets are named and their buckets filter', async ({ page }) => {
	// Q.15. The server sends keys that double as query selectors — `stars`, `has` — because the rail writes
	// the string it reads. Neither is a heading anybody would write, so both are named here, and a rating
	// bucket says "5 stars" rather than sitting as a bare number beside its count.
	const recorder = await connect(page);
	await page.goto('/assets');

	for (const heading of ['Status', 'Orientation', 'Rating', 'Attachments']) {
		await expect(page.getByRole('group', { name: heading })).toBeVisible();
	}
	await expect(page.getByRole('checkbox', { name: 'Archived 8' })).toBeVisible();
	await expect(page.getByRole('checkbox', { name: 'Landscape 88' })).toBeVisible();
	await expect(page.getByRole('checkbox', { name: '5 stars 4' })).toBeVisible();
	// Singular for one, because "1 stars" is the kind of detail that makes a page feel unfinished.
	await expect(page.getByRole('checkbox', { name: '1 star 1' })).toBeVisible();
	await expect(page.getByRole('checkbox', { name: 'Has attachments 9' })).toBeVisible();

	// And the query each writes is the one the parser reads, not the label.
	await page.getByRole('checkbox', { name: 'Has attachments 9' }).check();
	await expect(page.getByLabel('Search assets')).toHaveValue('has:attachment');
	expect(recorder.urls.some((url) => url.includes('q=has%3Aattachment'))).toBe(true);

	await page.getByRole('checkbox', { name: '5 stars 4' }).check();
	await expect(page.getByLabel('Search assets')).toHaveValue('has:attachment stars:5');
});

test('the advanced form writes the query and can narrow it', async ({ page }) => {
	// Q.16. The form is not a second way to search: it composes the same string the box holds, so what the
	// user sees is still what the server got.
	const recorder = await connect(page);
	await page.goto('/assets');

	await page.getByRole('button', { name: 'Advanced' }).click();
	await expect(page.getByRole('heading', { name: 'Advanced search' })).toBeVisible();

	// The first row defaults to a filename substring, which is the case this slice exists for: `0043` is not a
	// token the index holds, and it is what somebody reading a filename off a delivery note has.
	await page.getByLabel('Value').fill('0043');
	await expect(page.getByText('filename:*0043*')).toBeVisible();

	// A second condition, joined. A timestamp is offered a range and not a wildcard: the server refuses
	// `ingested_at:202*`, so the form does not suggest it.
	await page.getByRole('button', { name: 'Add a condition' }).click();
	await page.locator('#field-1').selectOption('ingested_at');
	await expect(page.locator('#operator-1')).not.toContainText('contains');
	await page.locator('#operator-1').selectOption('at_least');
	await page.locator('#value-1').fill('2024-01-01');
	await expect(page.getByText('filename:*0043* AND ingested_at:>=2024-01-01')).toBeVisible();

	await page.getByRole('button', { name: 'Search', exact: true }).last().click();
	await expect(page.getByLabel('Search assets')).toHaveValue(
		'filename:*0043* AND ingested_at:>=2024-01-01'
	);
	expect(recorder.urls.some((url) => url.includes('filename%3A*0043*'))).toBe(true);

	// And "search within results" ANDs onto what the box already holds rather than replacing it.
	await page.getByRole('button', { name: 'Advanced' }).click();
	await page.getByLabel('Many assets at once').fill('a.jpg, b.jpg');
	await page.getByRole('button', { name: 'Search within results' }).click();
	await expect(page.getByLabel('Search assets')).toHaveValue(
		'filename:*0043* AND ingested_at:>=2024-01-01 AND (filename:a.jpg OR filename:b.jpg)'
	);
});

test('a pasted filename with a hyphen is not quoted', async ({ page }) => {
	// Found by driving the real stack: `filename:"sample-003.jpg"` is correct and reads like an escape somebody
	// should worry about, in the one box on the page whose job is to be readable.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('button', { name: 'Advanced' }).click();
	await page.getByLabel('Many assets at once').fill('sample-003.jpg, sample-004.jpg');
	await expect(
		page.getByText('(filename:sample-003.jpg OR filename:sample-004.jpg)')
	).toBeVisible();
});

test('the advanced form has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('button', { name: 'Advanced' }).click();
	await expect(page.getByRole('heading', { name: 'Advanced search' })).toBeVisible();
	const results = await new AxeBuilder({ page }).analyze();
	expect(results.violations).toEqual([]);
});

test('the type-ahead completes a word with the keyboard alone', async ({ page }) => {
	// Q.17. The box is where somebody's hands already are; reaching for a pointer to accept a completion is
	// slower than finishing the word, so the whole interaction has to work from the keyboard.
	await connect(page);
	await page.goto('/assets');

	const box = page.getByLabel('Search assets');
	await box.fill('acm');
	const list = page.getByRole('listbox', { name: 'Suggestions' });
	await expect(list).toBeVisible();
	await expect(box).toHaveAttribute('aria-expanded', 'true');
	await expect(page.getByRole('option', { name: /acme/ }).first()).toBeVisible();

	// Arrow keys move the active option without moving focus out of the box — the caret stays where the
	// user is typing.
	await box.press('ArrowDown');
	await expect(box).toHaveAttribute('aria-activedescendant', 'suggestion-0');
	await expect(box).toBeFocused();
	await box.press('Enter');
	// The whole token is replaced rather than appended: `acm` plus `brand:acme` would be two clauses.
	await expect(box).toHaveValue('brand:acme');
	await expect(list).toBeHidden();
});

test('a suggestion with a space arrives already quoted', async ({ page }) => {
	// The fragment comes from the server verbatim. A client that assembled `brand:Acme Corp` itself would
	// produce a brand filter plus the free text "Corp" — a query that changes when it is clicked.
	await connect(page);
	await page.goto('/assets');
	await page.getByLabel('Search assets').fill('Acme');
	await page.getByRole('option', { name: /Acme Corp/ }).click();
	await expect(page.getByLabel('Search assets')).toHaveValue('brand:"Acme Corp"');
});

test('one character offers nothing', async ({ page }) => {
	// Every value in the library, ordered by count, at the cost of three queries per keystroke.
	await connect(page);
	await page.goto('/assets');
	await page.getByLabel('Search assets').fill('a');
	await expect(page.getByRole('listbox', { name: 'Suggestions' })).toBeHidden();
});

test('a misspelled field offers the name it meant, and applies it in place', async ({ page }) => {
	// The refusal is still a refusal: the fix is offered, not applied. Answering `brnad:acme` as `brand:acme`
	// would be a filter nobody asked for.
	const recorder = await connect(page);
	// A predicate rather than a glob: the query string is what distinguishes this request, and a glob over a
	// `?` is matching a wildcard against the thing that separates the path from the part that matters.
	await page.route(
		(url) => url.pathname === '/search' && (url.searchParams.get('q') ?? '').startsWith('brnad'),
		(route) =>
			route.fulfill({
				status: 400,
				json: {
					message: 'no field or alias named "brnad"',
					code: 'unknown_field',
					at: 1,
					suggestion: 'brand'
				}
			})
	);
	await page.goto('/assets?q=brnad:acme');

	// Filtered, because the page carries more than one alert region and the parse refusal is the one this
	// case is about.
	await expect(
		page.getByRole('alert').filter({ hasText: 'no field or alias named' })
	).toBeVisible();
	await page.getByRole('button', { name: /Did you mean/ }).click();
	// The key is replaced and the value is kept: the column the parser gave says which half was wrong.
	await expect(page.getByLabel('Search assets')).toHaveValue('brand:acme');
	expect(recorder.urls.some((url) => url.includes('q=brand%3Aacme'))).toBe(true);
});

test('an empty page does not describe the ordering of nothing', async ({ page }) => {
	// "0 assets · ranked by relevance, capped at the first 1,000" is a sentence about the ordering of an empty
	// list, and beside a did-you-mean it sits between the count and the one thing worth clicking.
	await connect(page);
	await page.route(
		(url) => url.pathname === '/search' && url.searchParams.get('q') === 'brand:nothing',
		(route) => route.fulfill({ json: { items: [], total: 0, offset: 0, ranked: true } })
	);
	await page.goto('/assets?q=brand:nothing');
	await expect(page.getByText('No assets match this search.')).toBeVisible();
	await expect(page.getByText(/ranked by relevance/)).toBeHidden();
});

test('an empty page offers a query that would work', async ({ page }) => {
	await connect(page);
	await page.route(
		(url) => url.pathname === '/search' && url.searchParams.get('q') === 'brand:acmee',
		(route) =>
			route.fulfill({
				json: { items: [], total: 0, offset: 0, ranked: true, did_you_mean: 'brand:acme' }
			})
	);
	await page.goto('/assets?q=brand:acmee');

	// The button is the assertion: "0 assets" appears twice on the page — once for the eye and once for a
	// screen reader — and the offer beside it is what this case is about.
	await page.getByRole('button', { name: /Did you mean/ }).click();
	await expect(page.getByLabel('Search assets')).toHaveValue('brand:acme');
});

test('the type-ahead has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByLabel('Search assets').fill('acm');
	await expect(page.getByRole('listbox', { name: 'Suggestions' })).toBeVisible();
	const results = await new AxeBuilder({ page }).analyze();
	expect(results.violations).toEqual([]);
});

test('a truncated facet says so', async ({ page }) => {
	// A rail that silently cuts off makes "no other brands" and "ninety other brands" look identical, and a
	// user filters on the wrong assumption.
	await connect(page);
	await page.goto('/assets');
	await expect(page.getByText(/Showing the 2 most common/)).toBeVisible();
});

test('opening an asset shows its detail and its tier decides what the panel says', async ({
	page
}) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	const panel = page.getByRole('complementary', { name: 'Selected asset' });
	await expect(panel).toBeVisible();
	await expect(panel.getByText('campaign-0000.jpg')).toBeVisible();
	// Asset 0 is `hot`, so the original is available — and the panel still says the rights check happens at
	// delivery, because a badge is not a permission.
	await expect(panel.getByText(/The original is available/)).toBeVisible();
	await expect(panel.getByText(/rights check at the point of delivery/)).toBeVisible();
});

test('a rejected field shows its error next to that field', async ({ page }) => {
	// The property axe cannot see and a screen-reader user depends on: an error in a banner at the top of a
	// twenty-field form is one they meet after leaving the field that caused it.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	// Scoped to the panel: `getByLabel` matches substrings, and the grid's favourite stars are labelled with
	// their filename — `campaign-0000.jpg` — so an unscoped 'Campaign' now matches sixty buttons as well as the
	// field. The locator was always loose; the star made it ambiguous.
	const campaign = page
		.getByRole('complementary', { name: 'Selected asset' })
		.getByLabel('Campaign');
	await campaign.fill('');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(campaign).toHaveAttribute('aria-invalid', 'true');
	const describedBy = await campaign.getAttribute('aria-describedby');
	expect(describedBy).toBeTruthy();
	await expect(page.locator(`#${describedBy}`)).toHaveText('campaign is required');
});

test('a successful edit shows the server’s normalised document', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	await page.getByLabel('Caption').fill('a quay at dusk');
	await page.getByRole('button', { name: 'Save' }).click();
	await expect(page.getByText('Saved')).toBeVisible();
});

test('the browser has no axe violations at WCAG 2.1 AA', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await expect(page.getByRole('grid')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on /assets:\n${detail}`).toEqual([]);
});

test('the detail panel has no axe violations', async ({ page }) => {
	// Scanned open, because the panel is where the form controls are and a form is where label association
	// goes wrong.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();
	await expect(page.getByRole('complementary', { name: 'Selected asset' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.failureSummary).join(' | ')}`)
		.join('\n');
	expect(results.violations, `axe violations with the panel open:\n${detail}`).toEqual([]);
});

test('the upload panel has no axe violations and its file input is reachable by keyboard', async ({
	page
}) => {
	// A `display: none` input is unreachable by Tab, which makes the control mouse-only — the exact failure
	// a styled drop zone invites.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('button', { name: 'Upload' }).click();

	const input = page.locator('input[type="file"]');
	await expect(input).toBeAttached();
	await input.focus();
	await expect(input).toBeFocused();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});

test('an unconnected browser explains itself instead of failing silently', async ({ page }) => {
	// No `addInitScript`, so there is no key. The grid must not simply be empty: an empty library and an
	// unconfigured app look identical, and only one of them is actionable.
	await page.goto('/assets');
	await expect(page.getByRole('alert')).toContainText('Not connected');
	await expect(page.getByRole('link', { name: 'Open Settings' })).toBeVisible();
});

test('the settings page distinguishes a missing server from a rejected key', async ({ page }) => {
	// The two failures a user actually hits, and the ones that look the same if the check is one request:
	// somebody re-issues a key that was fine because the port was wrong.
	await page.route('**/127.0.0.1:9999/**', (route) => route.abort());
	await page.goto('/settings');

	await page.getByLabel('API address').fill('http://127.0.0.1:9999');
	await page.getByLabel('API key').fill('damrs_whatever');
	await page.getByRole('button', { name: 'Connect' }).click();

	await expect(page.getByRole('status')).toContainText('No server answered');
});

test('settings reports a key that authenticates but grants nothing', async ({ page }) => {
	await page.route('**/127.0.0.1:8099/health', (route) => route.fulfill({ body: 'ok' }));
	await page.route('**/127.0.0.1:8099/assets*', (route) =>
		route.fulfill({ status: 403, body: '' })
	);
	await page.goto('/settings');

	await page.getByLabel('API address').fill('http://127.0.0.1:8099');
	await page.getByLabel('API key').fill('damrs_machine_key');
	await page.getByRole('button', { name: 'Connect' }).click();

	// A machine key with no membership authenticates and holds nothing. Saying "no permission" rather than
	// "bad key" is the difference between fixing it and re-issuing it.
	await expect(page.getByRole('status')).toContainText('grants nothing');
});

test('the settings page has no axe violations', async ({ page }) => {
	await page.goto('/settings');
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on /settings:\n${detail}`).toEqual([]);
});

test('the grid is a single tab stop and the arrow keys move within it', async ({ page }) => {
	// The roving-tabindex contract, exercised through a real browser rather than synthetic events: tabbing
	// must reach the grid once, not once per cell.
	await connect(page);
	await page.goto('/assets');
	await page.locator('[role="gridcell"]').first().focus();
	await expect(page.locator('[role="gridcell"][tabindex="0"]')).toHaveCount(1);

	await page.keyboard.press('ArrowRight');
	await expect(page.locator('[role="gridcell"]').nth(1)).toBeFocused();
	await page.keyboard.press('ArrowDown');
	await expect(page.locator('[role="gridcell"]').nth(5)).toBeFocused();
	await expect(page.locator('[role="gridcell"][tabindex="0"]')).toHaveCount(1);
});

test('selecting an asset is announced in a live region', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.locator('[role="gridcell"]').first().click();
	await expect(page.getByRole('status').filter({ hasText: /selected/i })).toHaveText(
		/1 of 120 assets selected/i
	);
});

test('a multivalued field is sent as an array, not a delimited string', async ({ page }) => {
	// The bug this endpoint exists for. Without `multivalued` the editor sent `"blue, green"` to a field that
	// takes an array, and the server refused it with a message about delimiters that a user can do nothing
	// with. Found by editing a multivalued field in a real browser, not by any test that existed at the time.
	const recorder = await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	const colours = page.getByLabel('Colours');
	await expect(colours).toHaveValue('blue, red');
	await colours.fill('blue, green, teal');
	await page.getByRole('button', { name: 'Save' }).click();
	await expect(page.getByText('Saved')).toBeVisible();

	expect(recorder.patches.at(-1)).toEqual({ colours: ['blue', 'green', 'teal'] });
});

test('a read-only field is not offered for editing', async ({ page }) => {
	// Offering it produces a refusal the user cannot act on: the field is set by ingest or a connector.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	await expect(page.getByLabel('Ingested at')).toHaveCount(0);
	// Said out loud rather than silently omitted, or a user wonders where a field they can see elsewhere went.
	await expect(page.getByText(/1 read-only field not shown/)).toBeVisible();
});

test('a required field is marked as such for a screen reader, not only with an asterisk', async ({
	page
}) => {
	// "asterisk" read aloud is not information.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	// The accessible name is the label's whole text content, asterisk and parenthetical included — which is
	// exactly what a screen reader reads, so asserting on it asserts what is announced.
	const campaign = page.getByLabel(/Campaign.*required/);
	await expect(campaign).toBeVisible();
	await expect(page.locator('label[for="f-campaign"]')).toContainText('(required)');
});

test('a multivalued field says how to separate values, and the hint is announced with the field', async ({
	page
}) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	const colours = page.getByLabel('Colours');
	const describedBy = await colours.getAttribute('aria-describedby');
	expect(describedBy).toBeTruthy();
	await expect(page.locator(`#${describedBy}`)).toHaveText('Separate values with commas.');
});

test('an asset with a thumbnail renders it, and one without says it is processing', async ({
	page
}) => {
	// The chain's visible end. Between an upload finishing and the worker deriving it there is no thumbnail, and
	// a cell that rendered an empty box would look like a broken image rather than work in progress.
	const recorder = await connect(page);
	await page.goto('/assets');
	await expect(page.getByRole('grid')).toBeVisible();

	const cells = page.getByRole('gridcell');
	const first = cells.nth(0).locator('img');
	await expect(first).toHaveAttribute('src', 'http://127.0.0.1:8099/d/absolute-token');
	// Decoded, not merely present: a 404 would still be an `<img>` in the DOM.
	await expect(first).toHaveJSProperty('naturalWidth', 1);

	// Index 2 is `archive` in the fixture, and the placeholder says so rather than "processing": an archived
	// asset is not being worked on, it is in cold storage, and telling a user "processing" about something that
	// will never finish is worse than saying nothing.
	await expect(cells.nth(2).getByTestId('thumbnail-placeholder')).toHaveText('archived');
	await expect(cells.nth(2).locator('img')).toHaveCount(0);
	// Index 3 is `restoring`, which is neither hot nor archived — it falls through to "processing".
	await expect(cells.nth(3).getByTestId('thumbnail-placeholder')).toHaveText('processing');

	expect(recorder.urls.some((url) => url.includes('/d/absolute-token'))).toBe(true);
});

test('a root-relative thumbnail URL is resolved against the API, not the page', async ({
	page
}) => {
	// The server sends `/d/<token>` unless the deployment configures its public origin. A browser resolves that
	// against *this* origin — in development a Vite port — and gets a 404 from the wrong server. Found exactly
	// that way.
	await connect(page);
	await page.goto('/assets');

	const second = page.getByRole('gridcell').nth(1).locator('img');
	await expect(second).toHaveAttribute('src', 'http://127.0.0.1:8099/d/relative-token');
	await expect(second).toHaveJSProperty('naturalWidth', 1);
});

test('a thumbnail is not announced twice', async ({ page }) => {
	// The filename beneath is already the cell's accessible name. A screen reader reading "harbour.jpg, image,
	// harbour.jpg" is worse than one that reads it once — so the image is decorative *in this context*, and the
	// asset's real alt text is a metadata field for where the image carries content.
	await connect(page);
	await page.goto('/assets');

	const img = page.getByRole('gridcell').nth(0).locator('img');
	await expect(img).toHaveAttribute('alt', '');
	await expect(img).toHaveAttribute('aria-hidden', 'true');
});

test('the detail panel shows the thumbnail too', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	const panel = page.getByRole('complementary', { name: 'Selected asset' });
	await expect(panel.locator('img')).toHaveJSProperty('naturalWidth', 1);
});

test('the lightbox opens on activate, is a real modal, and steps with the arrow keys', async ({
	page
}) => {
	// `<dialog showModal()>` rather than a div: the focus trap, the inert background and Escape all come from
	// the platform, and the trap is the one hand-rolled modals reliably get wrong — a keyboard user tabs
	// straight out of a div "modal" and operates a UI they cannot see.
	await connect(page);
	await page.goto('/assets');

	// Enter on the focused cell is the activate gesture; a click only selects.
	await page.getByRole('gridcell').first().click();
	await expect(page.getByRole('dialog')).toHaveCount(0, {});
	await page.keyboard.press('Enter');

	const dialog = page.getByRole('dialog');
	await expect(dialog).toBeVisible();
	// Named, or a screen reader announces "dialog" with no indication of what opened.
	await expect(dialog).toHaveAccessibleName(/Preview of campaign-0000\.jpg/);
	// The preview, not the thumbnail: `Contain`-fitted, so nothing is cropped out of an image being inspected.
	await expect(dialog.locator('img')).toHaveAttribute(
		'src',
		'http://127.0.0.1:8099/d/preview-token'
	);
	// And it carries its filename as alt text, unlike the grid — here the image *is* the content.
	await expect(dialog.locator('img')).toHaveAttribute('alt', 'campaign-0000.jpg');

	// Modal means the background is inert. `showModal` is what provides that; the `open` attribute does not.
	await expect(page.locator('dialog[open]')).toHaveCount(1);

	await page.keyboard.press('ArrowRight');
	await expect(dialog).toHaveAccessibleName(/campaign-0000\.jpg/);

	await page.keyboard.press('Escape');
	await expect(page.getByRole('dialog')).toHaveCount(0);
});

test('the lightbox says why there is no preview rather than showing an empty frame', async ({
	page
}) => {
	// Between an upload finishing and the worker rendering, and for formats that never get an image rendition,
	// there is no preview. An empty frame reads as a broken image.
	const recorder = await connect(page);
	await page.route('**/127.0.0.1:8099/assets/*', async (route) => {
		if (route.request().url().includes('/metadata')) return route.fallback();
		recorder.urls.push(route.request().url());
		return route.fulfill({
			json: {
				...summary(2),
				values: {},
				technical: {},
				duration_ms: null,
				page_count: null,
				color_space: null,
				has_alpha: false,
				content_hash: 'b'.repeat(64),
				status: 'active',
				enrichment_state: 'pending',
				legal_hold: false,
				release_at: null,
				expires_at: null,
				version_no: 1,
				created_at: '2026-08-01T09:00:00Z',
				updated_at: '2026-08-01T09:00:00Z',
				thumbnail_url: null,
				preview_url: null
			}
		});
	});

	await page.goto('/assets');
	await page.getByRole('gridcell').nth(2).click();
	await page.keyboard.press('Enter');

	const dialog = page.getByRole('dialog');
	await expect(dialog).toBeVisible();
	await expect(dialog.locator('img')).toHaveCount(0);
	// Index 2 is `archive` in the fixture, so the reason is cold storage rather than "still processing".
	await expect(dialog.getByText(/cold storage/)).toBeVisible();
});

test('the lightbox has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();
	await page.keyboard.press('Enter');
	await expect(page.getByRole('dialog')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.failureSummary).join(' | ')}`)
		.join('\n');
	expect(results.violations, `axe violations in the lightbox:\n${detail}`).toEqual([]);
});

test('the bulk bar appears with a selection and runs a delete to its honest end state', async ({
	page
}) => {
	const recorder = await connect(page);
	await page.goto('/assets');

	// No selection, no bar: a toolbar for nothing is noise.
	await expect(page.getByRole('toolbar', { name: 'Bulk operations' })).toHaveCount(0);

	// Click selects one; ctrl-click adds a second.
	await page.getByRole('gridcell').nth(0).click();
	await page
		.getByRole('gridcell')
		.nth(1)
		.click({ modifiers: ['ControlOrMeta'] });

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await expect(bar).toBeVisible();
	await expect(bar).toContainText('2 selected');

	// Delete goes through the preview first, and the dialog carries the server's numbers — including how many
	// of the selection fell outside the caller's scope.
	await bar.getByRole('button', { name: 'Delete…' }).click();
	await expect(bar).toContainText('Delete 1 asset?');
	await expect(bar).toContainText('1 of the selection is outside your scope');
	const previewCall = recorder.bulk.find((c) => c.url.includes('/bulk/preview'));
	expect(previewCall?.body.kind).toBe('delete');
	expect(previewCall?.body.asset_ids).toHaveLength(2);

	// Confirming creates the operation with the same selection the preview saw.
	await bar.getByRole('button', { name: 'Delete 1 asset' }).click();

	// The end state is `partial`, rendered as exactly that — the named failure and no green tick.
	//
	// Asserted *before* the recorder, and that ordering is the fix for a real flake: clicking starts an async
	// create-then-poll, and reading `recorder` on the next line raced it — the POST had not landed yet, so
	// `find` returned undefined about two runs in three. Waiting on something the user can see is also the
	// honest synchronisation point; a bare timeout would only make the race rarer.
	await expect(bar).toContainText('partial: 1 applied, 1 failed');
	await expect(bar).toContainText('legal hold blocks deletion');

	const createCall = recorder.bulk.find((c) => c.url.endsWith('/bulk'));
	expect(createCall?.body.asset_ids).toEqual(previewCall?.body.asset_ids);

	// Dismissing clears the selection, so the next operation starts from nothing.
	await bar.getByRole('button', { name: 'Dismiss' }).click();
	await expect(page.getByRole('toolbar', { name: 'Bulk operations' })).toHaveCount(0);
});

test('ordering a selection reports the server’s count', async ({ page }) => {
	// An order is not a bulk operation — it is a request for somebody's decision, so it skips the
	// preview/confirm flow. What it keeps is the honesty about numbers: the server narrows the selection to what
	// the requester may ask for, and the bar says so rather than implying all ten went.
	const recorder = await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();
	await page
		.getByRole('gridcell')
		.nth(1)
		.click({ modifiers: ['ControlOrMeta'] });

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await bar.getByRole('button', { name: 'Order…' }).click();
	await bar.getByLabel('Why').fill('The spring brochure');
	await bar.getByRole('button', { name: 'Send request' }).click();

	await expect(bar).toContainText('ORD-000009 sent for approval');
	// The mock returns one fewer item than asked for, so the count is stated rather than assumed.
	await expect(bar).toContainText('1 of 2 are yours to ask for');
	expect(recorder.ordered).toEqual(['2:The spring brochure']);
});

test('adding a selection to a collection reports what the server took', async ({ page }) => {
	// Not routed through the bulk preview/confirm flow, and deliberately: adding to a collection is arranging
	// a working set, not an audited operation over a target set. What it keeps is the honesty about numbers —
	// the server filters the ids through the caller's own scope, so the bar states what arrived rather than
	// implying the whole selection did.
	const recorder = await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();
	await page
		.getByRole('gridcell')
		.nth(1)
		.click({ modifiers: ['ControlOrMeta'] });

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await bar.getByRole('button', { name: 'Add to collection…' }).click();
	// The list is fetched on open rather than on mount: the bar appears on every selection and most selections
	// never touch a collection.
	await bar.getByRole('button', { name: /Press kit/ }).click();

	await expect(bar).toContainText('Added 1 to Press kit');
	await expect(bar).toContainText('1 were outside your scope');
	expect(recorder.added).toEqual(['press-kit:2']);
});

test('an order needs a reason before it can be sent', async ({ page }) => {
	// The reason is the entire question an approver answers, so the button is unavailable until there is one.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await bar.getByRole('button', { name: 'Order…' }).click();
	await expect(bar.getByRole('button', { name: 'Send request' })).toBeDisabled();
	await bar.getByLabel('Why').fill('Because');
	await expect(bar.getByRole('button', { name: 'Send request' })).toBeEnabled();
});

test('the bulk metadata flow sends one field as a patch', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();
	await page
		.getByRole('gridcell')
		.nth(1)
		.click({ modifiers: ['ControlOrMeta'] });

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await bar.getByRole('button', { name: 'Set metadata…' }).click();
	await bar.getByLabel('Field').selectOption('campaign');
	await bar.getByLabel('Value').fill('spring-2026');
	await bar.getByRole('button', { name: 'Preview' }).click();
	await expect(bar).toContainText('Update 1 asset?');
	await bar.getByRole('button', { name: 'Update 1 asset' }).click();

	// Waits for the operation to reach a terminal state before reading what was sent — see the note in the
	// delete case above for why reading the recorder straight after the click is a race.
	await expect(bar).toContainText(/partial|Done/);

	const createCall = recorder.bulk.find((c) => c.url.endsWith('/bulk'));
	expect(createCall?.body.kind).toBe('metadata_set');
	expect(
		(createCall?.body as { params?: { values?: Record<string, string> } }).params?.values
	).toEqual({
		campaign: 'spring-2026'
	});
});

test('changing the selection abandons an unconfirmed bulk dialog', async ({ page }) => {
	// The previewed numbers are for another set; confirming them against a new selection would delete
	// something the dialog never described.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();

	const bar = page.getByRole('toolbar', { name: 'Bulk operations' });
	await bar.getByRole('button', { name: 'Delete…' }).click();
	await expect(bar).toContainText('Delete 1 asset?');

	await page
		.getByRole('gridcell')
		.nth(2)
		.click({ modifiers: ['ControlOrMeta'] });
	await expect(bar).not.toContainText('Delete 1 asset?');
	await expect(bar).toContainText('2 selected');
});

test('a published asset says so, and the bar can publish a selection', async ({ page }) => {
	// Q.14. Publication is the act that admits an asset to a page anybody can reach, so it is a chip on the
	// cell and a confirmation that names it — not a metadata edit reading "Update 2 assets".
	const recorder = await connect(page);
	await page.goto('/assets');

	await expect(page.getByTestId('published-badge').first()).toBeVisible();

	// Selection is a click on the cell, as everywhere else in this grid.
	await page.getByRole('gridcell').nth(0).click();
	// Exact, because "Unpublish…" contains "Publish…".
	await page.getByRole('button', { name: 'Publish…', exact: true }).click();
	await expect(page.getByRole('button', { name: /^Publish 1 asset$/ })).toBeVisible();
	await page.getByRole('button', { name: /^Publish 1 asset$/ }).click();
	expect(recorder.bulk.some((call) => call.body.kind === 'publish')).toBe(true);
});

test('the current search downloads as a CSV, and an oversized one says how large', async ({
	page
}) => {
	// Q.18. The export is authenticated, so it is a fetch and a blob rather than a link — and the failure worth
	// showing is the one with a number in it.
	const recorder = await connect(page);
	await page.goto('/assets?q=brand:acme');

	const download = page.waitForEvent('download');
	await page.getByRole('button', { name: 'Export CSV' }).click();
	const file = await download;
	expect(file.suggestedFilename()).toBe('search-results.csv');
	// The query travelled: an export of the whole library would be a different file from the one on screen.
	expect(recorder.urls.some((url) => url.includes('export.csv?q=brand%3Aacme'))).toBe(true);

	await page.goto('/assets?q=brand:everything');
	await page.getByRole('button', { name: 'Export CSV' }).click();
	// Filtered: the page carries more than one alert region, and the server's sentence is the one this case is
	// about — the count and what to do about it.
	const refusal = page.getByRole('alert').filter({ hasText: 'that search matches' });
	await expect(refusal).toContainText('40000');
	await expect(refusal).toContainText('narrow the query');
});

test('the bulk bar has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();
	await expect(page.getByRole('toolbar', { name: 'Bulk operations' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});

test('a dependent field appears only when its condition is met', async ({ page }) => {
	// Q.19b. The server refuses an inapplicable value too; hiding the box is why nobody has to see that
	// refusal. A form that asks a question and then rejects the answer is a form nobody trusts back.
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();

	const panel = page.getByRole('complementary', { name: 'Selected asset' });
	await expect(panel.getByLabel('Has people')).toBeVisible();
	// Not rendered at all, and the count below says why — a field simply absent reads as a schema somebody
	// forgot to finish.
	await expect(panel.getByLabel('Release reference')).toHaveCount(0);
	await expect(panel.getByText(/applies only when another field says so/)).toBeVisible();

	// Answering the parent reveals it in the same breath, from the draft rather than after a save: a form that
	// only revealed it on reload is a form somebody saves twice.
	await panel.getByLabel('Has people').fill('true');
	await expect(panel.getByLabel('Release reference')).toBeVisible();

	// And taking the answer back hides it again.
	await panel.getByLabel('Has people').fill('false');
	await expect(panel.getByLabel('Release reference')).toHaveCount(0);
});
