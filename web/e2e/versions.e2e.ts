/**
 * The version panel through a real browser, against a mocked API.
 *
 * Three properties that exist only here:
 *
 * - **A single-version asset shows no panel at all.** Every asset is version 1 of itself, so "Versions (1)" on every
 *   asset in the library is noise on almost all of them.
 * - **"Current" is a state, not a control.** A disabled button on the current row would be a control that exists to
 *   be unavailable.
 * - **Promoting keeps the version number.** The history is the one place that has to stay literal: renumbering
 *   version 2 as version 4 would claim somebody uploaded something they did not.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';
const OLD = '00000000-0000-4000-8000-0000000000aa';

type Version = {
	asset_id: string;
	version_no: number;
	is_current: boolean;
	filename: string;
	bytes: number;
	content_hash: string;
	replaces_id: string | null;
	uploaded_by: { id: string; name: string; email: string } | null;
	created_at: string;
};

function version(overrides: Partial<Version> = {}): Version {
	return {
		asset_id: ASSET,
		version_no: 1,
		is_current: true,
		filename: 'brochure.pdf',
		bytes: 2_400_000,
		content_hash: 'a'.repeat(64),
		replaces_id: null,
		uploaded_by: { id: 'p-ada', name: 'Ada Lovelace', email: 'ada@example.com' },
		created_at: '2026-08-01T09:00:00Z',
		...overrides
	};
}

async function connect(page: Page, options: { versions?: Version[]; promoteStatus?: number } = {}) {
	const promoted: string[] = [];
	let versions = options.versions ?? [version()];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;
		const method = route.request().method();

		if (path.endsWith('/versions/current') && method === 'POST') {
			const id = path.split('/')[2] ?? '';
			promoted.push(id);
			if (options.promoteStatus) {
				return route.fulfill({
					status: options.promoteStatus,
					json: { reason: 'no longer yours' }
				});
			}
			versions = versions.map((row) => ({ ...row, is_current: row.asset_id === id }));
			return route.fulfill({ json: versions });
		}
		if (path.endsWith('/versions')) {
			return route.fulfill({ json: versions });
		}
		if (path === '/assets') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/search') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path.endsWith('/comments')) return route.fulfill({ json: [] });
		if (path.endsWith('/categories')) return route.fulfill({ json: [] });
		if (path.endsWith('/type')) return route.fulfill({ json: { field_keys: [] } });
		if (path.startsWith('/assets/')) {
			return route.fulfill({
				json: {
					...summary(),
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
						asset_id: ASSET,
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

	function summary() {
		return {
			id: ASSET,
			filename: 'brochure.pdf',
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
			average_stars: null
		};
	}

	return { promoted };
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Version history' });
}

async function open(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').first().dblclick();
	await expect(page.getByRole('region', { name: 'Comments' })).toBeVisible();
}

test('a single-version asset shows no version panel', async ({ page }) => {
	await connect(page);
	await open(page);

	// Every asset is version 1 of itself. A panel on every asset in the library would be noise on almost all of
	// them, and the detail panel is already long.
	await expect(panel(page)).toHaveCount(0);
});

test('a history lists newest first and marks the current one', async ({ page }) => {
	await connect(page, {
		versions: [
			version({ version_no: 2, is_current: true, filename: 'brochure-v2.pdf', replaces_id: OLD }),
			version({ asset_id: OLD, version_no: 1, is_current: false, filename: 'brochure-v1.pdf' })
		]
	});
	await open(page);

	await expect(panel(page).getByRole('heading', { level: 3 })).toContainText('Versions (2)');
	const rows = panel(page).getByRole('listitem');
	await expect(rows.nth(0)).toContainText('v2');
	await expect(rows.nth(0).getByText('current', { exact: true })).toHaveCount(1);
	await expect(rows.nth(1)).toContainText('v1');

	// "Current" is a state on that row and offers nothing; only the others offer a way to change it.
	await expect(rows.nth(0).getByRole('button', { name: 'Make current' })).toHaveCount(0);
	await expect(rows.nth(1).getByRole('button', { name: 'Make current' })).toHaveCount(1);
});

test('promoting an earlier version keeps its number', async ({ page }) => {
	const recorder = await connect(page, {
		versions: [
			version({ version_no: 2, is_current: true, filename: 'brochure-v2.pdf', replaces_id: OLD }),
			version({ asset_id: OLD, version_no: 1, is_current: false, filename: 'brochure-v1.pdf' })
		]
	});
	await open(page);

	await panel(page).getByRole('button', { name: 'Make current' }).click();

	// v1 is current and is still v1 — a promotion, not a copy. Renumbering it would claim an upload that never
	// happened.
	// The badge, matched exactly — the demoted row also contains the words "Make current" on its button, so a
	// substring check would find "current" on both rows and pass whatever happened.
	const rows = panel(page).getByRole('listitem');
	await expect(rows.filter({ hasText: 'v1' }).getByText('current', { exact: true })).toHaveCount(1);
	await expect(rows.filter({ hasText: 'v2' }).getByText('current', { exact: true })).toHaveCount(0);
	// And no renumbered copy appeared.
	await expect(panel(page).getByRole('listitem').filter({ hasText: 'v4' })).toHaveCount(0);
	expect(recorder.promoted).toEqual([OLD]);
});

test('a refusal is shown in the server’s own words', async ({ page }) => {
	await connect(page, {
		versions: [
			version({ version_no: 2, is_current: true }),
			version({ asset_id: OLD, version_no: 1, is_current: false })
		],
		promoteStatus: 404
	});
	await open(page);

	await panel(page).getByRole('button', { name: 'Make current' }).click();
	await expect(panel(page).getByRole('alert')).toContainText('no longer yours');
});

test('the panel says which version a download resolves to', async ({ page }) => {
	await connect(page, {
		versions: [
			version({ version_no: 2, is_current: true }),
			version({ asset_id: OLD, version_no: 1, is_current: false })
		]
	});
	await open(page);

	// The rule the rest of the system follows, stated where somebody is looking at two versions and choosing.
	await expect(panel(page)).toContainText('resolve to the current version');
	await expect(panel(page)).toContainText('Older ones stay readable by their own link');
});

test('the version panel has no accessibility violations', async ({ page }) => {
	await connect(page, {
		versions: [
			version({ version_no: 3, is_current: true }),
			version({ asset_id: OLD, version_no: 2, is_current: false, uploaded_by: null }),
			version({ asset_id: 'x', version_no: 1, is_current: false })
		]
	});
	await open(page);
	await expect(panel(page)).toBeVisible();

	const results = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Version history"]')
		.analyze();
	expect(results.violations).toEqual([]);
});
