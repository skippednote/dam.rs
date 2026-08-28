/**
 * The download panel through a real browser, against a mocked API (Q.11d).
 *
 * The Rust gates prove the permissions, the cache key and the rights. What exists only here are five properties of
 * the *interface*:
 *
 * - **Each format shows the sentence somebody wrote for it.** A list of sizes makes a person guess, and that
 *   sentence is the reason the conversions table exists rather than a hard-coded set of dimensions.
 * - **"Being prepared" is a state, not an error.** The first person to ask for a format waits while it renders; the
 *   panel says so and keeps asking. A failure message would be wrong twice — nothing failed, and it is on its way.
 * - **A refusal is the server's own sentence.** A licence that does not cover this use, or a permission a format
 *   needs, is something the person can act on. Replacing it with "could not download" wastes their afternoon.
 * - **A caller who may look and not take sees no panel at all** — not an error about a thing they were never
 *   offered.
 * - **An archived original is described, not dimmed.** A restore is a different action; a disabled button invites
 *   pressing it.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const ASSET = '00000000-0000-4000-8000-000000000000';

type Format = {
	id: string;
	key: string;
	label: string;
	description: string;
	media_class: string;
	max_width: number;
	max_height: number;
	format: string;
	quality: number;
	fit: string;
	background: string;
	required_permission: string | null;
	is_active: boolean;
	sort_order: number;
};

function format(overrides: Partial<Format> = {}): Format {
	return {
		id: `c-${overrides.key ?? 'web'}`,
		key: 'web-2048',
		label: 'Web JPEG',
		description: 'Sized for a web page, and small enough to email.',
		media_class: 'image',
		max_width: 2048,
		max_height: 2048,
		format: 'jpeg',
		quality: 82,
		fit: 'contain',
		background: 'ffffff',
		required_permission: null,
		is_active: true,
		sort_order: 0,
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		formats?: Format[];
		originalAvailable?: boolean;
		mediaClass?: string;
		/** Status for the options request: 403 is a reader who may not download. */
		optionsStatus?: number;
		/** How many times a download request answers "rendering" before returning a URL. */
		renderingTimes?: number;
		/** A refusal for the download request, with the server's sentence. */
		downloadRefusal?: { status: number; reason: string };
		/** The intended-use vocabulary this tenant's licences reference. */
		channels?: string[];
		territories?: string[];
		/** Status for the vocabulary request: a tenant with no licences still gets a download button. */
		vocabularyStatus?: number;
	} = {}
) {
	const recorder = { asked: [] as string[], declared: [] as (string | null)[] };
	let rendering = options.renderingTimes ?? 0;

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;

		if (path.endsWith('/usage-options') || path === '/usage-options') {
			if (options.vocabularyStatus) {
				return route.fulfill({ status: options.vocabularyStatus, json: { reason: 'no' } });
			}
			return route.fulfill({
				json: {
					channels: options.channels ?? [],
					territories: options.territories ?? [],
					default_channel: 'internal',
					default_territory: 'WORLD'
				}
			});
		}
		if (path.endsWith('/download-options')) {
			if (options.optionsStatus) {
				return route.fulfill({
					status: options.optionsStatus,
					json: { reason: 'this key holds no download scope' }
				});
			}
			return route.fulfill({
				json: {
					original_available: options.originalAvailable ?? true,
					media_class: options.mediaClass ?? 'image',
					conversions: options.formats ?? [format()]
				}
			});
		}
		if (path.endsWith('/download')) {
			// Enforced, like the real server. axum's JSON extractor answers 415 without this header, and the app's
			// `request` helper omitted it for exactly one endpoint — this one — which every mocked suite happily
			// accepted while a real click failed. A mock that is more permissive than the server is a mock that
			// certifies bugs.
			// `headerValue` is async; comparing the promise made this reject everything, which is its own small
			// lesson about mocks that are stricter than they mean to be.
			if ((await route.request().headerValue('content-type')) !== 'application/json') {
				return route.fulfill({
					status: 415,
					json: { reason: 'expected application/json' }
				});
			}
			const body = route.request().postDataJSON() as {
				format: string;
				channel?: string;
				territory?: string;
			};
			recorder.asked.push(body.format);
			// What the request *carried*, which is what makes a declaration a declaration: the server reads the
			// presence of both fields, not a flag.
			recorder.declared.push(
				body.channel && body.territory ? `${body.channel}/${body.territory}` : null
			);
			if (options.downloadRefusal) {
				return route.fulfill({
					status: options.downloadRefusal.status,
					json: { reason: options.downloadRefusal.reason }
				});
			}
			if (rendering > 0) {
				rendering -= 1;
				return route.fulfill({
					status: 202,
					json: { url: null, status: 'rendering', format: body.format }
				});
			}
			// A relative URL that resolves inside the app, so following it does not leave the origin the test
			// controls — the panel's navigation is real, and a cross-origin one would end the test.
			return route.fulfill({
				json: { url: '/settings', status: 'ready', format: body.format }
			});
		}
		if (path === '/assets' || path === '/search') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
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
		if (path.startsWith('/assets/')) {
			return route.fulfill({
				json: {
					...summary(),
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
		return route.fulfill({ status: 404, json: {} });
	});

	function summary() {
		return {
			id: ASSET,
			filename: 'harbour.jpg',
			mime: 'image/jpeg',
			bytes: 2_400_000,
			width: 4000,
			height: 3000,
			tier: 'hot',
			rights_state: 'allowed',
			provenance_state: 'none',
			thumbnail_url: null,
			tag_confidence: null,
			is_favourite: false,
			average_stars: null,
			has_attachment: false
		};
	}

	return recorder;
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Download' });
}

async function open(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').first().dblclick();
	await expect(page.getByRole('region', { name: 'Comments' })).toBeVisible();
}

test('each format shows the sentence written for it', async ({ page }) => {
	await connect(page, {
		formats: [
			format(),
			format({
				id: 'c-print',
				key: 'print-full',
				label: 'Print PNG',
				description: 'Full size and lossless, for a printer.',
				format: 'png',
				max_width: 4096,
				max_height: 4096,
				sort_order: 1
			})
		]
	});
	await open(page);

	await expect(panel(page).getByText('Web JPEG')).toBeVisible();
	await expect(panel(page).getByText('small enough to email')).toBeVisible();
	await expect(panel(page).getByText('Full size and lossless')).toBeVisible();
	// The shape is named too, because "Web JPEG" alone does not say how big.
	await expect(panel(page).getByText('2048 × 2048')).toBeVisible();
	// In the configured order, not alphabetical.
	const rows = panel(page).getByRole('listitem');
	await expect(rows.nth(1)).toContainText('Web JPEG');
	await expect(rows.nth(2)).toContainText('Print PNG');
});

test('the original is offered first and asks for itself by name', async ({ page }) => {
	const recorder = await connect(page);
	await open(page);

	await panel(page)
		.getByRole('button', { name: /Original file/ })
		.click();
	await expect(page).toHaveURL(/settings/);
	expect(recorder.asked).toEqual(['original']);
});

test('a format being prepared says so and keeps asking', async ({ page }) => {
	// The first person to choose a format waits while it renders. Two "rendering" answers, then a URL.
	const recorder = await connect(page, { renderingTimes: 2 });
	await open(page);

	await panel(page)
		.getByRole('button', { name: /Web JPEG/ })
		.click();

	// Said, not silent, and not an error: nothing failed.
	await expect(panel(page).getByText('is being prepared')).toBeVisible();
	// And it arrives without the person doing anything more.
	await expect(page).toHaveURL(/settings/, { timeout: 15_000 });
	expect(recorder.asked).toEqual(['web-2048', 'web-2048', 'web-2048']);
});

test('a refusal is shown in the server’s own words', async ({ page }) => {
	await connect(page, {
		downloadRefusal: {
			status: 403,
			reason: 'rights refuse this download (denied): license_expired'
		}
	});
	await open(page);

	await panel(page)
		.getByRole('button', { name: /Web JPEG/ })
		.click();

	// The verdict and its code, as sent. A customer who cannot download their own asset needs to know why.
	await expect(panel(page).getByRole('alert')).toContainText('license_expired');
});

test('a caller who may not download sees no panel', async ({ page }) => {
	// 403 on the options request is a reader, not a fault. Showing an error about formats they were never
	// offered would be telling them off for opening an asset.
	await connect(page, { optionsStatus: 403 });
	await open(page);

	await expect(panel(page)).toHaveCount(0);
	await expect(page.getByText('Could not read the download formats')).toHaveCount(0);
});

test('a failed read of the formats is reported', async ({ page }) => {
	// Distinct from a reader's 403, which is silence. A 500 means something is wrong and the person should know
	// why the list is missing — and the first version of this panel could not show it: the banner lived inside a
	// section gated on the formats having loaded, so a failed load rendered nothing at all.
	await connect(page, { optionsStatus: 500 });
	await open(page);

	await expect(page.getByRole('region', { name: 'Download' }).getByRole('alert')).toContainText(
		'download scope'
	);
});

test('an archived original is described rather than dimmed', async ({ page }) => {
	await connect(page, { originalAvailable: false });
	await open(page);

	// A restore is a different action. A disabled button would invite pressing it and explain nothing.
	await expect(panel(page).getByText('archived and needs a restore')).toBeVisible();
	await expect(panel(page).getByRole('button', { name: /Original file/ })).toHaveCount(0);
	// The prepared formats are still there: they are separate objects that do not tier.
	await expect(panel(page).getByRole('button', { name: /Web JPEG/ })).toBeVisible();
});

test('a class with no formats says which class', async ({ page }) => {
	await connect(page, { formats: [], mediaClass: 'video' });
	await open(page);

	// "No prepared formats for video assets yet" rather than a blank space, which reads as a fault.
	await expect(panel(page).getByText('No prepared formats for video')).toBeVisible();
	// The original is still offered — nothing about a missing format set stops somebody taking the file.
	await expect(panel(page).getByRole('button', { name: /Original file/ })).toBeVisible();
});

test('a stated use travels with the download, and a default does not', async ({ page }) => {
	const recorder = await connect(page, {
		channels: ['web', 'print'],
		territories: ['WORLD', 'GB']
	});
	await open(page);

	// Untouched: nothing is sent, and the server records that nobody was asked. The panel names which default
	// applies rather than leaving the reader to guess what "not stated" means — asserted on the option's text
	// rather than its visibility, because an `option` inside a closed `select` is never visible.
	await expect(panel(page).getByLabel('What for')).toHaveValue('');
	await expect(panel(page).getByLabel('What for').locator('option').first()).toHaveText(
		'Not stated (internal)'
	);
	await panel(page)
		.getByRole('button', { name: /Web JPEG/ })
		.click();
	await expect(page).toHaveURL(/settings/);
	expect(recorder.declared).toEqual([null]);

	// Now stated. Both fields, because half an answer would be recorded as a whole one.
	await open(page);
	await panel(page).getByLabel('What for').selectOption('print');
	await expect(panel(page).getByText('Stating the use records it')).toBeVisible();
	await panel(page).getByLabel('Where').selectOption('GB');
	await expect(panel(page).getByText('Recorded against this asset as print in GB')).toBeVisible();
	await panel(page)
		.getByRole('button', { name: /Web JPEG/ })
		.click();
	await expect(page).toHaveURL(/settings/);
	expect(recorder.declared[1]).toBe('print/GB');
});

test('a half-answered use is not sent as a declaration', async ({ page }) => {
	// A channel with no territory is not an answer. The server can only see what arrived, so sending half would
	// have it record a stated use nobody completed.
	const recorder = await connect(page, { channels: ['web'], territories: ['GB'] });
	await open(page);

	await panel(page).getByLabel('What for').selectOption('web');
	await panel(page)
		.getByRole('button', { name: /Web JPEG/ })
		.click();
	await expect(page).toHaveURL(/settings/);
	expect(recorder.declared).toEqual([null]);
});

test('a tenant with no vocabulary still has a download button', async ({ page }) => {
	// The form is absent, not empty, and the failure of one read does not remove the other. A tenant with no
	// licences has nothing to declare against — and still needs to be able to take their own files.
	await connect(page, { vocabularyStatus: 500 });
	await open(page);

	await expect(panel(page).getByLabel('What for')).toHaveCount(0);
	await expect(panel(page).getByRole('button', { name: /Original file/ })).toBeVisible();
	await expect(panel(page).getByRole('button', { name: /Web JPEG/ })).toBeVisible();
});

test('the download panel has no accessibility violations', async ({ page }) => {
	await connect(page, {
		formats: [
			format(),
			format({ id: 'c-print', key: 'print-full', label: 'Print PNG', format: 'png', sort_order: 1 })
		],
		channels: ['web', 'print'],
		territories: ['WORLD', 'GB']
	});
	await open(page);
	await expect(panel(page).getByText('Web JPEG')).toBeVisible();
	// With the form present, which is the new interactive markup here.
	await expect(panel(page).getByLabel('What for')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
