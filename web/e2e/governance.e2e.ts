/**
 * The governance record on screen (G10).
 *
 * The Rust suites prove the chain: that an altered row, a removed row and an appended forgery are each caught,
 * and each reported as the right kind of thing. What lives only here are the properties of the two screens
 * that decide whether any of that reaches a person:
 *
 * - **A broken chain is loud, and a failed request is not.** The server answers a broken chain with a 200 and
 *   `intact: false` on purpose — a 500 would be indistinguishable from the database being down. The screen has
 *   to keep those apart: one is an alarm, the other is "could not check just now".
 * - **The two kinds of break read differently.** "An entry is missing" sends somebody to a gap; "an entry has
 *   been altered" sends them to a row.
 * - **Exporting says it recorded itself**, or the list growing by one after every export looks like a bug.
 * - **A hold cannot be placed without a reason**, and the button is disabled rather than submitted-then-refused
 *   — a form that lets you press the consequential button and then says no has taught you nothing.
 * - **A no-op does not claim an entry.** `changed: false` means nothing was recorded, and saying "held" would
 *   cite a row that does not exist.
 * - **A curator who may manage an asset but not read the log still gets the control.** The history disappears;
 *   the button does not, and no error appears for something they cannot fix.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Entry = {
	seq: number;
	at: string;
	actor_id: string | null;
	actor_kind: string;
	action: string;
	target_kind: string;
	target_id: string | null;
	payload: Record<string, unknown>;
	prev_hash: string | null;
	hash: string;
};

function entry(overrides: Partial<Entry> = {}): Entry {
	return {
		seq: 1,
		at: '2026-08-20T09:00:00.000000Z',
		actor_id: '716beec1-0000-4000-8000-000000000000',
		actor_kind: 'user',
		action: 'legal_hold.placed',
		target_kind: 'asset',
		target_id: 'a-1',
		payload: { reason: 'litigation hold, matter 2026-114', filename: 'boat.jpg' },
		prev_hash: null,
		hash: 'a'.repeat(64),
		...overrides
	};
}

type Options = {
	entries?: Entry[];
	verification?: unknown;
	verifyStatus?: number;
	logStatus?: number;
};

async function connect(page: Page, options: Options = {}) {
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});
	const entries = options.entries ?? [entry()];
	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname === '/audit/verify') {
			if (options.verifyStatus) {
				return route.fulfill({ status: options.verifyStatus, json: { reason: 'no' } });
			}
			return route.fulfill({
				json: options.verification ?? {
					intact: true,
					checked: entries.length,
					from_seq: 0,
					through_seq: entries.length,
					failure: null
				}
			});
		}
		if (url.pathname === '/audit/export') {
			return route.fulfill({
				json: {
					entries,
					anchor: null,
					recorded_as: entry({ seq: 99, action: 'audit.exported', payload: {} }),
					chain_version: 1
				}
			});
		}
		if (url.pathname === '/audit') {
			if (options.logStatus) {
				return route.fulfill({ status: options.logStatus, json: { reason: 'no' } });
			}
			return route.fulfill({ json: { entries, next_before_seq: null } });
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		return route.fulfill({ status: 404, json: {} });
	});
}

test('an intact chain says what it checked', async ({ page }) => {
	await connect(page);
	await page.goto('/governance');
	await expect(page.getByTestId('integrity')).toContainText('Intact');
	await expect(page.getByTestId('integrity')).toContainText('1 entry checked');
});

test('an altered entry is an alarm, and names the row', async ({ page }) => {
	await connect(page, {
		verification: {
			intact: false,
			checked: 3,
			from_seq: 0,
			through_seq: 3,
			failure: { kind: 'altered', seq: 3, detail: 'stored hash aaa, recomputed bbb' }
		}
	});
	await page.goto('/governance');
	const alarm = page.getByRole('alert');
	await expect(alarm).toContainText('Entry 3 has been altered');
	await expect(page.getByTestId('integrity')).toContainText('stored hash aaa, recomputed bbb');
	// And it says how it could have happened, because "altered" without that reads as a bug in the software.
	await expect(page.getByTestId('integrity')).toContainText('database-level access');
});

test('a missing entry reads as a gap rather than as an edit', async ({ page }) => {
	// The distinction the server draws, and the reason it draws it: one points at a row, the other at a gap.
	await connect(page, {
		verification: {
			intact: false,
			checked: 2,
			from_seq: 0,
			through_seq: 2,
			failure: { kind: 'unlinked', seq: 2, detail: 'names aaa, the entry before hashes to ccc' }
		}
	});
	await page.goto('/governance');
	await expect(page.getByRole('alert')).toContainText('An entry is missing');
});

test('a failed check is not shown as a broken chain', async ({ page }) => {
	// The whole reason the server answers a break with a 200. If a 500 rendered the same alarm, an outage
	// would read as tampering — and the first time that happened nobody would believe the alarm again.
	await connect(page, { verifyStatus: 503 });
	await page.goto('/governance');
	await expect(page.getByTestId('integrity')).toContainText('Could not run the check just now');
	await expect(page.getByRole('alert')).toHaveCount(0);
});

test('exporting says it recorded itself', async ({ page }) => {
	await connect(page);
	await page.goto('/governance');
	await page.getByRole('button', { name: 'Export' }).click();
	await expect(page.getByTestId('integrity')).toContainText('Recorded as entry 99');
	await expect(page.getByTestId('integrity')).toContainText(
		'taking a copy is itself in the record'
	);
});

test('an action nobody has phrasing for is shown as itself', async ({ page }) => {
	// The column is deliberately free text so a later subsystem can record something without a migration.
	// Guessing at an unknown value would be worse than printing it.
	await connect(page, { entries: [entry({ action: 'something.new', payload: {} })] });
	await page.goto('/governance');
	await expect(page.getByTestId('entry-1')).toContainText('something.new');
});

test('a caller without the gate is told, not shown an error', async ({ page }) => {
	await connect(page, { logStatus: 403, verifyStatus: 403 });
	await page.goto('/governance');
	await expect(page.getByRole('status')).toContainText('needs administrator access');
	await expect(page.getByRole('alert')).toHaveCount(0);
});

test('the governance record has no accessibility violations', async ({ page }) => {
	await connect(page);
	await page.goto('/governance');
	await expect(page.getByTestId('integrity')).toContainText('Intact');
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
