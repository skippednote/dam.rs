/**
 * The review queue, through a real browser against a mocked API (M5b).
 *
 * The Rust suites prove the predicate, the state machine and the feedback rows. What belongs here is the part
 * only a browser can show:
 *
 * - **Agreement is what leads**, and the badge says which kind of evidence a reviewer is looking at — two
 *   generators agreeing reads differently from one model claiming 95%.
 * - **No is a button, not a dismissal.** Both verdicts are one click and both are recorded.
 * - **A decided tag leaves the queue without it reordering underneath.** A reviewer working down a long list
 *   should not have the ground move after every click.
 * - **A stale click says so.** Two people with the same queue open is ordinary, and the second one deserves an
 *   explanation rather than a silent no-op.
 * - **What a model wrote is shown with the model that wrote it** — that marking is the disclosure, not a detail.
 * - **No axe violations, in both themes.**
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Tag = {
	term_id: string;
	slug: string;
	label: string;
	confidence: number | null;
	votes: number;
	source: string;
};

type Row = {
	asset_id: string;
	filename: string;
	mime: string;
	suggested: Tag[];
	fields: {
		key: string;
		value: unknown;
		model: string;
		confidence: number | null;
		reviewed: boolean;
	}[];
};

function tag(slug: string, overrides: Partial<Tag> = {}): Tag {
	return {
		term_id: `term-${slug}`,
		slug,
		label: slug[0].toUpperCase() + slug.slice(1),
		confidence: 0.8,
		votes: 1,
		source: 'llm',
		...overrides
	};
}

function row(overrides: Partial<Row> = {}): Row {
	return {
		asset_id: 'asset-1',
		filename: 'SS26_lookbook_cover.jpg',
		mime: 'image/jpeg',
		suggested: [tag('footwear')],
		fields: [
			{
				key: 'alt_text',
				value: 'A runner on a wet path at dusk',
				model: 'claude-opus-5',
				confidence: 0.71,
				reviewed: false
			}
		],
		...overrides
	};
}

type Recorder = { decisions: { path: string; accept: boolean }[] };

async function connect(
	page: Page,
	options: { queue?: Row[]; decideStatus?: number } = {}
): Promise<Recorder> {
	const recorder: Recorder = { decisions: [] };
	const queue = options.queue ?? [row()];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/ai/review' && method === 'GET') {
			return route.fulfill({ json: queue });
		}
		if (url.pathname.includes('/tags/') && method === 'PATCH') {
			const body = route.request().postDataJSON() as { accept: boolean };
			recorder.decisions.push({ path: url.pathname, accept: body.accept });
			return route.fulfill({ status: options.decideStatus ?? 204, body: '' });
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the queue says what kind of evidence each suggestion has', async ({ page }) => {
	await connect(page, {
		queue: [
			row({
				suggested: [
					tag('footwear', { votes: 3, confidence: 0.4 }),
					tag('outdoor', { votes: 1, confidence: 0.95 })
				]
			})
		]
	});
	await page.goto('/review');

	// Agreement, not a self-reported number — the badge is the difference.
	await expect(page.getByText('3 generators agree')).toBeVisible();
	await expect(page.getByText('claimed 95%')).toBeVisible();
	await expect(page.getByText('2 suggestions across 1 asset.')).toBeVisible();
});

test('what a model wrote is shown with the model that wrote it', async ({ page }) => {
	await connect(page);
	await page.goto('/review');
	await expect(page.getByText('A runner on a wet path at dusk')).toBeVisible();
	// The marking is the disclosure. A value shown without it would be indistinguishable from a person's.
	await expect(page.getByText('claude-opus-5')).toBeVisible();
	await expect(page.getByText('alt_text')).toBeVisible();
});

test('yes and no are both one click and both recorded', async ({ page }) => {
	const recorder = await connect(page, {
		queue: [row({ suggested: [tag('footwear'), tag('outdoor')] })]
	});
	await page.goto('/review');

	await page.getByRole('button', { name: /^Confirm Footwear/ }).click();
	await expect(page.getByRole('status')).toContainText('Confirmed Footwear');

	await page.getByRole('button', { name: /^Reject Outdoor/ }).click();
	await expect(page.getByRole('status')).toContainText('Rejected Outdoor');

	expect(recorder.decisions).toEqual([
		{ path: '/assets/asset-1/tags/term-footwear', accept: true },
		{ path: '/assets/asset-1/tags/term-outdoor', accept: false }
	]);
});

test('a decided tag leaves the list without the rest reordering', async ({ page }) => {
	await connect(page, {
		queue: [row({ suggested: [tag('footwear'), tag('outdoor'), tag('studio')] })]
	});
	await page.goto('/review');
	await expect(page.getByText('3 suggestions across 1 asset.')).toBeVisible();

	await page.getByRole('button', { name: /^Confirm Outdoor/ }).click();

	await expect(page.getByText('2 suggestions across 1 asset.')).toBeVisible();
	// Its buttons are gone, which is the assertion that means "left the list" — the word itself is still on
	// screen in the confirmation message, and asserting on that would pass for the wrong reason.
	await expect(page.getByRole('button', { name: /^Confirm Outdoor/ })).toBeHidden();
	// The others are where they were: the mock would have returned the full list again on a reload, so this
	// also proves the screen did not reload.
	expect(await page.getByRole('button', { name: /^Confirm Footwear/ }).count()).toBe(1);
	await expect(page.getByText('Studio')).toBeVisible();
});

test('a stale click is explained rather than silently ignored', async ({ page }) => {
	// Two reviewers with the same queue open. The server says 404 because there was nothing suggested left to
	// decide, and the person clicking deserves to know why nothing happened.
	await connect(page, { decideStatus: 404 });
	await page.goto('/review');
	await page.getByRole('button', { name: /^Confirm Footwear/ }).click();
	await expect(page.getByRole('alert')).toContainText('already decided');
});

test('an empty queue says which of the two reasons it is empty for', async ({ page }) => {
	await connect(page, { queue: [] });
	await page.goto('/review');
	await expect(page.getByText('Either no model has run yet')).toBeVisible();
});

for (const theme of ['light', 'dark'] as const) {
	test(`the review queue has no axe violations in ${theme}`, async ({ page }) => {
		await connect(page, {
			queue: [
				row({ suggested: [tag('footwear', { votes: 2 }), tag('outdoor')] }),
				row({
					asset_id: 'asset-2',
					filename: 'campaign_hero.png',
					mime: 'image/png',
					suggested: [tag('studio', { confidence: null })],
					fields: []
				})
			]
		});
		await page.emulateMedia({ colorScheme: theme });
		await page.goto('/review');
		await expect(page.getByText('campaign_hero.png')).toBeVisible();

		const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
		expect(results.violations).toEqual([]);
	});
}
