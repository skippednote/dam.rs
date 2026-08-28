/**
 * The landing page through a real browser, against a mocked API.
 *
 * The Rust gate proves the scoping and the counts. What only exists here is the phrasing, and three properties that
 * are decisions about the *interface*:
 *
 * - **A feed line reads as a sentence**, so an unrecognised kind is phrased plainly rather than dropped. Hiding
 *   activity is worse than describing it awkwardly.
 * - **The page says the numbers are the reader's own**, because a scoped reader seeing smaller totals than a
 *   colleague needs to know that is the point rather than a fault.
 * - **A count with nothing to do about it is not a link.** "Nothing written about them" has no selector yet, and a
 *   link that led elsewhere would answer a different question while looking like it answered this one.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

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
		asset_id: 'a-1',
		filename: 'harbour.jpg',
		actor: ADA,
		context: {},
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		counts?: Partial<{
			assets: number;
			uploads_this_week: number;
			downloads_this_week: number;
			comments_this_week: number;
			without_metadata: number;
		}>;
		activity?: Entry[];
		spotlights?: { id: string; name: string; cached_count: number | null; mine: boolean }[];
		status?: number;
		connected?: boolean;
	} = {}
) {
	if (options.connected !== false) {
		await page.addInitScript(() => {
			localStorage.setItem('damrs.api_key', 'damrs_test_key');
			localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
		});
	}

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;
		if (path === '/dashboard') {
			if (options.status) {
				return route.fulfill({
					status: options.status,
					json: { reason: 'the events table is gone' }
				});
			}
			return route.fulfill({
				json: {
					counts: {
						assets: 0,
						uploads_this_week: 0,
						downloads_this_week: 0,
						comments_this_week: 0,
						without_metadata: 0,
						...options.counts
					},
					activity: options.activity ?? [],
					spotlights: options.spotlights ?? []
				}
			});
		}
		return route.fulfill({ status: 404, json: {} });
	});
}

test('an empty library says what will appear rather than showing nothing', async ({ page }) => {
	await connect(page);
	await page.goto('/');

	await expect(page.getByRole('heading', { name: 'Recent activity' })).toBeVisible();
	await expect(page.getByText('Uploads, shares and comments appear here')).toBeVisible();
	await expect(page.getByText('Save a search from the')).toBeVisible();
	// Zeroes are rendered, not hidden: a dashboard that omitted them would be indistinguishable from one that
	// failed to load.
	await expect(page.getByTestId('count-assets')).toContainText('0');
});

test('the page says the numbers belong to the reader', async ({ page }) => {
	await connect(page, { counts: { assets: 12 } });
	await page.goto('/');

	// A scoped reader sees smaller totals than a colleague. Saying so is the difference between a dashboard that
	// looks wrong and one that is understood.
	await expect(page.getByText('what you can see')).toBeVisible();
	await expect(page.getByTestId('count-assets')).toContainText('12');
});

test('each feed kind reads as a sentence', async ({ page }) => {
	await connect(page, {
		activity: [
			entry({ kind: 'upload', filename: 'harbour.jpg' }),
			entry({ kind: 'share', filename: 'quay.jpg' }),
			entry({ kind: 'comment', filename: 'dawn.jpg', context: { visibility: 'public' } }),
			entry({
				kind: 'comment',
				filename: 'dusk.jpg',
				context: { visibility: 'private' },
				id: 'e-private'
			}),
			entry({ kind: 'download', filename: 'pier.jpg' }),
			entry({ kind: 'restore', filename: 'old.jpg' })
		]
	});
	await page.goto('/');

	await expect(page.getByText('Ada Lovelace uploaded harbour.jpg')).toBeVisible();
	await expect(page.getByText('Ada Lovelace shared quay.jpg')).toBeVisible();
	await expect(page.getByText('Ada Lovelace commented on dawn.jpg')).toBeVisible();
	// Said to be private, and that is *all* that is said — the words never reach this page.
	await expect(page.getByText('Ada Lovelace left a private comment on dusk.jpg')).toBeVisible();
	await expect(page.getByText('Ada Lovelace downloaded pier.jpg')).toBeVisible();
	await expect(page.getByText('asked for old.jpg to be restored')).toBeVisible();
});

test('an unrecognised kind is phrased rather than dropped', async ({ page }) => {
	// The events column is deliberately open text, so a future subsystem can record something without a migration.
	// A dashboard that silently skipped what it did not recognise would hide activity.
	await connect(page, {
		activity: [entry({ kind: 'transcoded', filename: 'clip.mp4', id: 'e-odd' })]
	});
	await page.goto('/');

	await expect(page.getByText('Ada Lovelace: transcoded on clip.mp4')).toBeVisible();
});

test('an event with no actor still reads', async ({ page }) => {
	// The system does things, and so do people who have since been deleted. A line has to read without a name
	// rather than showing an empty gap.
	await connect(page, {
		activity: [entry({ kind: 'upload', actor: null, filename: 'batch-001.jpg', id: 'e-noactor' })]
	});
	await page.goto('/');

	await expect(page.getByText('Somebody uploaded batch-001.jpg')).toBeVisible();
});

test('a saved search shows its count as a cached figure', async ({ page }) => {
	await connect(page, {
		spotlights: [
			{ id: 's-1', name: 'Spring campaign', cached_count: 42, mine: true },
			{ id: 's-2', name: 'Everything unlicensed', cached_count: null, mine: false }
		]
	});
	await page.goto('/');

	await expect(page.getByText('Spring campaign')).toBeVisible();
	await expect(page.getByText('· yours')).toBeVisible();
	// "When last counted", never "results": it is computed for nobody in particular, and presenting it as the
	// reader's own count would leak how many assets exist beyond their scope.
	await expect(page.getByText('42 when last counted')).toBeVisible();
	// A search with no cached count shows no number at all rather than a zero, which would be a claim.
	const uncounted = page.getByRole('listitem').filter({ hasText: 'Everything unlicensed' });
	await expect(uncounted).not.toContainText('0');
});

test('the undescribed count is not a link, and the asset count is', async ({ page }) => {
	await connect(page, { counts: { assets: 5, without_metadata: 3 } });
	await page.goto('/');

	// A figure you can act on is a way in — the link is inside the `dd`, because a `dl` may not have an `a` as a
	// direct child and axe rightly refused the first version.
	await expect(page.getByTestId('count-assets').getByRole('link')).toHaveAttribute(
		'href',
		/assets/
	);
	// This one is not, because no selector answers it yet. A link somewhere else would look like an answer.
	await expect(page.getByTestId('count-undescribed')).toContainText('3');
	await expect(page.getByTestId('count-undescribed').locator('a')).toHaveCount(0);
});

test('a failure is reported rather than left as an empty page', async ({ page }) => {
	await connect(page, { status: 500 });
	await page.goto('/');

	await expect(page.getByRole('alert')).toContainText('events table is gone');
});

test('with no key the page says how to connect', async ({ page }) => {
	await connect(page, { connected: false });
	await page.goto('/');

	// Scoped to the page body: the nav also says "Not connected" when there is no key, which is correct in both
	// places and ambiguous to a locator.
	await expect(page.getByText('Not connected. Add an API key')).toBeVisible();
	// In the page, not the nav — the nav links to Settings whether or not there is a key.
	await expect(
		page.locator('#main-content').getByRole('link', { name: 'Settings', exact: true })
	).toBeVisible();
});

test('the dashboard has no accessibility violations', async ({ page }) => {
	await connect(page, {
		counts: {
			assets: 120,
			uploads_this_week: 7,
			downloads_this_week: 31,
			comments_this_week: 4,
			without_metadata: 12
		},
		activity: [
			entry({ kind: 'upload' }),
			entry({ kind: 'comment', context: { visibility: 'private' }, id: 'e-p' })
		],
		spotlights: [{ id: 's-1', name: 'Spring campaign', cached_count: 42, mine: true }]
	});
	await page.goto('/');
	await expect(page.getByRole('heading', { name: 'Recent activity' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
