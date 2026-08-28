/**
 * Share links, through a real browser, against a mocked API.
 *
 * Same posture as `browse.e2e.ts`: the Rust gate proves the endpoints against a real Postgres (ten cases
 * for the share contract alone), so what belongs here is only the browser half — that the token is shown
 * once and composed onto *this app's* origin rather than the API's, that the portal asks for a passcode
 * when the server says 401 and shows the server's own refusal text otherwise, that a rights-refused share
 * renders the filename but neither pixels nor a download button, and that none of it has an axe violation.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

/** A 1x1 WebP, so the portal's `<img>` has real bytes to decode. */
const WEBP_1PX = 'UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==';

const ASSET_ID = '00000000-0000-4000-8000-000000000000';
const SHARE_ID = '00000000-0000-4000-8000-00000000aaaa';

function summary() {
	return {
		id: ASSET_ID,
		filename: 'campaign-0000.jpg',
		mime: 'image/jpeg',
		bytes: 2_400_000,
		width: 4000,
		height: 3000,
		tier: 'hot',
		rights_state: 'allowed',
		provenance_state: 'valid',
		thumbnail_url: null,
		tag_confidence: null,
		// Engagement, as every summary now carries it (Q.5c).
		is_favourite: false,
		average_stars: null,
		// Paperwork flag, as every summary now carries it (Q.9).
		has_attachment: false
	};
}

function shareRow(overrides: Record<string, unknown> = {}) {
	return {
		id: SHARE_ID,
		filename: 'campaign-0000.jpg',
		created_at: '2026-08-10T09:00:00Z',
		expires_at: '2026-09-10T09:00:00Z',
		max_downloads: 5,
		download_count: 2,
		has_passcode: true,
		allow_original: false,
		revoked: false,
		live: true,
		...overrides
	};
}

const PORTAL_VIEW = {
	filename: 'campaign-0000.jpg',
	mime: 'image/jpeg',
	bytes: 2_400_000,
	width: 4000,
	height: 3000,
	preview_url: '/d/portal-preview-token',
	preview_unavailable: null,
	download_allowed: true,
	downloads_remaining: 3,
	expires_at: '2026-09-10T09:00:00Z'
};

type Recorder = {
	/** POST /shares bodies, so a test can assert what the server was actually asked to create. */
	created: Record<string, unknown>[];
	/** DELETE /shares/{id} ids. */
	revoked: string[];
	/** POST /share/{token}(/download) bodies, keyed by path — the passcode travels in the body, never the URL. */
	portal: { path: string; body: Record<string, unknown> }[];
};

/**
 * The management side: an authenticated session, like `browse.e2e.ts`.
 *
 * `revoked` mutates the list the next GET returns, because the page reloads after a revoke and a mock that
 * kept saying "live" would hide a page that never actually called DELETE.
 */
