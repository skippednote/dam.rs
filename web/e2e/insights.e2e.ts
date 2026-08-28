/**
 * Insights through a real browser (M6c, §8.1).
 *
 * The Rust suites prove the queries, the scoping and the exports. What lives only here are properties of the
 * *screen*:
 *
 * - **The window shown is the window the server used.** A request for ten years comes back as 366 days, and
 *   labelling the chart with what was asked for would be wrong.
 * - **A quiet day is visible as a quiet day.** The spine comes back with zeroes in it, and the chart draws a
 *   baseline tick rather than nothing — a gap that looks like missing data is the failure this avoids.
 * - **The chart is decorative and the table is the data.** The SVG is `aria-hidden`, which is only honest
 *   because every figure in it is also in the table beneath — the same rows, not a summary.
 * - **The page says the numbers are the reader's own**, because "Ada says 1,240 and I see 12" is otherwise a
 *   bug report.
 * - **The contributors list says it is not a performance measure**, because somebody will otherwise use it as
 *   one.
 * - **An export goes through the API key**, not a plain link, and carries the chosen window.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Day = {
	day: string;
	uploads: number;
	downloads: number;
	edits: number;
	comments: number;
	shares: number;
};

function quiet(day: string): Day {
	return { day, uploads: 0, downloads: 0, edits: 0, comments: 0, shares: 0 };
}

function busy(day: string, downloads: number): Day {
	return { day, uploads: 2, downloads, edits: 1, comments: 1, shares: 0 };
}

const SEVEN: Day[] = [
	busy('2026-08-17', 4),
	quiet('2026-08-18'),
	quiet('2026-08-19'),
	quiet('2026-08-20'),
	quiet('2026-08-21'),
	quiet('2026-08-22'),
	busy('2026-08-23', 6)
];

type Insights = {
	days: number;
	series: Day[];
	most_downloaded: {
		asset_id: string;
		filename: string;
		mime: string;
		count: number;
		last_at: string | null;
	}[];
	never_downloaded: {
		asset_id: string;
		filename: string;
		mime: string;
		count: number;
		last_at: string | null;
	}[];
	never_downloaded_total: number;
	by_class: { class: string; assets: number; bytes: number }[];
	contributors: {
		person: { id: string; name: string; email: string };
		uploads: number;
		edits: number;
		comments: number;
	}[];
};

function insights(overrides: Partial<Insights> = {}): Insights {
	return {
		days: 7,
		series: SEVEN,
		most_downloaded: [
			{
				asset_id: 'a1111111-1111-4111-8111-111111111111',
				filename: 'harbour.jpg',
				mime: 'image/jpeg',
				count: 8,
				last_at: '2026-08-23T09:00:00Z'
			},
			{
				asset_id: 'b1111111-1111-4111-8111-111111111111',
				filename: 'quayside.jpg',
				mime: 'image/jpeg',
				count: 2,
				last_at: '2026-08-17T09:00:00Z'
			}
		],
		never_downloaded: [
			{
				asset_id: 'c1111111-1111-4111-8111-111111111111',
				filename: 'forgotten-2019.tif',
				mime: 'image/tiff',
				count: 0,
				last_at: null
			}
		],
		never_downloaded_total: 1,
		by_class: [
			{ class: 'video', assets: 3, bytes: 900_000_000 },
			{ class: 'image', assets: 120, bytes: 10_000_000 }
		],
		contributors: [
			{
				person: { id: 'p-ada', name: 'Ada', email: 'ada@example.com' },
				uploads: 12,
				edits: 3,
				comments: 5
			}
		],
		...overrides
	};
}

async function connect(page: Page, options: { body?: Insights; refuse?: number } = {}) {
	const recorder = { windows: [] as number[], exports: [] as string[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname === '/insights/export.csv') {
			// Recorded with its window, because an export that ignored the chosen range would silently be a
			// file about a different period than the page it came from.
			recorder.exports.push(url.search);
			// The header matters: a fetch that came back as JSON with a .csv name is the failure mode.
			return route.fulfill({
				headers: {
					'content-type': 'text/csv; charset=utf-8',
					'content-disposition': 'attachment; filename="activity.csv"'
				},
				body: 'day,uploads,downloads,edits,comments,shares\n2026-08-23,2,6,1,1,0\n'
			});
		}
		if (url.pathname === '/insights') {
			if (options.refuse) {
				return route.fulfill({ status: options.refuse, json: { reason: 'refused' } });
			}
			const asked = Number(url.searchParams.get('days') ?? 30);
			recorder.windows.push(asked);
			// The server clamps, and the screen must show what came back rather than what it asked for.
			const clamped = Math.min(Math.max(asked, 1), 366);
			const body = options.body ?? insights();
			return route.fulfill({ json: { ...body, days: clamped } });
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'ada@example.com' } });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the totals are the reader’s own, and the page says so', async ({ page }) => {
	await connect(page);
	await page.goto('/insights');

	await expect(page.getByText('counts only the assets', { exact: false })).toContainText(
		'somebody with wider access sees larger ones'
	);
	// No library-wide figure anywhere: the sentence explains its own absence.
	await expect(page.getByText('there is no library-wide total on this page')).toBeVisible();

	// Summed from the series rather than taken from a field the server does not send.
	await expect(page.getByRole('definition').filter({ hasText: '10' }).first()).toBeVisible();

	// One row per kind, each with its own peak stated — because five scales that are not comparable to each
	// other are only honest if the page says what each one is.
	const lines = page.getByTestId('sparklines').locator('li');
	await expect(lines).toHaveCount(5);
	await expect(lines.nth(1)).toContainText('Downloads');
	await expect(lines.nth(1)).toContainText('peak 6/day');
	await expect(lines.nth(0)).toContainText('peak 2/day');
});

test('the window shown is the window the server used', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/insights');
	await expect(page.getByTestId('window')).toHaveText('last 30 days');

	await page.getByRole('button', { name: 'a year' }).click();
	await expect(page.getByTestId('window')).toHaveText('last 366 days');
	expect(recorder.windows).toEqual([30, 366]);

	// Pressed state, not just a colour: a button group whose selection is only visible is a group a screen
	// reader cannot report.
	await expect(page.getByRole('button', { name: 'a year' })).toHaveAttribute(
		'aria-pressed',
		'true'
	);
	await expect(page.getByRole('button', { name: '30 days' })).toHaveAttribute(
		'aria-pressed',
		'false'
	);
});

test('a quiet week reads as quiet rather than as missing data', async ({ page }) => {
	await connect(page);
	await page.goto('/insights');

	// Seven rows for seven days. The five empty ones are present, which is what stops the chart drawing a
	// straight line across them.
	await page.getByText('The same figures as a table', { exact: false }).click();
	const rows = page.getByTestId('series').locator('tbody tr');
	await expect(rows).toHaveCount(7);
	await expect(rows.nth(3).locator('th')).toHaveText('2026-08-20');
	await expect(rows.nth(3).locator('td').first()).toHaveText('0');
});

test('the chart is hidden from screen readers and the table carries every figure', async ({
	page
}) => {
	await connect(page);
	await page.goto('/insights');

	// Hiding the picture is only honest because the numbers are all somewhere else.
	await expect(page.getByTestId('sparklines').locator('svg[aria-hidden="true"]')).toHaveCount(5);
	await page.getByText('The same figures as a table', { exact: false }).click();

	// Every day, and every one of the five counts.
	const table = page.getByTestId('series');
	await expect(table.locator('thead th')).toHaveCount(6);
	await expect(table.getByText('2026-08-23')).toBeVisible();
	const last = table.locator('tbody tr').last();
	await expect(last.locator('td')).toHaveText(['2', '6', '1', '1', '0']);
});

test('the lists say which question they answer', async ({ page }) => {
	await connect(page);
	await page.goto('/insights');

	await expect(page.getByTestId('most-downloaded').getByText('harbour.jpg')).toBeVisible();
	await expect(page.getByTestId('never-downloaded').getByText('forgotten-2019.tif')).toBeVisible();
	// "Ever, not in this window" is the whole point of the second list, and it is easy to assume otherwise.
	await expect(page.getByText('Never taken by anybody, ever', { exact: false })).toContainText(
		'not just in this window'
	);
	await expect(page.getByText('Oldest first', { exact: false })).toBeVisible();

	// Storage by class, largest first, with a readable size rather than a byte count.
	const classes = page.getByTestId('by-class').locator('li');
	await expect(classes.first()).toContainText('video');
	await expect(classes.first()).toContainText('858 MiB');
});

test('a capped unused list says how many there really are', async ({ page }) => {
	// Twenty rows here read as "we have twenty unused assets". On the dev library that was twenty of a much
	// larger number, which is the difference between a tidy-up and a storage problem.
	await connect(page, { body: insights({ never_downloaded_total: 1_340 }) });
	await page.goto('/insights');
	await expect(page.getByText('Showing the 1 oldest of', { exact: false })).toContainText('1,340');
});

test('an unused list that is complete does not claim to be a page of one', async ({ page }) => {
	// The sentence appears only when the list is actually cut short: saying "showing 1 of 1" is noise that
	// makes the honest case read like the truncated one.
	await connect(page);
	await page.goto('/insights');
	await expect(page.getByText('Showing the', { exact: false })).toHaveCount(0);
});

test('the contributors list says it is not a performance measure', async ({ page }) => {
	// Somebody will otherwise use it as one, and the numbers change with the reader — so it would be a
	// comparison of how much of each person's work the reader happens to be allowed to see.
	await connect(page);
	await page.goto('/insights');

	await expect(page.getByText('not a measure of anybody', { exact: false })).toContainText(
		'different for every reader'
	);
	await expect(page.getByText('Downloads are deliberately absent', { exact: false })).toBeVisible();

	const table = page.getByTestId('contributors');
	await expect(table.getByText('Ada')).toBeVisible();
	// The email is shown because it says something the name does not. When `display_name` falls back to the
	// email — which it does whenever nobody set one — printing both read "ada@example.com ada@example.com" on
	// the live page.
	await expect(table.getByText('ada@example.com')).toBeVisible();
	// Four columns: person, uploads, edits, comments. No download column to be filled.
	await expect(table.locator('thead th')).toHaveCount(4);
	await expect(table.locator('thead')).not.toContainText('Download');
});

test('an export goes through the API key and carries the chosen window', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/insights');
	await page.getByRole('button', { name: '7 days' }).click();
	await expect(page.getByTestId('window')).toHaveText('last 7 days');

	await page.getByRole('button', { name: 'Export CSV' }).first().click();
	await expect(page.getByRole('status')).toContainText('activity.csv');
	expect(recorder.exports).toEqual(['?report=activity&days=7']);

	// Each section exports its own report, not one file for the page.
	await page.getByRole('button', { name: 'Export CSV' }).nth(1).click();
	expect(recorder.exports[1]).toBe('?report=storage&days=7');
});

test('an empty library says which nothing it means', async ({ page }) => {
	await connect(page, {
		body: insights({
			series: [quiet('2026-08-23')],
			most_downloaded: [],
			never_downloaded: [],
			never_downloaded_total: 0,
			by_class: [],
			contributors: []
		})
	});
	await page.goto('/insights');

	await expect(page.getByText('Nothing stored that you can see')).toBeVisible();
	await expect(page.getByText('Nothing you can see was downloaded in this window')).toBeVisible();
	await expect(
		page.getByText('Everything you can see has been downloaded at least once')
	).toBeVisible();
	await expect(page.getByText('No activity by anybody in this window')).toBeVisible();
});

test('a person with no display name is not printed twice', async ({ page }) => {
	// `display_name` falls back to the email, so name and email are the same string for anybody who never set
	// one — which is most people in a fresh tenant. The live page read "ada@example.com ada@example.com".
	await connect(page, {
		body: insights({
			contributors: [
				{
					person: {
						id: 'p-ada',
						name: 'ada@example.com',
						email: 'ada@example.com'
					},
					uploads: 12,
					edits: 3,
					comments: 5
				}
			]
		})
	});
	await page.goto('/insights');
	const row = page.getByTestId('contributors').locator('tbody tr').first();
	await expect(row.locator('th')).toHaveText('ada@example.com');
});

test('a refusal is a sentence rather than an empty page', async ({ page }) => {
	await connect(page, { refuse: 403 });
	await page.goto('/insights');
	await expect(page.getByRole('alert')).toBeVisible();
});

test('the insights screen has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/insights');
	await expect(page.getByTestId('contributors')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);

	// And with the table open, which is the part a screen-reader user actually reads.
	await page.getByText('The same figures as a table', { exact: false }).click();
	await expect(page.getByTestId('series').locator('tbody tr').first()).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
