/**
 * The tenant's caps (G19).
 *
 * The Rust suites prove the arithmetic, the level/flow distinction and the enforcement. What lives only here are
 * properties of the screen that exists so a cap is never a surprise:
 *
 * - **A level and a flow are labelled differently.** "900 of 1,000" is what exists for storage and what happened
 *   this month for spend, and one bar for both would be misleading about the more alarming one.
 * - **A soft cap over its limit says work continues.** It is the deliberate default — a hard cap on ingest loses
 *   a customer's work — so a row that is over must not read as an outage.
 * - **No caps is stated, and states that absent is not zero.** Otherwise an empty page reads as everything
 *   exhausted.
 * - **The page says nothing here can change a limit**, or read-only fields just look like a missing button.
 * - **Units are part of the meaning.** Bytes render as bytes and cents as currency; a raw integer for either is
 *   a number nobody can act on.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Quota = {
	quota_key: string;
	limit_value: number;
	used: number;
	warn_at_fraction: number;
	enforcement: string;
	standing: string;
	is_level: boolean;
	warned_at: string | null;
	exceeded_at: string | null;
};

function quota(overrides: Partial<Quota> = {}): Quota {
	return {
		quota_key: 'storage_bytes',
		limit_value: 1_099_511_627_776,
		used: 989_560_464_998,
		warn_at_fraction: 0.8,
		enforcement: 'soft',
		standing: 'warned',
		is_level: true,
		warned_at: '2026-08-20T09:00:00Z',
		exceeded_at: null,
		...overrides
	};
}

async function connect(page: Page, options: { quotas?: Quota[]; refuse?: number } = {}) {
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});
	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname === '/quotas') {
			if (options.refuse) {
				return route.fulfill({ status: options.refuse, json: { reason: 'refused' } });
			}
			return route.fulfill({
				json: { period_start: '2026-08-01', quotas: options.quotas ?? [quota()] }
			});
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		return route.fulfill({ status: 404, json: {} });
	});
}

test('the page says a cap cannot be raised from here, and why', async ({ page }) => {
	// Read-only figures with no explanation just look like a missing button.
	await connect(page);
	await page.goto('/settings/limits');
	await expect(page.getByText('Nothing here can be changed', { exact: false })).toContainText(
		'part of the agreement rather than a setting'
	);
});

test('a level and a flow are labelled differently', async ({ page }) => {
	await connect(page, {
		quotas: [
			quota(),
			quota({
				quota_key: 'ai_spend_cents_month',
				limit_value: 50_000,
				used: 41_200,
				is_level: false,
				enforcement: 'hard',
				standing: 'warned'
			})
		]
	});
	await page.goto('/settings/limits');

	await expect(page.getByTestId('cap-storage_bytes')).toContainText('held right now');
	await expect(page.getByTestId('cap-ai_spend_cents_month')).toContainText('used this month');
	// And the units are part of the meaning: bytes as bytes, cents as currency.
	await expect(page.getByTestId('cap-storage_bytes')).toContainText('922 GiB of 1.0 TiB');
	await expect(page.getByTestId('cap-ai_spend_cents_month')).toContainText('412.00 of 500.00');
});

test('a hard cap says work will be refused and a soft cap says it will not', async ({ page }) => {
	await connect(page, {
		quotas: [
			quota({ enforcement: 'hard' }),
			quota({ quota_key: 'asset_count', enforcement: 'soft', limit_value: 100, used: 90 })
		]
	});
	await page.goto('/settings/limits');

	await expect(page.getByTestId('cap-storage_bytes')).toContainText(
		'At the limit, new work is refused'
	);
	await expect(page.getByTestId('cap-asset_count')).toContainText('work continues');
	await expect(page.getByTestId('cap-asset_count')).toContainText('nothing stops');
});

test('a soft cap over its limit does not read as an outage', async ({ page }) => {
	// The deliberate default. A hard cap on ingest loses a customer's work, so `soft` is what an operator gets
	// unless they ask otherwise — and a row that is over must say the work continues.
	await connect(page, {
		quotas: [
			quota({
				enforcement: 'soft',
				standing: 'refused',
				limit_value: 100,
				used: 500,
				warned_at: '2026-08-10T09:00:00Z',
				exceeded_at: null
			})
		]
	});
	await page.goto('/settings/limits');
	const row = page.getByTestId('cap-storage_bytes');
	// "over", not "new work refused" — the enforcement mode decides the wording.
	await expect(row).toContainText('over');
	await expect(row).not.toContainText('new work refused');
	await expect(row).toContainText('nothing stops');
});

test('a hard cap that is over says when it started', async ({ page }) => {
	// "We were not told" has to be answerable, and the tenant is the person who most needs to know they have
	// been over for three weeks.
	await connect(page, {
		quotas: [
			quota({
				enforcement: 'hard',
				standing: 'refused',
				used: 1_200_000_000_000,
				exceeded_at: '2026-08-05T09:00:00Z'
			})
		]
	});
	await page.goto('/settings/limits');
	await expect(page.getByTestId('cap-storage_bytes')).toContainText('new work refused');
	await expect(page.getByTestId('exceeded-storage_bytes')).toContainText('Over since');
});

test('a stamp from earlier in the period is past tense', async ({ page }) => {
	// The stamps never clear — that is what keeps "we were not told" answerable — but a row currently only
	// warned showing "Over since" reads as over right now. Found on a real tenant whose cap had been raised:
	// 183 of 200, "close to the limit", and "Over since 3:34am" underneath it.
	await connect(page, {
		quotas: [
			quota({
				quota_key: 'asset_count',
				limit_value: 200,
				used: 183,
				standing: 'warned',
				enforcement: 'hard',
				warned_at: '2026-08-24T03:30:00Z',
				exceeded_at: '2026-08-24T03:34:00Z'
			})
		]
	});
	await page.goto('/settings/limits');
	await expect(page.getByTestId('was-over-asset_count')).toContainText('is not now');
	await expect(page.getByTestId('exceeded-asset_count')).toHaveCount(0);
	// And the badge agrees: warned, not refused.
	await expect(page.getByTestId('cap-asset_count')).toContainText('close to the limit');
	await expect(page.getByTestId('cap-asset_count')).not.toContainText('Over since');
});

test('no caps says so, and says absent is not zero', async ({ page }) => {
	// An empty page would read as a tenant who had exhausted everything.
	await connect(page, { quotas: [] });
	await page.goto('/settings/limits');
	await expect(page.getByTestId('no-caps')).toContainText('No caps are set');
	await expect(page.getByTestId('no-caps')).toContainText('not a cap of zero');
	await expect(page.getByTestId('caps')).toHaveCount(0);
});

test('the month is named rather than left as a date', async ({ page }) => {
	await connect(page);
	await page.goto('/settings/limits');
	await expect(
		page.getByText('a calendar month, so they line up with the bill', { exact: false })
	).toBeVisible();
});

test('a caller without Manage is told which permission is missing', async ({ page }) => {
	await connect(page, { refuse: 403 });
	await page.goto('/settings/limits');
	await expect(page.getByRole('alert')).toContainText('does not hold Manage');
});

test('the limits screen is reachable from Connection', async ({ page }) => {
	await connect(page);
	await page.goto('/settings');
	await page.getByRole('link', { name: 'Limits' }).click();
	await expect(page.getByRole('heading', { name: 'Limits', level: 1 })).toBeVisible();
});

test('the limits screen has no axe violations', async ({ page }) => {
	await connect(page, {
		quotas: [
			quota(),
			quota({
				quota_key: 'asset_count',
				is_level: true,
				limit_value: 100,
				used: 100,
				standing: 'refused',
				enforcement: 'hard',
				exceeded_at: '2026-08-05T09:00:00Z'
			}),
			quota({
				quota_key: 'ai_spend_cents_month',
				is_level: false,
				standing: 'allowed',
				used: 10,
				limit_value: 50_000,
				warned_at: null
			})
		]
	});
	await page.goto('/settings/limits');
	await expect(page.getByTestId('caps').locator('li')).toHaveCount(3);
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