async function connectManagement(page: Page): Promise<Recorder> {
	const recorder: Recorder = { created: [], revoked: [], portal: [] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/assets') {
			return route.fulfill({ json: { items: [summary()], total: 1, offset: 0 } });
		}
		if (url.pathname === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/shares' && method === 'POST') {
			recorder.created.push(route.request().postDataJSON() as Record<string, unknown>);
			return route.fulfill({
				json: {
					id: SHARE_ID,
					token: 'tok-e2e-shown-once',
					portal_path: '/share/tok-e2e-shown-once'
				}
			});
		}
		if (url.pathname === '/shares' && method === 'GET') {
			return route.fulfill({
				json: [
					shareRow(recorder.revoked.includes(SHARE_ID) ? { revoked: true, live: false } : {}),
					shareRow({
						id: '00000000-0000-4000-8000-00000000bbbb',
						filename: null,
						expires_at: null,
						max_downloads: null,
						download_count: 0,
						has_passcode: false,
						allow_original: true,
						revoked: true,
						live: false
					})
				]
			});
		}
		if (url.pathname.startsWith('/shares/') && method === 'DELETE') {
			recorder.revoked.push(url.pathname.split('/').pop() ?? '');
			return route.fulfill({ status: 204, body: '' });
		}
		if (url.pathname.endsWith('/usage-options') || url.pathname === '/usage-options') {
			// Q.12's intended-use vocabulary, asked alongside the formats. Empty here: a suite about
			// something else should not grow a form it never meant to test.
			return route.fulfill({
				json: {
					channels: [],
					territories: [],
					default_channel: 'internal',
					default_territory: 'WORLD'
				}
			});
		}
		if (url.pathname.endsWith('/download-options')) {
			// Q.11's download panel asks this on every asset the detail view opens. Answered so a suite about
			// sharing does not render an error banner it never intended to test.
			return route.fulfill({
				json: { original_available: true, media_class: 'image', conversions: [] }
			});
		}
		if (url.pathname.startsWith('/assets/')) {
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
					// The full engagement, as the detail payload now carries it (Q.5c). Present rather than
					// omitted because the API always sends it, and a mock that lags the contract tests a shape
					// the server never produces.
					engagement: {
						asset_id: '00000000-0000-4000-8000-000000000000',
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

	return recorder;
}

/**
 * The recipient side: no localStorage at all, because a recipient has none. The client falls back to its
 * default base (127.0.0.1:8099), which is exactly the address these mocks answer.
 */
async function connectPortal(
	page: Page,
	behaviour: {
		view: (body: Record<string, unknown>) => { status: number; json: unknown };
		download?: (body: Record<string, unknown>) => { status: number; json: unknown };
	}
): Promise<Recorder> {
	const recorder: Recorder = { created: [], revoked: [], portal: [] };

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());

		if (url.pathname.startsWith('/share/') && url.pathname.endsWith('/download')) {
			const body = (route.request().postDataJSON() ?? {}) as Record<string, unknown>;
			recorder.portal.push({ path: url.pathname, body });
			const reply = behaviour.download?.(body) ?? {
				status: 200,
				json: { url: '/d/portal-download-token', downloads_remaining: 2 }
			};
			return route.fulfill(reply);
		}
		if (url.pathname.startsWith('/share/')) {
			const body = (route.request().postDataJSON() ?? {}) as Record<string, unknown>;
			recorder.portal.push({ path: url.pathname, body });
			return route.fulfill(behaviour.view(body));
		}
		if (url.pathname.startsWith('/d/')) {
			return route.fulfill({ contentType: 'image/webp', body: Buffer.from(WEBP_1PX, 'base64') });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('creating a share shows the link once, on this origin', async ({ page }) => {
	const recorder = await connectManagement(page);
	await page.goto('/assets');
	await page.getByRole('gridcell').first().click();

	await page.getByRole('button', { name: 'Share…' }).click();
	await page.getByRole('button', { name: 'Create link' }).click();

	// The link is the *web app's* origin plus the portal route — not the API's origin. A recipient opens
	// the portal page; the portal page talks to the API.
	const link = page.getByRole('textbox', { name: 'Share link' });
	await expect(link).toHaveValue(/^http:\/\/localhost:\d+\/share\/tok-e2e-shown-once$/);
	await expect(page.getByText(/shown once/)).toBeVisible();

	// The default posture travels: expiring (a week), web rendition only.
	expect(recorder.created).toEqual([
		{ asset_id: ASSET_ID, expires_in_hours: 168, allow_original: false }
	]);
});

test('the shares page lists every link and revoke takes effect', async ({ page }) => {
	const recorder = await connectManagement(page);
	await page.goto('/shares');

	const table = page.getByRole('table');
	await expect(table.getByText('campaign-0000.jpg')).toBeVisible();
	// The share whose asset was deleted still appears — it existed, and the audit question "what did we
	// share" outlives the asset.
	await expect(table.getByText('(deleted asset)')).toBeVisible();
	await expect(table.getByText('live')).toBeVisible();
	await expect(table.getByText('2 / 5')).toBeVisible();
	await expect(table.getByText('passcode · web only')).toBeVisible();

	await page.getByRole('button', { name: 'Revoke' }).click();
	await expect(table.getByText('revoked')).toHaveCount(2);
	expect(recorder.revoked).toEqual([SHARE_ID]);
});

test('the portal shows the file and a download spends the limit', async ({ page }) => {
	const recorder = await connectPortal(page, {
		view: () => ({ status: 200, json: PORTAL_VIEW })
	});
	await page.goto('/share/tok-e2e-live');

	await expect(page.getByRole('heading', { name: 'campaign-0000.jpg' })).toBeVisible();
	await expect(page.getByRole('img', { name: 'campaign-0000.jpg' })).toBeVisible();
	await expect(page.getByText('3 downloads remaining')).toBeVisible();

	await page.getByRole('button', { name: 'Download' }).click();
	// The grant navigates the page itself (no popup to block); the mock serves image bytes at /d/.
	await page.waitForURL('**/d/portal-download-token');
	expect(recorder.portal.map((call) => call.path)).toEqual([
		'/share/tok-e2e-live',
		'/share/tok-e2e-live/download'
	]);
});

test('a passcode-gated link asks, refuses a wrong one, opens on the right one', async ({
	page
}) => {
	const recorder = await connectPortal(page, {
		view: (body) => {
			if (body.passcode === 'orchid') return { status: 200, json: PORTAL_VIEW };
			return {
				status: 401,
				json: {
					reason:
						body.passcode === undefined
							? 'this link requires a passcode'
							: 'that passcode is not right'
				}
			};
		}
	});
	await page.goto('/share/tok-e2e-coded');

	await expect(page.getByRole('heading', { name: 'This link needs a passcode' })).toBeVisible();

	const field = page.getByLabel('Passcode');
	await field.fill('wrong');
	await page.getByRole('button', { name: 'Open' }).click();
	// The server's own message, verbatim — "required" and "wrong" are different advice.
	await expect(page.getByRole('alert')).toHaveText('that passcode is not right');

	await field.fill('orchid');
	await page.getByRole('button', { name: 'Open' }).click();
	await expect(page.getByRole('heading', { name: 'campaign-0000.jpg' })).toBeVisible();

	// The passcode travelled in POST bodies, never in the URL where it would land in server logs.
	for (const call of recorder.portal) {
		expect(call.path).not.toContain('wrong');
		expect(call.path).not.toContain('orchid');
	}
});

test('a rights-refused share names the file but offers neither pixels nor a download', async ({
	page
}) => {
	await connectPortal(page, {
		view: () => ({
			status: 200,
			json: {
				...PORTAL_VIEW,
				preview_url: null,
				preview_unavailable: 'This file is not licensed for distribution.',
				download_allowed: false,
				downloads_remaining: null
			}
		})
	});
	await page.goto('/share/tok-e2e-unlicensed');

	await expect(page.getByRole('heading', { name: 'campaign-0000.jpg' })).toBeVisible();
	await expect(page.getByText('This file is not licensed for distribution.')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Download' })).toHaveCount(0);
	await expect(page.getByText(/Downloading isn't available/)).toBeVisible();
});

test('a dead link says why and what to do, not a bare error', async ({ page }) => {
	await connectPortal(page, {
		view: () => ({ status: 404, json: { reason: 'this link has expired' } })
	});
	await page.goto('/share/tok-e2e-dead');

	await expect(
		page.getByRole('heading', { name: 'This link does not work any more' })
	).toBeVisible();
	await expect(page.getByRole('alert')).toHaveText('this link has expired');
	await expect(page.getByText(/ask the person who sent it/)).toBeVisible();
});

test('the shares page has no axe violations', async ({ page }) => {
	await connectManagement(page);
	await page.goto('/shares');
	await expect(page.getByRole('table')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on /shares:\n${detail}`).toEqual([]);
});

test('the portal has no axe violations, passcode form included', async ({ page }) => {
	await connectPortal(page, {
		view: (body) =>
			body.passcode
				? { status: 200, json: PORTAL_VIEW }
				: { status: 401, json: { reason: 'this link requires a passcode' } }
	});
	await page.goto('/share/tok-e2e-axe');

	await expect(page.getByRole('heading', { name: 'This link needs a passcode' })).toBeVisible();
	let results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	let detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the passcode form:\n${detail}`).toEqual([]);

	await page.getByLabel('Passcode').fill('anything');
	await page.getByRole('button', { name: 'Open' }).click();
	await expect(page.getByRole('heading', { name: 'campaign-0000.jpg' })).toBeVisible();

	results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the opened portal:\n${detail}`).toEqual([]);
});
