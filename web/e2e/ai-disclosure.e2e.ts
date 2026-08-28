/**
 * The per-asset AI disclosure, through a real browser against a mocked API (M5b·4, G2).
 *
 * The Rust gate proves who may read it. What exists only here is the marking itself, and three properties of it:
 *
 * - **It is on the asset, not only in the review queue.** Article 50's obligation is that somebody encountering
 *   the content can tell which of it a machine produced; a marking visible only to whoever runs the queue is a
 *   marking the people it exists for never see.
 * - **It names the field and the model, and says nothing about the picture.** An authentic photograph with an
 *   AI-written description is not AI-generated content, and labelling it so would be both wrong and
 *   commercially damaging.
 * - **"Nothing here was written by a model" is an answer.** Somebody may be looking specifically for that, and
 *   an empty panel would leave them wondering whether the request failed.
 *
 * Plus: nothing is fetched until it is opened, and no axe violations in either theme.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';

type Field = {
	key: string;
	value: unknown;
	model: string;
	confidence: number | null;
	reviewed: boolean;
};

function summary(id: string, filename = 'harbour.jpg') {
	return {
		id,
		filename,
		mime: 'image/jpeg',
		bytes: 4096,
		width: 2048,
		height: 1365,
		version_no: 1,
		content_hash: 'a'.repeat(64),
		rights_state: 'allowed',
		provenance_state: 'none',
		tier: 'hot',
		created_at: '2026-08-01T10:00:00Z',
		updated_at: '2026-08-01T10:00:00Z',
		engagement: { favourite: false, watching: false, rating: null, average: null, ratings: 0 }
	};
}

async function connect(
	page: Page,
	options: { fields?: Field[]; status?: number } = {}
): Promise<string[]> {
	/** Every disclosure request, in order — this is what proves nothing was fetched too early. */
	const asked: string[] = [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;

		if (path.endsWith('/ai')) {
			asked.push(path.split('/')[2] ?? '');
			if (options.status) {
				return route.fulfill({ status: options.status, json: { reason: 'no' } });
			}
			return route.fulfill({ json: options.fields ?? [] });
		}
		if (path === '/assets' || path === '/search') {
			return route.fulfill({ json: { items: [summary(ASSET)], total: 1, offset: 0 } });
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path.endsWith('/history')) return route.fulfill({ json: [] });
		if (path.endsWith('/comments')) return route.fulfill({ json: [] });
		if (path.endsWith('/categories')) return route.fulfill({ json: [] });
		if (path.endsWith('/attachments')) return route.fulfill({ json: [] });
		if (path.endsWith('/versions')) return route.fulfill({ json: [] });
		if (path.endsWith('/type')) return route.fulfill({ json: { field_keys: [] } });
		if (path.endsWith('/usage-options')) {
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
			return route.fulfill({
				json: {
					...summary(ASSET),
					values: {},
					technical: {},
					duration_ms: null,
					page_count: null,
					color_space: 'sRGB'
				}
			});
		}
		if (path === '/health') return route.fulfill({ body: 'ok' });
		return route.fulfill({ status: 404, json: {} });
	});

	return asked;
}

/** Opens the first asset. The comments region is the marker that the detail panel has finished rendering. */
async function select(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').nth(0).dblclick();
	await expect(page.getByRole('region', { name: 'Comments' })).toBeVisible();
}

/** Opens the first asset and reveals the disclosure. */
async function open(page: Page) {
	await select(page);
	await page.getByText('Written by AI', { exact: true }).click();
}

test('the disclosure is not fetched until it is opened', async ({ page }) => {
	const asked = await connect(page, {
		fields: [
			{
				key: 'description',
				value: 'Boats at rest before sunrise.',
				model: 'claude-opus-5',
				confidence: 0.71,
				reviewed: false
			}
		]
	});
	await select(page);
	// The detail panel is fully rendered and nothing has been asked for. A panel that fetched eagerly would add a
	// request to every asset anybody clicks.
	await expect(page.getByText('Written by AI', { exact: true })).toBeVisible();
	expect(asked).toEqual([]);

	await page.getByText('Written by AI', { exact: true }).click();
	await expect(page.getByText('Boats at rest before sunrise.')).toBeVisible();
	expect(asked).toEqual([ASSET]);
});

test('it names the field and the model, and says nothing about the picture', async ({ page }) => {
	await connect(page, {
		fields: [
			{
				key: 'description',
				value: 'Boats at rest before sunrise.',
				model: 'claude-opus-5',
				confidence: 0.71,
				reviewed: false
			},
			{
				key: 'alt_text',
				value: 'A harbour at dawn',
				model: 'claude-opus-5',
				confidence: null,
				reviewed: true
			}
		]
	});
	await open(page);

	await expect(page.getByText('description', { exact: true })).toBeVisible();
	await expect(page.getByText('claude-opus-5').first()).toBeVisible();
	await expect(page.getByText('claimed 71%')).toBeVisible();
	await expect(page.getByText('not checked yet')).toBeVisible();
	await expect(page.getByText('checked by a person')).toBeVisible();
	// The grading: the words are marked, the image is not.
	await expect(page.getByText('The image itself is untouched')).toBeVisible();
});

test('an asset a model never touched says so', async ({ page }) => {
	await connect(page, { fields: [] });
	await open(page);
	await expect(page.getByText('Nothing on this asset was written by a model.')).toBeVisible();
});

test('a failed read does not claim the asset is clean', async ({ page }) => {
	// The mistake the history panel made once: an error and "nothing here" stacked together, which reads as a
	// claim the panel is in no position to make.
	await connect(page, { status: 500 });
	await open(page);
	await expect(page.getByRole('alert')).toBeVisible();
	await expect(page.getByText('Nothing on this asset was written by a model.')).toBeHidden();
});

for (const theme of ['light', 'dark'] as const) {
	test(`the disclosure has no axe violations in ${theme}`, async ({ page }) => {
		await connect(page, {
			fields: [
				{
					key: 'description',
					value: 'Boats at rest before sunrise.',
					model: 'claude-opus-5',
					confidence: 0.71,
					reviewed: false
				}
			]
		});
		await page.emulateMedia({ colorScheme: theme });
		await open(page);
		await expect(page.getByText('Boats at rest before sunrise.')).toBeVisible();

		const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
		expect(results.violations).toEqual([]);
	});
}
