/**
 * The attachment panel through a real browser, against a mocked API.
 *
 * Two properties exist only here:
 *
 * - **An empty list is stated, not hidden.** "No release on file" is what somebody clearing an asset for use needs
 *   to know, and an absent panel would make that indistinguishable from the feature not existing. This is the
 *   opposite decision from the version panel, which *does* hide itself — because every asset is version 1 of itself
 *   and no asset is paperwork by default.
 * - **Detaching says it is not deleting.** Somebody correcting a mis-attachment must not have to wonder whether
 *   they destroyed a signed release.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';
const DOC = '00000000-0000-4000-8000-0000000000dd';
/** A second asset, so switching selection can be exercised at all. */
const OTHER = '00000000-0000-4000-8000-000000000001';

type Attachment = {
	asset_id: string;
	attached_to: string;
	kind: string;
	filename: string;
	mime: string;
	bytes: number;
	uploaded_by: { id: string; name: string; email: string } | null;
	created_at: string;
};

function attachment(overrides: Partial<Attachment> = {}): Attachment {
	return {
		asset_id: DOC,
		attached_to: ASSET,
		kind: 'release',
		filename: 'model-release.pdf',
		mime: 'application/pdf',
		bytes: 184_320,
		uploaded_by: { id: 'p-ada', name: 'Ada Lovelace', email: 'ada@example.com' },
		created_at: '2026-08-01T09:00:00Z',
		...overrides
	};
}

