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
		thumbnail_url: null,
		tag_confidence: index % 3 === 0 ? null : 0.55 + (index % 40) / 100
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
type Recorder = { urls: string[]; patches: Record<string, unknown>[] };

async function connect(page: Page): Promise<Recorder> {
	const recorder: Recorder = { urls: [], patches: [] };

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
					updated_at: '2026-08-01T09:00:00Z'
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

	const campaign = page.getByLabel('Campaign');
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
