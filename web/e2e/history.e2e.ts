/**
 * The per-asset history panel through a real browser, against a mocked API (Q.10).
 *
 * The Rust gate proves the scoping and the group-wide read. What exists only here are four properties of the
 * *interface*:
 *
 * - **Nothing is fetched until it is opened.** A detail panel already makes several requests, and most people
 *   opening an asset want the picture and the fields, not an audit trail.
 * - **It stays open across assets.** Somebody who opened one history is usually comparing several; re-closing it on
 *   every selection would be the panel forgetting what they just asked for.
 * - **The sibling note appears only when a line names another file.** A history covers the whole version group, and
 *   an unexplained filename in the list is confusing — but saying so on a single-version asset is noise.
 * - **The phrasing is the dashboard's.** One renderer, so an unrecognised kind is phrased rather than dropped here
 *   too. That is asserted rather than assumed, because the shared module is only shared while both sides import it.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';
const OTHER = '00000000-0000-4000-8000-0000000000bb';

const ADA = { id: 'p-ada', name: 'Ada Lovelace', email: 'ada@example.com' };

type Entry = {
	id: string;
	occurred_at: string;
	kind: string;
	asset_id: string | null;
	filename: string | null;
	actor: typeof ADA | null;
	context: Record<string, unknown>;
};

function entry(overrides: Partial<Entry> = {}): Entry {
	return {
		id: `e-${overrides.kind ?? 'x'}-${overrides.filename ?? 'y'}`,
		occurred_at: new Date(Date.now() - 5 * 60_000).toISOString(),
		kind: 'upload',
		asset_id: ASSET,
		filename: 'brochure.pdf',
		actor: ADA,
		context: {},
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		history?: Entry[];
		/** Per-asset histories, for the case that moves between two assets. */
		byAsset?: Record<string, Entry[]>;
		status?: number;
		/** Two rows in the grid, so a second asset can be selected. */
		two?: boolean;
		/** Delays the history response, so the window between selecting an asset and its history arriving is long
		 *  enough to sample. */
		slow?: boolean;
	} = {}
) {
	/** Every history request, in order — this is what proves nothing was fetched too early. */
	const asked: string[] = [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;

		if (path.endsWith('/history')) {
			const id = path.split('/')[2] ?? '';
			asked.push(id);
			if (options.status) {
				return route.fulfill({
					status: options.status,
					json: { reason: 'the events table is gone' }
				});
			}
			const entries = options.byAsset ? (options.byAsset[id] ?? []) : (options.history ?? []);
			if (options.slow) await new Promise((resolve) => setTimeout(resolve, 700));
			return route.fulfill({ json: entries });
		}
		if (path === '/assets' || path === '/search') {
			const items = options.two ? [summary(ASSET), summary(OTHER, 'poster.jpg')] : [summary(ASSET)];
			return route.fulfill({ json: { items, total: items.length, offset: 0 } });
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path.endsWith('/comments')) return route.fulfill({ json: [] });
		if (path.endsWith('/categories')) return route.fulfill({ json: [] });
		if (path.endsWith('/attachments')) return route.fulfill({ json: [] });
		if (path.endsWith('/versions')) return route.fulfill({ json: [] });
		if (path.endsWith('/type')) return route.fulfill({ json: { field_keys: [] } });
		if (path.startsWith('/assets/')) {
			const id = path.split('/')[2] ?? ASSET;
			return route.fulfill({
				json: {
					...summary(id, id === OTHER ? 'poster.jpg' : 'brochure.pdf'),
					values: {},
					technical: {},
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
					engagement: {
						asset_id: id,
						average_stars: null,
						rating_count: 0,
						favourite_count: 0,
						my_stars: null,
						is_favourite: false,
						is_watched: false
					},
					preview_url: null
				}
			});
		}
		return route.fulfill({ status: 404, json: {} });
	});

	function summary(id: string, filename = 'brochure.pdf') {
		return {
			id,
			filename,
			mime: 'application/pdf',
			bytes: 2_400_000,
			width: null,
			height: null,
			tier: 'hot',
			rights_state: 'allowed',
			provenance_state: 'none',
			thumbnail_url: null,
			tag_confidence: null,
			is_favourite: false,
			average_stars: null,
			has_attachment: false
		};
	}

	return { asked };
}

async function open(page: Page, nth = 0) {
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(nth).dblclick();
	await expect(page.getByRole('region', { name: 'Comments' })).toBeVisible();
}

test('the history is not fetched until it is opened', async ({ page }) => {
	const recorder = await connect(page, { history: [entry()] });
	await open(page);

	// The detail panel is fully rendered — the comments region above is the marker — and nothing has been asked
	// for. A panel that fetched eagerly would add a request to every asset anybody clicks.
	expect(recorder.asked).toEqual([]);

	await page.getByText('History', { exact: true }).click();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toBeVisible();
	expect(recorder.asked).toEqual([ASSET]);
});

