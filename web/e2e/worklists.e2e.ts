/**
 * The worklists through a real browser, against a mocked API (Q.20, Q.2c·3).
 *
 * `dam_db` proves the eight SQL conditions and `dam-api` proves the contract. What lives only here are four
 * properties of the *interface*:
 *
 * - **Consequence outranks size.** Three of these lists are rights exposure and the rest are tidiness, so the
 *   page leads with the urgent ones however small they are. A page sorted by count would bury "served past its
 *   expiry date" under a thousand missing captions.
 * - **An empty list stays visible and goes quiet.** Hiding a zero would make the page shorter and leave nobody
 *   able to tell "nothing expired" from "we do not check for expiry".
 * - **A zero row is not a link.** A link to an empty page is a dead end that looks like a destination.
 * - **The explanation comes from the server.** The label and the sentence live beside the SQL that decides the
 *   list, so the detail page shows what it was given rather than a second copy that drifts.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Row = {
	key: string;
	label: string;
	explanation: string;
	count: number;
	urgent: boolean;
};

function rows(overrides: Partial<Record<string, number>> = {}): Row[] {
	const base: [string, string, boolean][] = [
		['expired', 'Past its scheduled expiry', true],
		['rights-expiring', 'Licence coverage ending', true],
		['rights-denied', 'Use not permitted', true],
		['expiring-soon', 'Scheduled to expire within 30 days', false],
		['no-licence', 'No licence recorded', false],
		['missing-required', 'Missing required metadata', false],
		['uncategorised', 'In no category', false],
		['embargoed', 'Not released yet', false],
		['enrichment-failed', 'Enrichment failed', false],
		['no-thumbnail', 'No thumbnail', false]
	];
	return base.map(([key, label, urgent]) => ({
		key,
		label,
		urgent,
		explanation: `What being on the ${label} list means, and what somebody should do about it.`,
		count: overrides[key] ?? 0
	}));
}

function asset(index: number) {
	return {
		id: `4444444${index}-4444-4444-8444-444444444444`,
		filename: `unfiled-${index}.jpg`,
		mime: 'image/jpeg',
		bytes: 4096,
		width: 800,
		height: 600,
		tier: 'hot',
		rights_state: 'unknown',
		provenance_state: 'none',
		thumbnail_url: null,
		tag_confidence: null,
		is_favourite: false,
		average_stars: null,
		has_attachment: false,
		published_at: null
	};
}

async function connect(
	page: Page,
	options: { lists?: Row[]; items?: number; notFound?: boolean } = {}
) {
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;

		if (path === '/worklists') {
			return route.fulfill({ json: options.lists ?? rows({ uncategorised: 3, expired: 1 }) });
		}
		if (path.startsWith('/worklists/')) {
			if (options.notFound) {
				return route.fulfill({ status: 404, json: { reason: 'no such worklist' } });
			}
			const count = options.items ?? 3;
			return route.fulfill({
				json: {
					did_you_mean: null,
					items: Array.from({ length: count }, (_, index) => asset(index)),
					total: count,
					offset: 0,
					ranked: false
				}
			});
		}
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		return route.fulfill({ json: {} });
	});
}

test('the rights exposures lead, however small they are', async ({ page }) => {
	// One expired asset outranks a thousand uncategorised ones, because one is a licence somebody is breaching
	// and the other is filing. A page ordered by count would have it exactly backwards.
	await connect(page, { lists: rows({ expired: 1, uncategorised: 1000 }) });
	await page.goto('/worklists');

	// Awaited into existence first: `allTextContents` is a snapshot with no auto-wait, so reading it straight
	// after `goto` samples the page before the fetch resolves.
	await expect(page.getByRole('heading', { level: 2 })).toHaveCount(10);
	const headings = await page.getByRole('heading', { level: 2 }).allTextContents();
	expect(headings[0]).toBe('Past its scheduled expiry');
	expect(headings.indexOf('In no category')).toBeGreaterThan(
		headings.indexOf('No licence recorded')
	);
	await expect(page.getByText('rights exposure').first()).toBeVisible();
});

test('an unlicensed library is not one long alarm', async ({ page }) => {
	// Every asset arrives with no licence, so on a new tenant this list *is* the library. Badging it as an
	// exposure would outline the whole page in red from day one, and a signal that fires on every row is
	// background. Seen on the dev library, where it read 180 of 182.
	await connect(page, { lists: rows({ 'no-licence': 180, 'rights-expiring': 3 }) });
	await page.goto('/worklists');

	await expect(page.getByTestId('count-no-licence')).toHaveText('180');
	// Listed, linked, explained — and not badged. The one badge belongs to the three whose contracts are
	// running out, which is a change rather than an absence.
	await expect(page.getByRole('link', { name: 'No licence recorded' })).toBeVisible();
	await expect(page.getByText('rights exposure')).toHaveCount(1);
	// The wording of the explanation is the server's and is asserted there; what this file can prove is that
	// the page renders whatever it was given rather than a copy of its own.
	await expect(page.getByText('What being on the No licence recorded list means')).toBeVisible();
});

test('an empty list stays on the page and stops being a link', async ({ page }) => {
	await connect(page, { lists: rows({ uncategorised: 3 }) });
	await page.goto('/worklists');

	// Visible: "nothing expired" and "we do not check for expiry" must not look the same.
	await expect(page.getByTestId('count-expired')).toHaveText('0');
	// Not a link: a destination with nothing in it is a dead end.
	await expect(page.getByRole('link', { name: 'Past its scheduled expiry' })).toHaveCount(0);
	await expect(page.getByRole('link', { name: 'In no category' })).toBeVisible();
	// And a zero row carries no exposure badge, however urgent the category is.
	await expect(page.getByText('rights exposure')).toHaveCount(0);
});

test('the counts are stated as the readers own', async ({ page }) => {
	// Two people legitimately see different numbers here. Said out loud, because "Ada sees 40 and I see 12"
	// is otherwise a bug report.
	await connect(page);
	await page.goto('/worklists');

	await expect(page.getByText('Every number is what', { exact: false })).toContainText(
		'somebody with wider access sees larger ones'
	);
});

test('nothing outstanding reads as the good outcome', async ({ page }) => {
	await connect(page, { lists: rows() });
	await page.goto('/worklists');

	await expect(page.getByText('Nothing outstanding on any list', { exact: false })).toContainText(
		'filed, described, licensed and within its dates'
	);
	// The lists are still listed, so the page proves it checked rather than implying it.
	await expect(page.getByRole('heading', { level: 2 })).toHaveCount(10);
});

test('a worklist opens into the grid, with the servers own explanation', async ({ page }) => {
	await connect(page, { items: 3 });
	await page.goto('/worklists');
	await page.getByRole('link', { name: 'In no category' }).click();

	await expect(page.getByRole('heading', { level: 1 })).toHaveText('In no category');
	// From the server, not written into the page: the sentence lives beside the SQL that decides the list.
	await expect(page.getByText('What being on the In no category list means')).toBeVisible();
	await expect(page.getByText('3 assets · longest waiting first')).toBeVisible();
	// The browse grid, so keyboard navigation and virtualisation are the proven ones.
	await expect(page.getByRole('gridcell').first()).toBeVisible();
	await expect(page.getByText('unfiled-0.jpg')).toBeVisible();
});

test('a worklist nobody can see says so rather than looking broken', async ({ page }) => {
	await connect(page, { items: 0 });
	await page.goto('/worklists/uncategorised');

	await expect(page.getByText('Nothing on this list that you can see')).toBeVisible();
});

test('an unknown worklist says it does not exist', async ({ page }) => {
	await connect(page, { notFound: true });
	await page.goto('/worklists/uncategorized');

	// The American spelling is the plausible typo, and it must not quietly show a different backlog.
	await expect(page.getByRole('alert')).toHaveText('No such worklist.');
});

test('licence coverage and a retention date are separate lists', async ({ page }) => {
	// They read different columns and mean different things, and the page must not merge them. The defect that
	// put both here: the first version answered "is anything expiring?" from the asset's retention date, so the
	// grid badged three assets "Expiring" while the worklist named after it reported zero.
	await connect(page, { lists: rows({ 'rights-expiring': 3, 'expiring-soon': 0 }) });
	await page.goto('/worklists');

	await expect(page.getByTestId('count-rights-expiring')).toHaveText('3');
	await expect(page.getByTestId('count-expiring-soon')).toHaveText('0');
	// Licence coverage is the exposure; a retention date is housekeeping, so only one is badged.
	await expect(page.getByText('rights exposure')).toHaveCount(1);
	// And each says which column it read, so nobody has to guess why the numbers differ.
	await expect(page.getByRole('link', { name: 'Licence coverage ending' })).toBeVisible();
});

test('the worklists screens have no axe violations', async ({ page }) => {
	await connect(page, { lists: rows({ expired: 2, uncategorised: 40 }) });
	await page.goto('/worklists');
	await expect(page.getByText('rights exposure').first()).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);

	await page.goto('/worklists/uncategorised');
	await expect(page.getByRole('gridcell').first()).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
