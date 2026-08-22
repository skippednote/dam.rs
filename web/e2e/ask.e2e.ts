/**
 * Asking a question in the search box, through a real browser against a mocked API (M5d).
 *
 * The Rust gate proves the parser, the switch and the cap. What exists only here are four properties of the
 * *interface*, and they are the reason this feature is a query rather than an answer:
 *
 * - **The button appears only when the tenant has turned it on.** It costs money per press; an affordance that
 *   answers 422 is worse than no affordance.
 * - **The query lands in the box.** Somebody sees what was understood, can edit it, and the results come from
 *   the ordinary search path — not a second retrieval route into a governed library.
 * - **A question that did not become a query leaves the question alone** and says why. Searching the words is
 *   one keystroke away and costs nothing; replacing what they typed with an unusable query is not.
 * - **A spend cap reads as a spend cap**, not as a broken search.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';

type Recorder = { asked: string[]; searched: string[] };

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
	options: {
		enabled?: boolean;
		answer?: Record<string, unknown>;
		status?: number;
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = { asked: [], searched: [] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;
		const method = route.request().method();

		if (path === '/search/ask' && method === 'POST') {
			const body = route.request().postDataJSON() as { question: string };
			recorder.asked.push(body.question);
			if (options.status) {
				return route.fulfill({
					status: options.status,
					json: { reason: 'the tenant is over its hard AI spend cap for this month' }
				});
			}
			return route.fulfill({
				json: options.answer ?? {
					shorthand: 'harbour dawn',
					explanation: 'Assets mentioning both words.',
					confidence: 0.8,
					parses: true,
					problem: null,
					model: 'claude-opus-5'
				}
			});
		}
		if (path === '/ai/enrichment') {
			return route.fulfill({
				json: {
					is_enabled: false,
					guidance: '',
					language: 'English',
					model: null,
					alt_text_field: 'alt_text',
					description_field: 'description',
					suggest_tags: true,
					natural_language_search: options.enabled ?? false
				}
			});
		}
		if (path === '/search' || path === '/assets') {
			recorder.searched.push(url.searchParams.get('q') ?? '');
			return route.fulfill({
				json: { items: [summary(ASSET)], total: 1, offset: 0, ranked: true }
			});
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path === '/health') return route.fulfill({ body: 'ok' });
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the button appears only when the tenant has turned asking on', async ({ page }) => {
	await connect(page, { enabled: false });
	await page.goto('/assets');
	await expect(page.getByRole('button', { name: 'Search' })).toBeVisible();
	await expect(page.getByRole('button', { name: 'Ask' })).toBeHidden();

	await connect(page, { enabled: true });
	await page.goto('/assets');
	await expect(page.getByRole('button', { name: 'Ask' })).toBeVisible();
});

test('the query lands in the box, and the search that follows is the ordinary one', async ({
	page
}) => {
	const recorder = await connect(page, { enabled: true });
	await page.goto('/assets');
	await expect(page.getByRole('button', { name: 'Ask' })).toBeVisible();

	await page.getByLabel('Search assets').fill('photos of the harbour at dawn');
	await page.getByRole('button', { name: 'Ask' }).click();

	// The question went once; the box now holds the query.
	expect(recorder.asked).toEqual(['photos of the harbour at dawn']);
	await expect(page.getByLabel('Search assets')).toHaveValue('harbour dawn');
	// And what was understood is said, so a wrong query is correctable rather than mysterious.
	await expect(page.getByText('Understood as:')).toContainText('Assets mentioning both words');
	// The results came from the ordinary search path with the query in it.
	expect(recorder.searched.at(-1)).toBe('harbour dawn');
});

test('a question that did not become a query leaves the question alone', async ({ page }) => {
	await connect(page, {
		enabled: true,
		answer: {
			shorthand: 'campaign:spring',
			explanation: 'Assets in the spring campaign.',
			confidence: 0.9,
			parses: false,
			problem: { message: 'no such field: campaign', code: 'unknown_field', at: 1 },
			model: 'claude-opus-5'
		}
	});
	await page.goto('/assets');
	await expect(page.getByRole('button', { name: 'Ask' })).toBeVisible();

	await page.getByLabel('Search assets').fill('spring campaign assets');
	await page.getByRole('button', { name: 'Ask' }).click();

	// By its own text rather than by role: the grid already carries live regions for the result count, and
	// `getByRole('status')` would match those too.
	const note = page.getByText('did not become a query');
	await expect(note).toContainText('no such field: campaign');
	await expect(note).toContainText('Press Search to look for the words instead');
	// Their words are still there: searching them costs nothing, and an unusable query replacing what they
	// typed would lose it.
	await expect(page.getByLabel('Search assets')).toHaveValue('spring campaign assets');
});

test('a spend cap reads as a spend cap', async ({ page }) => {
	await connect(page, { enabled: true, status: 429 });
	await page.goto('/assets');
	await expect(page.getByRole('button', { name: 'Ask' })).toBeVisible();
	await page.getByLabel('Search assets').fill('anything');
	await page.getByRole('button', { name: 'Ask' }).click();
	await expect(page.getByRole('alert')).toContainText('spend cap');
});

for (const theme of ['light', 'dark'] as const) {
	test(`asking has no axe violations in ${theme}`, async ({ page }) => {
		await connect(page, { enabled: true });
		await page.emulateMedia({ colorScheme: theme });
		await page.goto('/assets');
		await expect(page.getByRole('button', { name: 'Ask' })).toBeVisible();
		await page.getByLabel('Search assets').fill('photos of the harbour at dawn');
		await page.getByRole('button', { name: 'Ask' }).click();
		await expect(page.getByText('Understood as:')).toBeVisible();

		const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
		expect(results.violations).toEqual([]);
	});
}