test('an opened history follows the selection', async ({ page }) => {
	const recorder = await connect(page, {
		two: true,
		byAsset: {
			[ASSET]: [entry({ kind: 'upload', filename: 'brochure.pdf' })],
			[OTHER]: [entry({ kind: 'download', filename: 'poster.jpg', id: 'e-other' })]
		}
	});
	await open(page);
	await page.getByText('History', { exact: true }).click();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toBeVisible();

	// Select the second asset in the same page — the disclosure's open state is the component's, so a navigation
	// would reset it and prove nothing. It stays open and shows *that* asset's history, rather than closing or
	// leaving the first asset's lines under the second's name.
	await page.getByRole('gridcell').nth(1).dblclick();
	await expect(page.getByText('Ada Lovelace downloaded poster.jpg')).toBeVisible();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toHaveCount(0);
	expect(recorder.asked).toEqual([ASSET, OTHER]);
});

test("the previous asset's history does not linger while the next one loads", async ({ page }) => {
	// The reset exists for this window and no end-state assertion can see it: the response overwrites the list
	// anyway, so without it the only symptom is a moment of showing one asset's history under another's name. On a
	// panel whose lines name filenames, that moment reads as a fact about the wrong asset.
	await connect(page, {
		two: true,
		slow: true,
		byAsset: {
			[ASSET]: [entry({ kind: 'upload', filename: 'brochure.pdf' })],
			[OTHER]: [entry({ kind: 'download', filename: 'poster.jpg', id: 'e-other' })]
		}
	});
	await open(page);
	await page.getByText('History', { exact: true }).click();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toBeVisible();

	await page.getByRole('gridcell').nth(1).dblclick();

	// Sampled at a point in time rather than with a retrying matcher, which would poll until the delayed response
	// arrived and cleared the list either way. 150 ms into a 700 ms window the panel is either already showing
	// "Loading…" (correct) or still showing the first asset's line (the bug).
	await page.waitForTimeout(150);
	const during = await page.locator('#detail-panel, main').first().innerText();
	expect(during).not.toContain('uploaded brochure.pdf');

	await expect(page.getByText('Ada Lovelace downloaded poster.jpg')).toBeVisible();
});

test('an unrecognised kind is phrased rather than dropped', async ({ page }) => {
	// The same renderer as the dashboard feed. Asserted here because the module is only shared while both sides
	// import it, and a second copy would be a second place to forget this.
	await connect(page, {
		history: [entry({ kind: 'transcoded', filename: 'brochure.pdf', id: 'e-odd' })]
	});
	await open(page);
	await page.getByText('History', { exact: true }).click();

	await expect(page.getByText('Ada Lovelace: transcoded on brochure.pdf')).toBeVisible();
});

test('a history that names another version says why', async ({ page }) => {
	await connect(page, {
		history: [
			entry({ kind: 'edit', filename: 'brochure-v2.pdf', id: 'e-v2' }),
			entry({ kind: 'upload', filename: 'brochure.pdf' })
		]
	});
	await open(page);
	await page.getByText('History', { exact: true }).click();

	// A filename the reader did not open is confusing without this line.
	await expect(page.getByText('Includes every version of this asset')).toBeVisible();
});

test('a single-version history does not explain versions', async ({ page }) => {
	await connect(page, { history: [entry({ kind: 'upload', filename: 'brochure.pdf' })] });
	await open(page);
	await page.getByText('History', { exact: true }).click();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toBeVisible();

	// Nothing to explain: every line is about the asset on screen. Saying it anyway would be noise on most assets.
	await expect(page.getByText('Includes every version of this asset')).toHaveCount(0);
});

test('an asset with no recorded history says so', async ({ page }) => {
	await connect(page, { history: [] });
	await open(page);
	await page.getByText('History', { exact: true }).click();

	// An asset imported before events were recorded has none. An empty panel would leave a reader wondering whether
	// the request failed.
	await expect(page.getByText('Nothing recorded yet')).toBeVisible();
});

test('a failure is reported rather than left as an empty list', async ({ page }) => {
	await connect(page, { status: 500 });
	await open(page);
	await page.getByText('History', { exact: true }).click();

	await expect(page.getByRole('alert')).toContainText('events table is gone');
	// And *only* that. Driving this against the live server showed the failure stacked on top of "nothing recorded
	// yet", which together read as "the read failed, and also there is no history" — a claim a failed read is in no
	// position to make.
	await expect(page.getByText('Nothing recorded yet')).toHaveCount(0);
});

test('the history panel has no accessibility violations', async ({ page }) => {
	await connect(page, {
		history: [
			entry({ kind: 'upload' }),
			entry({ kind: 'comment', context: { visibility: 'private' }, id: 'e-p' }),
			entry({ kind: 'edit', filename: 'brochure-v2.pdf', id: 'e-v2' })
		]
	});
	await open(page);
	await page.getByText('History', { exact: true }).click();
	await expect(page.getByText('Ada Lovelace uploaded brochure.pdf')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
