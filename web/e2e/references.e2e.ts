/**
 * Where an asset is used on connected sites (M3d·4, §11.4).
 *
 * The Rust suites prove the pin lapse, the sweep and who may report. What lives only here are properties of the
 * panel somebody reads *before a takedown*:
 *
 * - **"Used nowhere" is stated, not implied by an empty panel.** Somebody about to delete an asset needs that
 *   fact, and hiding the panel would make its absence indistinguishable from the feature not existing.
 * - **The page count is labelled as the site's own.** damrs cannot see somebody else's website. Presenting a
 *   reported number as a hard total beside two it does know is how a confident-sounding figure gets decided on.
 * - **A site that stopped reporting is called out on its row**, and says it is not counted above. Silently
 *   discounting it would make the totals look wrong; counting it would make them false.
 * - **Dead references are kept, behind a disclosure.** "This used to be on three pages" is why the number is
 *   what it is.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';

type Reference = {
	connector_id: string;
	connector_label: string;
	asset_id: string;
	remote_entity_type: string;
	remote_entity_id: string;
	remote_url: string | null;
	usage_count: number;
	usage_sample: unknown;
	synced_version_no: number | null;
	synced_at: string | null;
	state: string;
	version_drifted: boolean;
	refresh_overdue: boolean;
};

function reference(overrides: Partial<Reference> = {}): Reference {
	return {
		connector_id: 'c1111111-1111-4111-8111-111111111111',
		connector_label: 'Marketing site',
		asset_id: ASSET,
		remote_entity_type: 'media',
		remote_entity_id: '42',
		remote_url: 'https://www.example.com/media/42',
		usage_count: 12,
		usage_sample: [],
		synced_version_no: 1,
		synced_at: '2026-08-24T09:00:00Z',
		state: 'linked',
		version_drifted: false,
		refresh_overdue: false,
		...overrides
	};
}

type Impact = {
	sites: number;
	entities: number;
	pages: number;
	references: Reference[];
};

async function connect(page: Page, impact: Impact) {
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});
	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname.endsWith('/references')) {
			return route.fulfill({ json: impact });
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
					preview_url: null
				}
			});
		}
		// Everything else the detail panel asks for. An empty object rather than a 404 so the panel under test
		// is not competing with a wall of unrelated errors.
		return route.fulfill({ json: {} });
	});
}

/** Opens the first asset's detail panel. */
async function open(page: Page) {
	await page.goto('/assets');
	await expect(page.getByRole('gridcell').first()).toBeVisible();
	await page.getByRole('gridcell').first().click();
	await expect(page.getByRole('complementary', { name: 'Selected asset' })).toBeVisible();
}

test('a live reference says how much it would break, and whose number that is', async ({
	page
}) => {
	await connect(page, {
		sites: 2,
		entities: 3,
		pages: 16,
		references: [
			reference(),
			reference({ remote_entity_id: '43', usage_count: 3 }),
			reference({
				connector_id: 'd1111111-1111-4111-8111-111111111111',
				connector_label: 'Campaign microsite',
				remote_entity_id: '9',
				usage_count: 1
			})
		]
	});
	await open(page);

	const impact = page.getByTestId('impact');
	await expect(impact).toContainText('Sites');
	await expect(impact).toContainText('2');
	await expect(impact).toContainText('16');
	// The third number is labelled differently, because damrs cannot see somebody else's website.
	await expect(impact).toContainText('Pages, as reported');
	await expect(page.getByText('damrs cannot see somebody else', { exact: false })).toBeVisible();

	const rows = page.getByTestId('live-references').locator('li');
	await expect(rows).toHaveCount(3);
	await expect(rows.first()).toContainText('Marketing site');
	await expect(rows.first()).toContainText('12 pages');
	// The link goes to the site, so an operator can look at the page before deciding.
	await expect(rows.first().getByRole('link')).toHaveAttribute(
		'href',
		'https://www.example.com/media/42'
	);
});

test('used nowhere is stated rather than left as an empty panel', async ({ page }) => {
	// Somebody about to delete an asset needs this fact. An empty panel would be indistinguishable from the
	// feature not existing.
	await connect(page, { sites: 0, entities: 0, pages: 0, references: [] });
	await open(page);
	await expect(page.getByTestId('used-nowhere')).toContainText('Not in use on any connected site');
	await expect(page.getByTestId('used-nowhere')).toContainText('will not change any page');
	await expect(page.getByTestId('impact')).toHaveCount(0);
});

test('a site that stopped reporting is called out and says it is not counted', async ({ page }) => {
	// Silently discounting it makes the totals look wrong; counting it makes them false. So the row says so.
	await connect(page, {
		sites: 1,
		entities: 1,
		pages: 12,
		references: [
			reference(),
			reference({
				remote_entity_id: '77',
				usage_count: 40,
				refresh_overdue: true,
				synced_at: '2026-05-01T09:00:00Z'
			})
		]
	});
	await open(page);

	await expect(page.getByTestId('overdue-77')).toContainText('not reported for a while');
	await expect(page.getByTestId('overdue-77')).toContainText('not counted above');
	// And the totals are the live ones, not the sum of the rows.
	await expect(page.getByTestId('impact')).toContainText('12');
});

test('version drift reads as a job to run rather than a site to chase', async ({ page }) => {
	await connect(page, {
		sites: 1,
		entities: 1,
		pages: 4,
		references: [reference({ version_drifted: true, synced_version_no: 1 })]
	});
	await open(page);
	await expect(page.getByText('showing an older version')).toBeVisible();
	// Not conflated with the other kind of stale.
	await expect(page.getByText('not reported for a while')).toHaveCount(0);
});

test('dead references are kept behind a disclosure and say why', async ({ page }) => {
	await connect(page, {
		sites: 1,
		entities: 1,
		pages: 12,
		references: [
			reference(),
			reference({ remote_entity_id: '43', state: 'orphaned' }),
			reference({ remote_entity_id: '44', state: 'expired' })
		]
	});
	await open(page);

	await expect(page.getByText('2 no longer in use')).toBeVisible();
	await page.getByText('2 no longer in use').click();
	const dead = page.getByTestId('dead-references').locator('li');
	await expect(dead).toHaveCount(2);
	await expect(dead.first()).toContainText('the site no longer lists it');
	await expect(dead.nth(1)).toContainText('the licence expired');
});

test('an asset used nowhere but formerly referenced says both', async ({ page }) => {
	await connect(page, {
		sites: 0,
		entities: 0,
		pages: 0,
		references: [reference({ state: 'orphaned' })]
	});
	await open(page);
	await expect(page.getByTestId('used-nowhere')).toContainText('used to exist');
	await expect(page.getByTestId('used-nowhere')).not.toContainText('will not change any page');
});

test('the reference panel has no axe violations', async ({ page }) => {
	await connect(page, {
		sites: 2,
		entities: 3,
		pages: 16,
		references: [
			reference(),
			reference({ remote_entity_id: '43', version_drifted: true, refresh_overdue: true }),
			reference({ remote_entity_id: '44', state: 'orphaned', remote_url: null })
		]
	});
	await open(page);
	await expect(page.getByTestId('impact')).toBeVisible();
	await page.getByText('1 no longer in use').click();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
