/**
 * A portal, through a real browser against a mocked API (Q.14).
 *
 * The Rust gate proves the access rules — the share link's expiry, passcode and revocation, and that the set is
 * the outer bound. What exists only here is the page a stranger sees, and four properties of it:
 *
 * - **It is the tenant's page.** Title, intro, logo and one accent colour, applied as data rather than as a
 *   class — there is no fixed set of tenant colours to make classes from.
 * - **Every published asset is listed, including the ones whose bytes cannot be handed over**, with the reason.
 *   Hiding those would make the portal look like a smaller collection than the one that was published.
 * - **A passcode prompt is a prompt, not an error.** Nothing is wrong the first time it is asked for.
 * - **Search narrows what was given** and says so when it finds nothing.
 *
 * Plus: no axe violations in either theme, which matters more here than anywhere else in the app — this is the
 * one page the tenant's customers see.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Item = {
	asset_id: string;
	filename: string;
	mime: string | null;
	bytes: number | null;
	preview_url: string | null;
	preview_unavailable: string | null;
};

function item(name: string, overrides: Partial<Item> = {}): Item {
	return {
		asset_id: `asset-${name}`,
		filename: `${name}.jpg`,
		mime: 'image/jpeg',
		bytes: 204_800,
		// A data URI, so the page renders a real image without the suite depending on a delivery route.
		preview_url:
			'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%228%22 height=%228%22%3E%3C/svg%3E',
		preview_unavailable: null,
		...overrides
	};
}

function portal(overrides: Record<string, unknown> = {}) {
	return {
		title: 'Acme press kit',
		intro: 'Everything a journalist needs.',
		kind: 'standard',
		accent: '#ff6600',
		logo_url: null,
		allow_search: true,
		query: null,
		items: [item('harbour'), item('dawn')],
		total: 2,
		downloads_remaining: 5,
		expires_at: null,
		...overrides
	};
}

type Recorder = { visits: { q: string | null; passcode: string | null }[] };

async function connect(
	page: Page,
	options: { view?: Record<string, unknown>; status?: number; reason?: string } = {}
): Promise<Recorder> {
	const recorder: Recorder = { visits: [] };

	// No key in local storage: a portal visitor has no account, and the page must work without one. The base is
	// still needed so the client knows where the API is.
	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname.startsWith('/portal/')) {
			recorder.visits.push({
				q: url.searchParams.get('q'),
				passcode: url.searchParams.get('passcode')
			});
			if (options.status) {
				return route.fulfill({
					status: options.status,
					json: { reason: options.reason ?? 'no' }
				});
			}
			const q = url.searchParams.get('q');
			const view = options.view ?? portal();
			if (q) {
				const items = (view.items as Item[]).filter((row) => row.filename.includes(q));
				return route.fulfill({ json: { ...view, items, total: items.length, query: q } });
			}
			return route.fulfill({ json: view });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test("the portal is the tenant's page, and lists what was published", async ({ page }) => {
	await connect(page, {
		view: portal({
			logo_url:
				'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%228%22 height=%228%22%3E%3C/svg%3E'
		})
	});
	await page.goto('/portal/press-kit');

	await expect(page.getByRole('heading', { name: 'Acme press kit' })).toBeVisible();
	await expect(page.getByText('Everything a journalist needs.')).toBeVisible();
	await expect(page.getByText('2 assets')).toBeVisible();
	await expect(page.getByText('5 downloads left')).toBeVisible();
	await expect(page.getByRole('listitem')).toHaveCount(2);

	// The accent is applied as data. A class per tenant colour is not a thing that can exist.
	const colours = await page.evaluate(() => {
		// By the property it sets rather than "the div with a style": the page has more than one.
		const style = getComputedStyle(
			document.querySelector('[style*="--portal-accent"]') as HTMLElement
		);
		return {
			accent: style.getPropertyValue('--portal-accent').trim(),
			button: style.getPropertyValue('--portal-button').trim(),
			ink: style.getPropertyValue('--portal-ink').trim()
		};
	});
	expect(colours.accent).toBe('#ff6600');
	// And the button's *ink* is derived from it rather than assumed: white on #ff6600 is 3.1:1, which is not a
	// choice a page gets to make, so this orange gets dark text. `$lib/portal-colour` owns the maths and its
	// unit tests sweep the whole colour space; what matters here is that the page uses the derived pair at all.
	expect(colours.ink).toBe('#111827');
	expect(colours.button).toBe('#ff6600');
	// The logo is decoration, so its alt is empty rather than a guess at what it says.
	await expect(page.locator('img[alt=""]')).toBeVisible();
});

test('an asset whose bytes cannot be handed over is named with the reason', async ({ page }) => {
	await connect(page, {
		view: portal({
			items: [
				item('harbour'),
				item('unlicensed', {
					preview_url: null,
					preview_unavailable: 'this asset is not licensed for distribution'
				})
			]
		})
	});
	await page.goto('/portal/press-kit');

	// Listed, not hidden: the sender needs to know that something they published cannot be released.
	await expect(page.getByText('unlicensed.jpg')).toBeVisible();
	await expect(page.getByText('this asset is not licensed for distribution')).toBeVisible();
});

test('a passcode prompt is a prompt, not an error', async ({ page }) => {
	const recorder = await connect(page, { status: 401, reason: 'a passcode is required' });
	await page.goto('/portal/press-kit');

	await expect(page.getByRole('heading', { name: 'This portal needs a passcode' })).toBeVisible();
	// Nothing is wrong yet, so nothing is claimed to be.
	await expect(page.getByRole('alert')).toBeHidden();

	await page.getByLabel('Passcode').fill('wrong');
	await page.getByRole('button', { name: 'Open' }).click();
	// Now there is something wrong, in the server's words.
	await expect(page.getByRole('alert')).toContainText('a passcode is required');
	expect(recorder.visits.at(-1)?.passcode).toBe('wrong');
});

test('search narrows what was given and says when nothing matches', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/portal/press-kit');
	await expect(page.getByRole('listitem')).toHaveCount(2);

	await page.getByLabel('Search this portal').fill('harbour');
	await page.getByRole('button', { name: 'Search' }).click();
	await expect(page.getByRole('listitem')).toHaveCount(1);
	expect(recorder.visits.at(-1)?.q).toBe('harbour');

	await page.getByLabel('Search this portal').fill('boardroom');
	await page.getByRole('button', { name: 'Search' }).click();
	// Named, so a visitor knows the search ran rather than the page breaking.
	await expect(page.getByText('Nothing here matches')).toBeVisible();
});

test('a portal with searching off offers no search box', async ({ page }) => {
	await connect(page, { view: portal({ allow_search: false }) });
	await page.goto('/portal/press-kit');
	await expect(page.getByRole('heading', { name: 'Acme press kit' })).toBeVisible();
	await expect(page.getByLabel('Search this portal')).toBeHidden();
});

test('a dead portal says so in the words the server chose', async ({ page }) => {
	await connect(page, { status: 404, reason: 'this share link has expired' });
	await page.goto('/portal/press-kit');
	await expect(page.getByRole('heading', { name: 'This portal is not available' })).toBeVisible();
	// "Expired" tells a visitor to ask for a new link; "not found" would send them to re-type a correct URL.
	await expect(page.getByRole('alert')).toContainText('expired');
});

test('a capped portal shows what is beyond the page', async ({ page }) => {
	await connect(page, { view: portal({ total: 300 }) });
	await page.goto('/portal/press-kit');
	await expect(page.getByText('300 assets')).toBeVisible();
	await expect(page.getByText('Showing the first 2 of 300')).toBeVisible();
});

for (const theme of ['light', 'dark'] as const) {
	test(`the portal has no axe violations in ${theme}`, async ({ page }) => {
		await connect(page, {
			view: portal({
				items: [
					item('harbour'),
					item('unlicensed', {
						preview_url: null,
						preview_unavailable: 'this asset is not licensed for distribution'
					})
				]
			})
		});
		await page.emulateMedia({ colorScheme: theme });
		await page.goto('/portal/press-kit');
		await expect(page.getByRole('heading', { name: 'Acme press kit' })).toBeVisible();

		const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
		expect(results.violations).toEqual([]);
	});
}
