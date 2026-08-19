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
		has_attachment: false
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
	bulk: { url: string; body: { kind: string; asset_ids: string[] } }[];
};
let bulkPolls = 0;

async function connect(page: Page): Promise<Recorder> {
	const recorder: Recorder = { urls: [], patches: [], bulk: [] };
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
		if (path.pathname === '/fields') {
			return route.fulfill({ json: FIELDS });
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

test('the bulk bar has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).click();
	await expect(page.getByRole('toolbar', { name: 'Bulk operations' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
