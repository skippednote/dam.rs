/**
 * Placing and releasing a legal hold from the asset panel (G10).
 *
 * The chain's own properties are proved in Rust and the governance screen has its own suite. What only a
 * browser can show is whether the control in front of a curator behaves:
 *
 * - **The button is disabled until a reason is typed**, rather than submitted and then refused. A form that
 *   lets you press the consequential button and then says no has taught you nothing about why.
 * - **A no-op does not claim an entry.** `changed: false` means nothing was recorded, and reporting "held"
 *   would cite a row that does not exist.
 * - **The copy flips direction.** Releasing removes a legal protection and must not read like placing one.
 * - **The badge in the header follows.** It has drawn this state since the first release with nothing able to
 *   set it; the point of the panel is that the two now agree without a refetch.
 * - **A curator who may manage the asset but not read the log keeps the control.** The history disappears and
 *   no error appears for something they cannot fix.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';

const ASSET = '00000000-0000-4000-8000-000000000000';

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

type HoldOptions = {
	/** What the asset payload says, which is what the panel opens believing. */
	alreadyHeld?: boolean;
	/**
	 * What the server actually holds, when it differs from the payload.
	 *
	 * The realistic way a no-op happens: somebody else placed the hold after this panel loaded, so the screen
	 * offers "place" and the server answers that it was already placed.
	 */
	serverHeld?: boolean;
	history?: Entry[];
	logStatus?: number;
};

function entry(overrides: Partial<Entry> = {}): Entry {
	return {
		seq: 1,
		at: '2026-08-20T09:00:00.000000Z',
		actor_id: '716beec1-0000-4000-8000-000000000000',
		actor_kind: 'user',
		action: 'legal_hold.placed',
		target_kind: 'asset',
		target_id: ASSET,
		payload: { reason: 'litigation hold, matter 2026-114' },
		prev_hash: null,
		hash: 'a'.repeat(64),
		...overrides
	};
}

/** Tracks what the fake server currently believes, so a place-then-read sequence is coherent. */
let held = false;

async function connect(page: Page, options: HoldOptions = {}) {
	held = options.serverHeld ?? options.alreadyHeld ?? false;
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});
	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname.endsWith('/legal-hold')) {
			// The real server's rule: `changed` is whether the state actually moved, and nothing is recorded
			// when it did not.
			const asked = JSON.parse(route.request().postData() ?? '{}') as { held: boolean };
			const changed = asked.held !== held;
			held = asked.held;
			return route.fulfill({
				json: { asset_id: ASSET, held, changed, audit_seq: changed ? 42 : null }
			});
		}
		if (url.pathname === '/audit') {
			if (options.logStatus) {
				return route.fulfill({ status: options.logStatus, json: { reason: 'no' } });
			}
			return route.fulfill({ json: { entries: options.history ?? [], next_before_seq: null } });
		}
		if (url.pathname === '/assets') {
			return route.fulfill({
				json: {
					items: [
						{
							id: ASSET,
							filename: 'harbour.jpg',
							mime: 'image/jpeg',
							bytes: 4_000_000,
							width: 4000,
							height: 3000,
							tier: 'hot',
							rights_state: 'allowed',
							provenance_state: 'none',
							thumbnail_url: null,
							tag_confidence: null,
							is_favourite: false,
							average_stars: null,
							has_attachment: false,
							published_at: null
						}
					],
					total: 1,
					offset: 0
				}
			});
		}
		if (url.pathname === '/search/facets' || url.pathname === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		// The detail payload the panel is a child of. A `{}` here left the panel unrendered and every case
		// failing on the container rather than on anything this suite is about.
		if (url.pathname.startsWith('/assets/')) {
			return route.fulfill({
				json: {
					id: ASSET,
					filename: 'harbour.jpg',
					mime: 'image/jpeg',
					bytes: 4_000_000,
					width: 4000,
					height: 3000,
					tier: 'hot',
					rights_state: 'allowed',
					// `none`, not `unknown`. The provenance vocabulary is none/valid/invalid/untrusted, and
					// `unknown` belongs to *rights* — an invented value makes the detail panel throw
					// `Cannot read properties of undefined` from a metadata lookup, which presents as the panel
					// simply never opening.
					provenance_state: 'none',
					thumbnail_url: null,
					tag_confidence: null,
					is_favourite: false,
					average_stars: null,
					has_attachment: false,
					published_at: null,
					values: {},
					technical: {},
					duration_ms: null,
					page_count: null,
					color_space: 'sRGB',
					has_alpha: false,
					content_hash: 'a'.repeat(64),
					status: 'active',
					enrichment_state: 'done',
					legal_hold: options.alreadyHeld ?? false,
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
					preview_url: null
				}
			});
		}
		// Everything else the detail panel asks for. An empty object rather than a 404 so the panel under test
		// is not competing with a wall of unrelated errors.
		return route.fulfill({ json: {} });
	});
}

