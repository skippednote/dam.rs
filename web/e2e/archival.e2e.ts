/**
 * Archival through a real browser, against a mocked API (§6.4, §6.5).
 *
 * The Rust gates prove the planner, the arithmetic, the permissions and the state machine. What exists only
 * here are five properties of the *interface*, and each of them is a decision somebody could reasonably have
 * made differently:
 *
 * - **The price is on screen before the button.** Expedited against Bulk is roughly 10× on cost and 100× on
 *   latency; a chooser that showed only the words would be asking somebody to guess with their employer's
 *   money. This is the whole reason the quote endpoint exists as a read.
 * - **A tier the class cannot offer stays visible and says why.** Deep Archive has no Expedited. Hiding the
 *   row gives one asset two options and another three, which invites a question the response can answer.
 * - **Once it is running, the form becomes a status.** Offering the buttons again would offer a second
 *   retrieval; the server coalesces so pressing them is harmless, but a button that does nothing is worse
 *   than no button.
 * - **A plan is readable at library scale.** Thousands of skips grouped by reason, with the pinned ones spelled
 *   out, because "why did nothing happen?" is the only question anybody asks of a lifecycle run.
 * - **Dry run is stated on the row.** A rule nobody has taken off dry run has never moved anything, and that is
 *   the most important fact about it.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';

type Quote = {
	tier: string;
	available: boolean;
	est_cost_cents: number;
	eta_at: string | null;
	needs_approval: boolean;
	unavailable_because: string | null;
};

/** Glacier offers all three tiers, with the spread the screen exists to show. */
function glacierQuote(): Quote[] {
	return [
		{
			tier: 'expedited',
			available: true,
			est_cost_cents: 1500,
			eta_at: new Date(Date.now() + 5 * 60_000).toISOString(),
			needs_approval: false,
			unavailable_because: null
		},
		{
			tier: 'standard',
			available: true,
			est_cost_cents: 500,
			eta_at: new Date(Date.now() + 5 * 3_600_000).toISOString(),
			needs_approval: false,
			unavailable_because: null
		},
		{
			tier: 'bulk',
			available: true,
			est_cost_cents: 125,
			eta_at: new Date(Date.now() + 12 * 3_600_000).toISOString(),
			needs_approval: false,
			unavailable_because: null
		}
	];
}