async function connect(
	page: Page,
	options: { documents?: Attachment[]; detachStatus?: number; slowList?: boolean } = {}
) {
	const detached: string[] = [];
	let documents = options.documents ?? [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;
		const method = route.request().method();

		if (path.includes('/attachments/') && method === 'DELETE') {
			const id = path.split('/').pop() ?? '';
			detached.push(id);
			if (options.detachStatus) {
				return route.fulfill({ status: options.detachStatus, json: { reason: 'not yours' } });
			}
			documents = documents.filter((row) => row.asset_id !== id);
			return route.fulfill({ status: 204, body: '' });
		}
		if (path === `/assets/${OTHER}/attachments`) {
			// The second asset has no paperwork, so a list carried across selections would be visible.
			//
			// The delay applies *here* especially: this is the request whose in-flight window the flash would show
			// in. It was originally only on the branch below, which this one shadows — so the window was zero wide
			// and the case passed without exercising anything.
			if (options.slowList) {
				await new Promise((resolve) => setTimeout(resolve, 700));
			}
			return route.fulfill({ json: [] });
		}
		if (path.endsWith('/attachments')) {
			if (options.slowList) {
				// A deliberate delay, so the window between switching assets and the new list arriving is long
				// enough to look at. That window is where a flash of the previous asset's paperwork would show.
				await new Promise((resolve) => setTimeout(resolve, 700));
			}
			return route.fulfill({ json: documents });
		}
		if (path === '/assets') {
			return route.fulfill({ json: { items: [summary(), summary(OTHER)], total: 2, offset: 0 } });
		}
		if (path === '/search') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path.endsWith('/comments')) return route.fulfill({ json: [] });
		if (path.endsWith('/versions')) {
			return route.fulfill({
				json: [
					{
						asset_id: ASSET,
						version_no: 1,
						is_current: true,
						filename: 'portrait.jpg',
						bytes: 1,
						content_hash: 'a'.repeat(64),
						replaces_id: null,
						uploaded_by: null,
						created_at: '2026-08-01T09:00:00Z'
					}
				]
			});
		}
		if (path.endsWith('/categories')) return route.fulfill({ json: [] });
		if (path.endsWith('/type')) return route.fulfill({ json: { field_keys: [] } });
		// Q.11's download panel asks this on every asset the detail view opens. Answered here so a suite about
		// something else does not fail on an unmocked route — and answered with formats, because a panel that
		// renders an empty list is not the panel these suites are meant to be indifferent to.
		if (path.endsWith('/usage-options') || path === '/usage-options') {
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
		if (path.endsWith('/download-options')) {
			return route.fulfill({
				json: { original_available: true, media_class: 'image', conversions: [] }
			});
		}
		if (path.startsWith('/assets/')) {
			const id = path.split('/')[2] ?? ASSET;
			return route.fulfill({
				json: {
					...summary(id),
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

	function summary(id: string = ASSET) {
		return {
			id,
			filename: id === OTHER ? 'landscape.jpg' : 'portrait.jpg',
			mime: 'image/jpeg',
			bytes: 2_400_000,
			width: 4000,
			height: 3000,
			tier: 'hot',
			rights_state: 'allowed',
			provenance_state: 'none',
			thumbnail_url: null,
			tag_confidence: null,
			is_favourite: false,
			average_stars: null,
			has_attachment: id === ASSET && documents.length > 0
		};
	}

	return { detached };
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Attached documents' });
}

async function open(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').first().dblclick();
	await expect(panel(page)).toBeVisible();
}

test('an asset with no paperwork says so rather than hiding the panel', async ({ page }) => {
	await connect(page);
	await open(page);

	// The opposite decision from the version panel, and for a reason: "no release on file" is a fact somebody
	// clearing an asset for use needs, whereas "this is version 1" is true of almost everything.
	await expect(panel(page)).toContainText('No paperwork on file');
	await expect(panel(page)).toContainText('then attach it here');
});

test('a document shows its kind, size and uploader', async ({ page }) => {
	await connect(page, { documents: [attachment()] });
	await open(page);

	await expect(panel(page).getByRole('heading', { level: 3 })).toContainText('Documents (1)');
	const row = panel(page).getByRole('listitem').first();
	// The kind is named, because a PDF could be any of these and the difference matters for a rights question.
	await expect(row).toContainText('Release');
	await expect(row).toContainText('model-release.pdf');
	await expect(row).toContainText('180 KiB');
	await expect(row).toContainText('Ada Lovelace');
});

test('detaching explains that nothing is deleted', async ({ page }) => {
	const recorder = await connect(page, { documents: [attachment()] });
	await open(page);

	await panel(page).getByRole('button', { name: 'Detach' }).click();
	// The consequence, before the irreversible-looking verb: somebody correcting a mis-attachment must not have to
	// wonder whether they destroyed a signed release.
	await expect(panel(page)).toContainText('Nothing is deleted');
	await expect(panel(page)).toContainText('returns model-release.pdf to the library');

	await panel(page).getByRole('button', { name: 'Detach', exact: true }).last().click();
	await expect(panel(page)).toContainText('No paperwork on file');
	expect(recorder.detached).toEqual([DOC]);
});

test('several kinds render with their own labels', async ({ page }) => {
	await connect(page, {
		documents: [
			attachment({ kind: 'release', filename: 'release.pdf' }),
			attachment({ asset_id: 'd2', kind: 'licence', filename: 'licence.pdf' }),
			attachment({ asset_id: 'd3', kind: 'contract', filename: 'contract.pdf' }),
			attachment({ asset_id: 'd4', kind: 'other', filename: 'notes.pdf', uploaded_by: null })
		]
	});
	await open(page);

	await expect(panel(page).getByText('Release', { exact: true })).toBeVisible();
	await expect(panel(page).getByText('Licence', { exact: true })).toBeVisible();
	await expect(panel(page).getByText('Contract', { exact: true })).toBeVisible();
	// `other` reads as "Document" rather than "Other", which says what it is instead of what it is not.
	await expect(panel(page).getByText('Document', { exact: true })).toBeVisible();
});

test("a refusal is shown in the server's own words", async ({ page }) => {
	const recorder = await connect(page, { documents: [attachment()], detachStatus: 403 });
	await open(page);

	await panel(page).getByRole('button', { name: 'Detach' }).click();
	await panel(page).getByRole('button', { name: 'Detach', exact: true }).last().click();
	await expect(panel(page).getByRole('alert')).toContainText('not yours');
	// And the row stays, because the server refused.
	await expect(panel(page).getByText('model-release.pdf')).toBeVisible();
	expect(recorder.detached).toEqual([DOC]);
});

test('the attachment panel has no accessibility violations', async ({ page }) => {
	await connect(page, {
		documents: [attachment(), attachment({ asset_id: 'd2', kind: 'licence', filename: 'l.pdf' })]
	});
	await open(page);

	const results = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Attached documents"]')
		.analyze();
	expect(results.violations).toEqual([]);
});

test('selecting another asset clears the previous list', async ({ page }) => {
	// A list that followed the selection would show one asset's release form on another — which, for paperwork whose
	// whole job is answering "may we use *this*", is the worst possible thing to be wrong about.
	await connect(page, { documents: [attachment()] });
	await open(page);
	await expect(panel(page).getByText('model-release.pdf')).toBeVisible();

	await page.getByRole('gridcell').nth(1).dblclick();
	await expect(panel(page)).toContainText('No paperwork on file');
	await expect(panel(page).getByText('model-release.pdf')).toHaveCount(0);
});

test("the previous asset's paperwork does not flash while the next list loads", async ({
	page
}) => {
	// The reset exists for this window, and an end-state assertion cannot see it: `load()` overwrites the list
	// anyway, so without the clear the only symptom is a moment of showing one asset's release form on another.
	// For paperwork whose entire job is answering "may we use *this*", that moment is the whole problem.
	await connect(page, { documents: [attachment()], slowList: true });
	await open(page);
	await expect(panel(page).getByText('model-release.pdf')).toBeVisible();

	await page.getByRole('gridcell').nth(1).dblclick();

	// Sampled at a point in time, not with a retrying matcher. Playwright's matchers poll until they pass, so
	// `toHaveCount(0)` would succeed once the delayed response arrived and cleared the list either way — it cannot
	// see a transient. The reset happens synchronously when the effect runs, so 150 ms into a 700 ms window the
	// panel is either already empty (correct) or still showing the previous asset's release form (the bug).
	await page.waitForTimeout(150);
	const during = await panel(page).innerText();
	expect(during).not.toContain('model-release.pdf');

	// And then the real answer.
	await expect(panel(page)).toContainText('No paperwork on file');
});