/** Opens the first asset's detail panel and returns the legal-hold section. */
async function openHold(page: Page) {
	await page.goto('/assets');
	await expect(page.getByRole('gridcell').first()).toBeVisible();
	await page.getByRole('gridcell').first().click();
	await expect(page.getByRole('complementary', { name: 'Selected asset' })).toBeVisible();
	const heading = page.getByRole('heading', { name: 'Legal hold' });
	await expect(heading).toBeVisible();
	return page.locator('section', { has: heading });
}

test('the button will not fire without a reason', async ({ page }) => {
	await connect(page);
	const panel = await openHold(page);
	const button = panel.getByRole('button', { name: 'Place a hold' });
	await expect(button).toBeDisabled();
	await panel.getByRole('textbox').fill('litigation hold, matter 2026-114');
	await expect(button).toBeEnabled();
});

test('placing a hold cites the entry, flips the copy, and lights the badge', async ({ page }) => {
	await connect(page);
	const panel = await openHold(page);
	await expect(panel).toContainText('Not held');
	await panel.getByRole('textbox').fill('litigation hold, matter 2026-114');
	await panel.getByRole('button', { name: 'Place a hold' }).click();

	await expect(panel.getByRole('status')).toContainText('Recorded as entry 42');
	await expect(panel).toContainText('cannot be deleted');
	await expect(panel.getByRole('button', { name: 'Release the hold' })).toBeVisible();
	// The header badge, which existed long before anything could set it.
	await expect(
		page.getByRole('complementary', { name: 'Selected asset' }).getByText('Legal hold', {
			exact: true
		})
	).toHaveCount(2);
});

test('a hold somebody else already placed is reported as recording nothing', async ({ page }) => {
	// The screen loaded before the other person's change landed, so it offers "place" over a hold that
	// exists. Reporting success would cite an audit entry that was never written.
	await connect(page, { alreadyHeld: false, serverHeld: true });
	const panel = await openHold(page);
	await panel.getByRole('textbox').fill('litigation hold, matter 2026-114');
	await panel.getByRole('button', { name: 'Place a hold' }).click();
	await expect(panel.getByRole('status')).toContainText('nothing recorded');
});

test('releasing a hold says so and drops the badge', async ({ page }) => {
	await connect(page, { alreadyHeld: true });
	const panel = await openHold(page);
	await expect(panel).toContainText('cannot be deleted');
	await panel.getByRole('textbox').fill('matter closed');
	await panel.getByRole('button', { name: 'Release the hold' }).click();
	await expect(panel.getByRole('status')).toContainText('Released. Recorded as entry 42');
	await expect(panel).toContainText('Not held');
	await expect(
		page.getByRole('complementary', { name: 'Selected asset' }).getByText('Legal hold', {
			exact: true
		})
	).toHaveCount(1, { timeout: 5000 });
});

test('the history is read from the record, with each reason', async ({ page }) => {
	await connect(page, {
		alreadyHeld: true,
		history: [
			entry({ seq: 4, action: 'legal_hold.lifted', payload: { reason: 'matter closed' } }),
			entry({ seq: 3, action: 'legal_hold.placed', payload: { reason: 'matter 2026-114' } }),
			// An unrelated action on the same asset must not appear in a hold history.
			entry({ seq: 2, action: 'role.granted', payload: {} })
		]
	});
	const panel = await openHold(page);
	await expect(panel).toContainText('Released');
	await expect(panel).toContainText('matter closed');
	await expect(panel).toContainText('matter 2026-114');
	await expect(panel).not.toContainText('role.granted');
});

test('a curator who cannot read the log keeps the control', async ({ page }) => {
	await connect(page, { logStatus: 403 });
	const panel = await openHold(page);
	await expect(panel).not.toContainText('Previous holds');
	await expect(panel.getByRole('alert')).toHaveCount(0);
	await expect(panel.getByRole('button', { name: 'Place a hold' })).toBeVisible();
});

test('a record that cannot be read is said quietly, and never as "no holds"', async ({ page }) => {
	// Not an alert: this is a read nobody asked for, of a record a curator cannot repair, and a red banner
	// next to a working button attaches the alarm to the wrong thing. It also must not fall through to
	// "no hold has ever been placed" — a failed read is in no position to claim that.
	await connect(page, { logStatus: 503 });
	const panel = await openHold(page);
	await expect(panel).toContainText('Could not read the record');
	await expect(panel).not.toContainText('No hold has ever been placed');
	await expect(panel.getByRole('alert')).toHaveCount(0);
	await expect(panel.getByRole('button', { name: 'Place a hold' })).toBeVisible();
});
