/**
 * Tag vocabularies through a real browser, against a mocked API (Q.20b).
 *
 * `dam_db` proves the gate and the lifecycle; `dam-api` proves the contract. What lives only here are five
 * properties of the *interface*, and each is a decision somebody could reasonably have made differently:
 *
 * - **The machine-tagging switch says what it costs.** Ticking it puts every live term into the prompt of
 *   every enrichment call. A checkbox labelled "ai taggable" would tell nobody that.
 * - **A retired term stays on screen**, greyed, with what it merged into. Hiding it would make the list
 *   shorter and leave nobody able to answer "what happened to that tag" — the question a retirement creates.
 * - **A retired term offers no actions.** Nothing can be assigned to it, so Edit and Merge would be controls
 *   for nothing.
 * - **The threshold shown is the stored one.** It is clamped server-side, so echoing the typed value would
 *   show an operator a setting that is not in force.
 * - **There is no delete button**, because there is no delete endpoint: `asset_tags` cascades.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const VOCAB = '55555555-5555-4555-8555-555555555555';
const LIVE = '66666666-6666-4666-8666-666666666666';
const RETIRED = '77777777-7777-4777-8777-777777777777';

type Term = {
	id: string;
	path: string;
	slug: string;
	label: string;
	synonyms: string[];
	ai_threshold: number;
	ai_precision: number | null;
	asset_count: number;
	deprecated_at: string | null;
	superseded_by: string | null;
};

function term(overrides: Partial<Term> = {}): Term {
	return {
		id: LIVE,
		path: 'overcast',
		slug: 'overcast',
		label: 'Overcast',
		synonyms: ['cloudy'],
		ai_threshold: 0.35,
		ai_precision: null,
		asset_count: 4,
		deprecated_at: null,
		superseded_by: null,
		...overrides
	};
}

function vocabulary(overrides: Record<string, unknown> = {}) {
	return {
		id: VOCAB,
		key: 'moods',
		label: 'Moods',
		ai_taggable: false,
		term_count: 2,
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		vocabularies?: Record<string, unknown>[];
		terms?: Term[];
		/** What PATCH answers with, for the clamp case. */
		amended?: Term;
		retireRefusal?: string;
	} = {}
) {
	const recorder = { ai: [] as boolean[], patched: [] as unknown[], merged: [] as unknown[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;
		const method = route.request().method();

		if (path === '/vocabularies' && method === 'GET') {
			return route.fulfill({ json: options.vocabularies ?? [vocabulary()] });
		}
		if (path.endsWith('/ai')) {
			const body = route.request().postDataJSON() as { ai_taggable: boolean };
			recorder.ai.push(body.ai_taggable);
			return route.fulfill({ json: vocabulary({ ai_taggable: body.ai_taggable }) });
		}
		if (path.endsWith('/terms') && method === 'GET') {
			return route.fulfill({
				json: options.terms ?? [
					term(),
					term({
						id: RETIRED,
						path: 'gloomy',
						slug: 'gloomy',
						label: 'Gloomy',
						synonyms: [],
						asset_count: 0,
						deprecated_at: '2026-08-20T09:00:00Z',
						superseded_by: LIVE
					})
				]
			});
		}
		if (path.endsWith('/retire')) {
			if (options.retireRefusal) {
				return route.fulfill({ status: 409, json: { reason: options.retireRefusal } });
			}
			return route.fulfill({ json: term({ deprecated_at: '2026-08-23T09:00:00Z' }) });
		}
		if (path.endsWith('/merge')) {
			recorder.merged.push(route.request().postDataJSON());
			return route.fulfill({
				json: term({ deprecated_at: '2026-08-23T09:00:00Z', superseded_by: LIVE })
			});
		}
		if (method === 'PATCH') {
			recorder.patched.push(route.request().postDataJSON());
			// The server clamps, so the reply carries what was stored rather than what was sent.
			return route.fulfill({ json: options.amended ?? term({ ai_threshold: 1 }) });
		}
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('the machine-tagging switch says what it costs, in both directions', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/vocabularies');

	// Closed, and the sentence is about what opening it would do — not what the field is called.
	await expect(page.getByText('Opening it adds 2 terms to every enrichment prompt')).toBeVisible();
	await page.getByRole('button', { name: 'Open to machine tagging' }).click();

	await expect(page.getByRole('status')).toContainText('2 terms in every enrichment prompt');
	await expect(page.getByText('All 2 live terms sit in the prompt')).toBeVisible();
	// And the reassurance that matters when closing: existing tags are not withdrawn.
	await expect(page.getByText('tags already applied stay')).toBeVisible();
	expect(recorder.ai).toEqual([true]);
});

test('an open vocabulary with nothing live says so rather than counting zero', async ({ page }) => {
	// It happens for real: retire or merge away every term and this is exactly what is left. "Every one of
	// these 0 terms is in the prompt" reads as a glitch.
	await connect(page, { vocabularies: [vocabulary({ ai_taggable: true, term_count: 0 })] });
	await page.goto('/vocabularies');

	await expect(page.getByText('there is nothing live to suggest')).toBeVisible();
});

test('a retired term stays visible, says where it went, and offers nothing', async ({ page }) => {
	await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();

	// Visible and labelled: "what happened to that tag" is the question a retirement creates.
	const retired = page.locator('ul ul li').filter({ hasText: 'Gloomy' });
	await expect(retired).toContainText('retired, now Overcast');
	// No actions: nothing can be assigned to it, so Edit and Merge would be controls for nothing.
	await expect(retired.getByRole('button')).toHaveCount(0);

	// The live one has all three. Selected by *having* the buttons rather than by its label, because the
	// retired row's "retired, now Overcast" contains the live term's name — a filter on the text matches both
	// rows. Worth writing down: the same trap caught a hand-driven run of this screen.
	const live = page
		.locator('ul ul li')
		.filter({ has: page.getByRole('button', { name: 'Edit' }) })
		.first();
	await expect(live).toContainText('Overcast');
	await expect(live.getByRole('button', { name: 'Merge…' })).toBeVisible();
	await expect(live.getByRole('button', { name: 'Retire' })).toBeVisible();
});

test('the threshold shown is the stored one, not the typed one', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();

	const live = page
		.locator('ul ul li')
		.filter({ has: page.getByRole('button', { name: 'Edit' }) })
		.first();
	await live.getByRole('button', { name: 'Edit' }).click();
	// A typo that expresses a real intention: "never auto-apply".
	await page.getByLabel('Applies above').fill('1.5');
	await page.getByRole('button', { name: 'Save' }).click();

	// 1, because that is what governs. Showing 1.5 would show a setting that is not in force.
	await expect(page.getByRole('status')).toContainText('applying above 1');
	await expect(page.getByText('applies above 1', { exact: false })).toBeVisible();
	expect((recorder.patched[0] as Record<string, unknown>).ai_threshold).toBe(1.5);
	// And the slug was not sent at all: it is what a model answers with.
	expect(recorder.patched[0]).not.toHaveProperty('slug');
});

test('the edit form states that the slug is not going to change', async ({ page }) => {
	await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();
	await page
		.locator('ul ul li')
		.filter({ has: page.getByRole('button', { name: 'Edit' }) })
		.first()
		.getByRole('button', { name: 'Edit' })
		.click();

	await expect(page.getByText('The slug stays overcast')).toContainText('what an import resolves');
	// And what the threshold actually decides, which is the difference between suggested and applied.
	await expect(page.getByText('a tag is suggested for review rather than applied')).toBeVisible();
});

test('merging offers only live terms and says what moves', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();

	const live = page
		.locator('ul ul li')
		.filter({ has: page.getByRole('button', { name: 'Merge…' }) })
		.first();
	await live.getByRole('button', { name: 'Merge…' }).click();

	// Only live terms are targets, and not the term being merged — so with one live term and one retired one
	// there is nothing to choose, which is itself the honest state.
	const options = await page.getByLabel('Merge into').locator('option').allTextContents();
	expect(options).toEqual(['Choose a term…']);
	// The consequence is stated before the button can be used: assets move, and the id keeps resolving.
	await expect(page.getByText('4 assets move to the chosen term')).toContainText(
		'integrations holding its id keep working'
	);
	await expect(page.getByRole('button', { name: 'Merge', exact: true })).toBeDisabled();
	expect(recorder.merged).toHaveLength(0);
});

test('a refused retirement shows the servers own count', async ({ page }) => {
	await connect(page, {
		retireRefusal:
			'term 66666666-6666-4666-8666-666666666666 has 3 live child term(s); retire them first',
		terms: [term()]
	});
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();
	await page.getByRole('button', { name: 'Retire' }).click();

	// The count is the actionable part: it says how much work retiring this really is.
	await expect(page.getByRole('alert')).toContainText('3 live child term(s)');
});

test('there is no way to delete a term from the screen', async ({ page }) => {
	await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();

	// Not an omission: `asset_tags` cascades, so a delete would untag every asset that carried the term.
	await expect(page.getByRole('button', { name: /Delete|Remove/ })).toHaveCount(0);
});

test('the vocabularies screen has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/vocabularies');
	await page.getByRole('button', { name: 'Terms' }).click();
	await expect(page.locator('ul ul li')).toHaveCount(2);
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);

	// And with both editors open, which is where the label associations are easiest to get wrong.
	await page
		.locator('ul ul li')
		.filter({ has: page.getByRole('button', { name: 'Edit' }) })
		.first()
		.getByRole('button', { name: 'Edit' })
		.click();
	await expect(page.getByLabel('Applies above')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