async function connect(
	page: Page,
	options: {
		tier?: string;
		quote?: Quote[];
		/** The restore the asset already has, if any. */
		current?: Record<string, unknown> | null;
		policies?: Record<string, unknown>[];
		plan?: Record<string, unknown>;
	} = {}
) {
	const recorder = { asked: [] as string[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;

		if (path.endsWith('/restore/quote')) {
			return route.fulfill({ json: { options: options.quote ?? glacierQuote() } });
		}
		if (path.endsWith('/restore') && route.request().method() === 'POST') {
			recorder.asked.push(url.searchParams.get('tier') ?? '');
			return route.fulfill({
				json: {
					id: 'r-1',
					tier: url.searchParams.get('tier') ?? 'standard',
					state: 'queued',
					est_cost_cents: 500,
					bytes: 1024,
					eta_at: new Date(Date.now() + 5 * 3_600_000).toISOString(),
					available_at: null,
					expires_at: null,
					joined_existing: false
				}
			});
		}
		if (path.endsWith('/restore')) {
			return route.fulfill({ json: options.current ?? null });
		}
		if (path === '/lifecycle/policies') {
			return route.fulfill({ json: options.policies ?? [] });
		}
		if (path.endsWith('/plan')) {
			return route.fulfill({ json: options.plan ?? {} });
		}
		if (path === '/lifecycle/runs') {
			return route.fulfill({
				status: 202,
				json: { job_id: 'j-1', policies_in_dry_run: 1, policies_enabled: 1 }
			});
		}
		if (path === '/assets' || path === '/search') {
			return route.fulfill({
				json: { items: [asset(options.tier ?? 'archive')], total: 1, offset: 0 }
			});
		}
		if (path === '/fields' || path === '/categories' || path === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (path === '/people') return route.fulfill({ json: [] });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		if (path.endsWith('/comments')) return route.fulfill({ json: [] });
		if (path.endsWith('/categories')) return route.fulfill({ json: [] });
		if (path.endsWith('/attachments')) return route.fulfill({ json: [] });
		if (path.endsWith('/versions')) return route.fulfill({ json: [] });
		if (path.endsWith('/history')) return route.fulfill({ json: [] });
		if (path.endsWith('/type')) return route.fulfill({ json: { field_keys: [] } });
		if (path.endsWith('/download-options')) {
			return route.fulfill({
				json: { original_available: false, media_class: 'image', conversions: [] }
			});
		}
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
		if (path.startsWith('/assets/')) {
			return route.fulfill({ json: detail(options.tier ?? 'archive') });
		}
		return route.fulfill({ json: {} });
	});

	return recorder;
}

function asset(tier: string) {
	return {
		id: ASSET,
		filename: 'winter-shoot.jpg',
		mime: 'image/jpeg',
		bytes: 4_200_000,
		width: 4000,
		height: 3000,
		tier,
		rights_state: 'allowed',
		provenance_state: 'none',
		thumbnail_url: null,
		tag_confidence: null,
		is_favourite: false,
		average_stars: null,
		has_attachment: false
	};
}

function detail(tier: string) {
	return {
		...asset(tier),
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
		preview_url: null,
		metadata_type: null,
		published_at: null
	};
}

async function openDetail(page: Page) {
	await page.goto('/assets');
	// Double-click on the cell, as the other panel suites do: a single click selects for the bulk bar.
	await page.getByRole('gridcell').first().dblclick();
	await expect(page.getByRole('region', { name: 'Comments' })).toBeVisible();
}

test('the tiers are priced and timed before anything is asked for', async ({ page }) => {
	await connect(page);
	await openDetail(page);

	const panel = page.getByRole('region', { name: 'Restore from cold storage' });
	await expect(panel).toBeVisible();

	// The comparison is the point: three tiers, each with its own wait and its own price.
	await expect(panel).toContainText('$15.00');
	await expect(panel).toContainText('$5.00');
	await expect(panel).toContainText('$1.25');
	await expect(panel).toContainText('~5 min');
	await expect(panel).toContainText('~5 h');
	await expect(panel).toContainText('~12 h');
});

test('a tier the class cannot offer is shown refused rather than hidden', async ({ page }) => {
	await connect(page, {
		quote: [
			{
				tier: 'expedited',
				available: false,
				est_cost_cents: 0,
				eta_at: null,
				needs_approval: false,
				unavailable_because: 'this storage class has no expedited tier'
			},
			...glacierQuote().slice(1)
		]
	});
	await openDetail(page);

	const panel = page.getByRole('region', { name: 'Restore from cold storage' });
	await expect(panel).toContainText('this storage class has no expedited tier');
	await expect(panel.getByRole('radio', { name: /expedited/i })).toBeDisabled();
	// And the choice defaults to one that can actually be used, rather than to a disabled row.
	await expect(panel.getByRole('radio', { name: /standard/i })).toBeChecked();
});

test('asking sends the chosen tier and turns the form into a status', async ({ page }) => {
	const recorder = await connect(page);
	await openDetail(page);

	const panel = page.getByRole('region', { name: 'Restore from cold storage' });
	await panel.getByRole('radio', { name: /bulk/i }).check();
	await panel.getByRole('button', { name: 'Restore' }).click();

	await expect(panel).toContainText('Restoring');
	await expect(panel).toContainText('Queued');
	expect(recorder.asked).toEqual(['bulk']);
	// The tiers are gone: offering them again would offer a second retrieval.
	await expect(panel.getByRole('button', { name: 'Restore' })).toHaveCount(0);
});

test('a restore somebody else started says so rather than offering the button again', async ({
	page
}) => {
	await connect(page, {
		tier: 'restoring',
		current: {
			id: 'r-9',
			tier: 'standard',
			state: 'ongoing',
			est_cost_cents: 500,
			bytes: 1024,
			eta_at: new Date(Date.now() + 3_600_000).toISOString(),
			available_at: null,
			expires_at: null,
			joined_existing: true
		}
	});
	await openDetail(page);

	const panel = page.getByRole('region', { name: 'Restore from cold storage' });
	await expect(panel).toContainText('The provider is working on it.');
	await expect(panel).toContainText('waiting on the same copy');
});

test('the restore panel has no axe violations', async ({ page }) => {
	await connect(page);
	await openDetail(page);
	await expect(page.getByRole('region', { name: 'Restore from cold storage' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});

// ─── the storage screen ─────────────────────────────────────────────────────

const POLICY = {
	id: 'p-1',
	name: 'Cold originals to Glacier IR',
	enabled: true,
	applies_to: 'original',
	target_class: 'GLACIER_IR',
	after_days: 90,
	dry_run: true,
	max_objects_per_run: 10000,
	last_run_at: null,
	last_run_moved: null
};

test('a rule says it is in dry run, which means it has never moved anything', async ({ page }) => {
	await connect(page, { policies: [POLICY] });
	await page.goto('/storage');

	await expect(page.getByText('Cold originals to Glacier IR')).toBeVisible();
	await expect(page.getByText('dry run — moves nothing')).toBeVisible();
	await expect(page.getByText('never run')).toBeVisible();
});

test('a plan groups its skips and spells out the pins', async ({ page }) => {
	await connect(page, {
		policies: [POLICY],
		plan: {
			policy_name: 'Cold originals to Glacier IR',
			dry_run: true,
			halted: null,
			transitions: [
				{
					object_key: 'acme/o/aa/bb/one',
					from: 'STANDARD',
					to: 'GLACIER_IR',
					size_bytes: 5_000_000,
					min_duration_until: null
				}
			],
			skipped: [
				{ object_key: 'a', reason: 'pinned', detail: 'the asset is under legal hold' },
				{
					object_key: 'b',
					reason: 'pinned',
					detail: "a member of the pinned collection 'Campaign'"
				},
				{ object_key: 'c', reason: 'not_yet_eligible', detail: 'eligible from 2026-09-01' },
				{ object_key: 'd', reason: 'not_yet_eligible', detail: 'eligible from 2026-09-02' },
				{ object_key: 'e', reason: 'not_yet_eligible', detail: 'eligible from 2026-09-03' }
			]
		}
	});
	await page.goto('/storage');
	await page.getByRole('button', { name: 'Plan' }).click();

	// "object would move", singular, because the plan holds one — and the size beside it, which is the
	// number that tells an operator whether this is a routine sweep or a migration.
	await expect(page.getByText(/object would move/)).toContainText('4.8 MiB');
	// Grouped, most numerous first — three of one reason, two of another.
	await expect(page.getByText('3 — not idle long enough yet')).toBeVisible();
	await expect(page.getByText('2 — pinned')).toBeVisible();
	// And each pin named, because a pin is something somebody can go and undo.
	await expect(page.getByText('Pinned: the asset is under legal hold')).toBeVisible();
	await expect(
		page.getByText("Pinned: a member of the pinned collection 'Campaign'")
	).toBeVisible();
});

test('a sweep restates how many rules will move nothing', async ({ page }) => {
	await connect(page, { policies: [POLICY] });
	await page.goto('/storage');
	await page.getByRole('button', { name: 'Run a sweep now' }).click();

	// "I pressed run and nothing moved" is otherwise a support ticket rather than a policy doing what it says.
	await expect(page.getByRole('status')).toContainText('1 of 1 enabled rules are in dry run');
});

test('a tenant with no rules is told that nothing is being moved', async ({ page }) => {
	await connect(page, { policies: [] });
	await page.goto('/storage');

	// The useful property is that the empty state explains the *consequence* — nothing is moving, so
	// everything stays where it was put — rather than only saying "none". Asserted as substrings of the one
	// paragraph, because Playwright normalises whitespace for strings and the sentence wraps in the source.
	const empty = page.getByText('No tiering rules are enabled', { exact: false });
	await expect(empty).toBeVisible();
	await expect(empty).toContainText('stays in the class it was uploaded to');
});

test('the storage screen has no axe violations', async ({ page }) => {
	await connect(page, { policies: [POLICY] });
	await page.goto('/storage');
	await expect(page.getByText('Cold originals to Glacier IR')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
