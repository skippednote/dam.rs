/**
 * The engagement panel through a real browser, against a mocked API.
 *
 * The Rust gate proves the rules and the arithmetic. What only exists here is the browser half, and three
 * properties that are decisions about the *interface*:
 *
 * - **The rating is a radio group.** Five stars are five values of one thing. As buttons they would be five
 *   unrelated controls to a screen reader, with nothing saying which is chosen or that they are alternatives.
 * - **The average and the caller's own rating are shown as different facts.** A widget that displayed one number
 *   could only be lying about the other, so the stars draw the average and the radio marks the caller's.
 * - **Clearing is a separate control, because there is no zero star.** A sixth star meaning "none" is exactly the
 *   conflation the model avoids — an average over a table where zero means absent is wrong in a way nobody sees
 *   until the numbers are on a screen.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

const ASSET = '00000000-0000-4000-8000-000000000000';
/** A second asset, so switching selection can be exercised at all. */
const OTHER = '00000000-0000-4000-8000-000000000001';

type Engagement = {
	asset_id: string;
	average_stars: number | null;
	rating_count: number;
	favourite_count: number;
	my_stars: number | null;
	is_favourite: boolean;
	is_watched: boolean;
};

function engagement(overrides: Partial<Engagement> = {}): Engagement {
	return {
		asset_id: ASSET,
		average_stars: null,
		rating_count: 0,
		favourite_count: 0,
		my_stars: null,
		is_favourite: false,
		is_watched: false,
		...overrides
	};
}

type Recorder = {
	/** Method and path of every engagement write, so a test can assert what the toggle actually sent. */
	writes: { method: string; path: string; body: unknown }[];
};

/**
 * Mounts the mocks.
 *
 * The engagement endpoints are modelled *statefully* rather than returning one canned answer. A fixed reply made
 * the favourite click report "watching: true", so the watch toggle correctly computed its next action as DELETE
 * and the test read as a component bug. A toggle's next action depends on what the server last said, so a mock
 * that ignores which endpoint was called cannot exercise two toggles in one case.
 */
async function connect(
	page: Page,
	options: { initial?: Engagement; after?: Engagement; fail?: boolean } = {}
): Promise<Recorder> {
	const recorder: Recorder = { writes: [] };
	const initial = options.initial ?? engagement();
	let live = { ...initial };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		const path = url.pathname;

		if (/\/(rating|favourite|watch)$/.test(path)) {
			recorder.writes.push({
				method,
				path,
				body: method === 'PUT' ? route.request().postDataJSON() : null
			});
			if (options.fail) {
				return route.fulfill({
					status: 422,
					json: { reason: 'a rating is 1 to 5 stars; 9 is not' }
				});
			}
			if (options.after) {
				// An explicit answer, for the cases that are about one specific transition.
				live = { ...options.after };
				return route.fulfill({ json: live });
			}
			// Otherwise apply the write, so two toggles in one case behave as they would against the server.
			if (path.endsWith('/favourite')) {
				live = {
					...live,
					is_favourite: method === 'PUT',
					favourite_count: live.favourite_count + (method === 'PUT' ? 1 : -1)
				};
			} else if (path.endsWith('/watch')) {
				live = { ...live, is_watched: method === 'PUT' };
			}
			return route.fulfill({ json: live });
		}

		if (path === '/assets') {
			return route.fulfill({
				json: { items: [summary(), summary(OTHER)], total: 2, offset: 0 }
			});
		}
		if (path === '/favourites' || path === '/watches') {
			// Two rows, with the *second* asset first: the private lists are ordered by when the caller added
			// each one, and a page that re-sorted would show them the other way round.
			return route.fulfill({
				json: { items: [summary(OTHER), summary()], total: 2, offset: 0 }
			});
		}
		if (path === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (path === '/search' || path === '/search/facets') {
			return route.fulfill({
				json: path === '/search' ? { items: [summary()], total: 1, offset: 0 } : []
			});
		}
		if (path === '/categories' || path === '/schema/types' || path === '/shares') {
			return route.fulfill({ json: [] });
		}
		if (path.startsWith('/assets/') && path.endsWith('/categories')) {
			return route.fulfill({ json: [] });
		}
		if (path.startsWith('/assets/') && path.endsWith('/type')) {
			return route.fulfill({ json: { field_keys: [] } });
		}
		// Q.11's download panel asks this on every asset the detail view opens. Answered here so a suite about
		// something else does not fail on an unmocked route — and answered with formats, because a panel that
		// renders an empty list is not the panel these suites are meant to be indifferent to.
		if (path.endsWith('/usage-options') || path === '/usage-options') {
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
		if (path.endsWith('/download-options')) {
			return route.fulfill({
				json: { original_available: true, media_class: 'image', conversions: [] }
			});
		}
		if (path.startsWith('/assets/')) {
			// Keyed by which asset was asked for: the second one is deliberately untouched, so a panel that kept
			// the first one's stars would be visible.
			const id = path.split('/')[2] ?? ASSET;
			const state = id === OTHER ? engagement({ asset_id: OTHER }) : live;
			return route.fulfill({
				json: {
					...summary(id),
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
					engagement: state,
					preview_url: null
				}
			});
		}
		return route.fulfill({ status: 404, json: {} });
	});

	function summary(id: string = ASSET) {
		return {
			id,
			filename: id === OTHER ? 'quay.jpg' : 'harbour.jpg',
			mime: 'image/jpeg',
			bytes: 2_400_000,
			width: 4000,
			height: 3000,
			tier: 'hot',
			rights_state: 'allowed',
			provenance_state: 'none',
			thumbnail_url: null,
			tag_confidence: null,
			is_favourite: id === OTHER ? false : initial.is_favourite,
			average_stars: id === OTHER ? null : initial.average_stars,
			// Paperwork flag, as every summary now carries it (Q.9).
			has_attachment: false
		};
	}

	return recorder;
}

/** Opens the asset so the detail panel — and the engagement panel in it — is on screen. */
async function open(page: Page) {
	await page.goto('/assets');
	await page.getByRole('gridcell').first().dblclick();
	await expect(page.getByRole('region', { name: 'Ratings and favourites' })).toBeVisible();
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Ratings and favourites' });
}

test('an unrated asset says so rather than showing zero stars', async ({ page }) => {
	await connect(page);
	await open(page);

	// "Not yet rated" rather than a zero average: those are different facts, and a widget that drew them the
	// same way would be lying about one of them.
	await expect(panel(page).getByRole('status')).toContainText('Not yet rated');
	// And no clear control, because there is nothing to clear.
	await expect(panel(page).getByRole('button', { name: 'Clear' })).toHaveCount(0);
});

test('the rating is a radio group, and the average is stated in words', async ({ page }) => {
	await connect(page, {
		initial: engagement({ average_stars: 3.5, rating_count: 4, my_stars: 3 })
	});
	await open(page);

	// A group of five radios, one checked. As buttons these would be five unrelated controls with nothing
	// saying which is chosen.
	const radios = panel(page).getByRole('radio');
	await expect(radios).toHaveCount(5);
	await expect(radios.nth(2)).toBeChecked();

	// The average in words, because a screen reader gets nothing from five partially-filled stars — and 3.5 is
	// not 3, which is why the drawn stars are partially filled rather than rounded.
	await expect(panel(page).getByRole('status')).toContainText('Average 3.5 from 4 ratings');
});

test('rating sends the stars and redraws from the server answer', async ({ page }) => {
	const recorder = await connect(page, {
		initial: engagement(),
		after: engagement({ average_stars: 4, rating_count: 1, my_stars: 4 })
	});
	await open(page);

	await panel(page).getByRole('radio', { name: '4' }).check();

	await expect(panel(page).getByRole('status')).toContainText('Rated 4 of 5');
	// The average from the response, not a guess: it moved because of this request, and a guess could disagree.
	await expect(panel(page).getByRole('status')).toContainText(
		'The average is now 4.0 from 1 rating'
	);
	expect(recorder.writes).toEqual([
		{ method: 'PUT', path: `/assets/${ASSET}/rating`, body: { stars: 4 } }
	]);
});

test('clearing a rating is a DELETE, and appears only when there is one', async ({ page }) => {
	const recorder = await connect(page, {
		initial: engagement({ average_stars: 2, rating_count: 1, my_stars: 2 }),
		after: engagement()
	});
	await open(page);

	await panel(page).getByRole('button', { name: 'Clear' }).click();
	// Not a zero-star rating: "no opinion" and "thinks it is bad" must not share a representation.
	expect(recorder.writes).toEqual([
		{ method: 'DELETE', path: `/assets/${ASSET}/rating`, body: null }
	]);
	await expect(panel(page).getByRole('status')).toContainText('Nobody has rated this');
});

test('favourite and watch are pressed toggles that say what they do', async ({ page }) => {
	// No pinned answer: this case works two toggles, so the mock has to apply each write rather than reply with
	// one state that makes the second toggle look already-on.
	const recorder = await connect(page, { initial: engagement() });
	await open(page);

	const favourite = panel(page).getByRole('button', { name: /Favourite/ });
	// `aria-pressed` rather than colour alone: a toggle whose state is only visual is a toggle a screen-reader
	// user cannot read.
	await expect(favourite).toHaveAttribute('aria-pressed', 'false');
	await favourite.click();
	await expect(favourite).toHaveAttribute('aria-pressed', 'true');
	await expect(panel(page).getByRole('status')).toContainText('Added to your favourites');

	const watch = panel(page).getByRole('button', { name: /Watch/ });
	await watch.click();
	// The consequence, not the mechanism: "watching" means nothing on its own, and an eye icon means less.
	await expect(panel(page).getByRole('status')).toContainText('You will be told when this changes');

	expect(recorder.writes.map((w) => `${w.method} ${w.path.split('/').pop()}`)).toEqual([
		'PUT favourite',
		'PUT watch'
	]);
});

test('a count of people is shown and never a list of them', async ({ page }) => {
	await connect(page, {
		initial: engagement({ is_favourite: true, favourite_count: 7 })
	});
	await open(page);

	await expect(panel(page)).toContainText('7 people have favourited this');
	// And nothing about *who*. Nothing on this screen needs it, and "here they are" is a different disclosure.
	await expect(panel(page)).not.toContainText('@');
	// Watches have no count at all — how many colleagues are watching is closer to a fact about them.
	await expect(panel(page)).toContainText('Nobody is told how many people are watching');
});

test("a refusal is shown in the server's own words", async ({ page }) => {
	await connect(page, { fail: true });
	await open(page);

	await panel(page).getByRole('radio', { name: '3' }).check();
	await expect(panel(page).getByRole('alert')).toContainText('a rating is 1 to 5 stars');
});

test('the panel has no accessibility violations, rated or unrated', async ({ page }) => {
	await connect(page, {
		initial: engagement({
			average_stars: 3.5,
			rating_count: 4,
			my_stars: 3,
			is_favourite: true,
			favourite_count: 2,
			is_watched: true
		})
	});
	await open(page);

	const rated = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Ratings and favourites"]')
		.analyze();
	expect(rated.violations).toEqual([]);
});

test('the drawn stars fill partially rather than rounding', async ({ page }) => {
	// 3.4 and 3.5 are different numbers, and a widget that drew both as "three stars" would discard the very
	// distinction it exists to show. Measured from the fill widths, because that is the only place the claim
	// lives — nothing else on screen distinguishes the two.
	await connect(page, {
		initial: engagement({ average_stars: 3.5, rating_count: 4 })
	});
	await open(page);

	const widths = await panel(page)
		.locator('[data-testid="star-fill"]')
		.evaluateAll((nodes) => nodes.map((node) => (node as HTMLElement).style.width));
	expect(widths).toEqual(['100%', '100%', '100%', '50%', '0%']);
});

test('selecting another asset forgets the first one', async ({ page }) => {
	// The panel keeps the server's last answer as an override, so switching selection has to drop it. Otherwise
	// the second asset shows the first one's rating — with no symptom other than a wrong number.
	await connect(page, {
		initial: engagement({ average_stars: 5, rating_count: 2, my_stars: 5, is_favourite: true })
	});
	await open(page);
	await expect(panel(page).getByRole('status')).toContainText('Average 5.0 from 2 ratings');

	// Rate it, so there is an override to forget.
	await panel(page).getByRole('radio', { name: '4' }).check();
	await expect(panel(page).getByRole('status')).toContainText('Rated 4 of 5');

	await page.getByRole('gridcell').nth(1).dblclick();
	await expect(panel(page).getByRole('status')).toContainText('Not yet rated');
	await expect(panel(page).getByRole('button', { name: 'Clear' })).toHaveCount(0);
	await expect(panel(page).getByRole('button', { name: /Favourite/ })).toHaveAttribute(
		'aria-pressed',
		'false'
	);
});

test("the grid star toggles without stealing the grid's single tab stop", async ({ page }) => {
	const recorder = await connect(page, { initial: engagement() });
	await page.goto('/assets');

	const star = page.getByRole('button', { name: 'Add harbour.jpg to favourites' });
	await expect(star).toHaveAttribute('aria-pressed', 'false');
	// Not in the tab order. The WAI-ARIA grid pattern is one tab stop on the container, and a focusable control
	// per cell would make tabbing through a page of sixty assets sixty presses.
	await expect(star).toHaveAttribute('tabindex', '-1');

	await star.click();
	await expect(
		page.getByRole('button', { name: 'Remove harbour.jpg from favourites' })
	).toBeVisible();
	expect(recorder.writes).toEqual([
		{ method: 'PUT', path: `/assets/${ASSET}/favourite`, body: null }
	]);
});

test('clicking the star does not also select the asset', async ({ page }) => {
	await connect(page, { initial: engagement() });
	await page.goto('/assets');

	await page.getByRole('button', { name: 'Add harbour.jpg to favourites' }).click();
	// Without `stopPropagation` the click reaches the cell, which selects it and opens the panel — so a star
	// press would silently do two things.
	await expect(page.getByRole('region', { name: 'Ratings and favourites' })).toHaveCount(0);
});

test('f on the focused cell favourites it, and says so', async ({ page }) => {
	const recorder = await connect(page, { initial: engagement() });
	await page.goto('/assets');

	// `focus()` rather than `click()`, as the grid's other keyboard cases do: a click also selects the asset and
	// opens the detail panel, which moves focus out of the cell before the key arrives.
	await page.getByRole('gridcell').first().focus();
	await page.keyboard.press('f');

	// The visible end state *first*, then the recorder. Reading the recorder on the line after a keypress races
	// the fetch it triggers — the same flake this suite's bulk cases had, and it fails about as often as it
	// passes depending on machine load.
	//
	// Announced because focus stays on the cell: nothing the user is focused on changed state, so a silent
	// toggle would be indistinguishable from one that did nothing.
	await expect(
		page.getByRole('status').filter({ hasText: 'added to your favourites' })
	).toBeAttached();
	expect(recorder.writes).toEqual([
		{ method: 'PUT', path: `/assets/${ASSET}/favourite`, body: null }
	]);
});

test('ctrl+f is left to the browser', async ({ page }) => {
	const recorder = await connect(page, { initial: engagement() });
	await page.goto('/assets');

	await page.getByRole('gridcell').first().focus();
	await page.keyboard.press('Control+f');

	// Then a plain `f`, and wait for *that* to land. Asserting an empty recorder straight after ctrl+f would
	// pass whether the modifier was respected or the request was merely slow — so the plain press is what proves
	// the absence was about the modifier and not about timing.
	await page.keyboard.press('f');
	await expect(
		page.getByRole('status').filter({ hasText: 'added to your favourites' })
	).toBeAttached();

	// Find-in-page is a far more important key than this one, and stealing it would be a worse bug than not
	// having the shortcut at all.
	expect(recorder.writes).toHaveLength(1);
});

test('the favourites page shows the list in the order it was built', async ({ page }) => {
	await connect(page, { initial: engagement() });
	await page.goto('/favourites');

	await expect(page.getByRole('heading', { name: 'Favourites', level: 1 })).toBeVisible();
	// The count line, not the explanation above it — both say "most recently added first", because the page
	// explains the order and then labels it.
	await expect(page.getByText(/2\s+assets · most recently added first/)).toBeVisible();

	// The server's order, not re-sorted: `quay.jpg` is second in the library and first here.
	// By testid, not by class: `span.font-medium` also matched the rights badge, so the assertion was reading
	// ['quay.jpg', '✓ Cleared'] and passing for the wrong reason would have been a matter of luck.
	const names = await page.getByTestId('cell-filename').allTextContents();
	expect(names.map((name) => name.trim())).toEqual(['quay.jpg', 'harbour.jpg']);
});

test('removing from the favourites page takes the row away', async ({ page }) => {
	const recorder = await connect(page, { initial: engagement({ is_favourite: true }) });
	await page.goto('/favourites');

	await page
		.getByRole('button', { name: /from favourites/ })
		.first()
		.click();
	// The row *is* its membership here, unlike on the browse page where a star just toggles.
	await expect(page.getByTestId('cell-filename')).toHaveCount(1);
	expect(recorder.writes.map((w) => w.method)).toEqual(['DELETE']);
	await expect(
		page.getByRole('status').filter({ hasText: 'removed from your favourites' })
	).toBeAttached();
});

test('the watching page is honest that nothing is sent yet', async ({ page }) => {
	await connect(page, { initial: engagement() });
	await page.goto('/watches');

	await expect(page.getByRole('heading', { name: 'Watching', level: 1 })).toBeVisible();
	// A list of things you are "watching" with nothing ever arriving is worse than saying so.
	await expect(page.getByText('Notifications are not switched on yet')).toBeVisible();
	await expect(page.getByText('Nobody is told how many people watch an asset')).toBeVisible();
});

test('both private lists are reachable from the nav', async ({ page }) => {
	await connect(page, { initial: engagement() });
	await page.goto('/assets');

	const nav = page.getByRole('navigation', { name: 'Main' });
	await nav.getByRole('link', { name: 'Favourites' }).click();
	await expect(page.getByRole('heading', { name: 'Favourites', level: 1 })).toBeVisible();
	// `aria-current`, so a screen-reader user is told where they are and not only shown it.
	await expect(nav.getByRole('link', { name: 'Favourites' })).toHaveAttribute(
		'aria-current',
		'page'
	);

	await nav.getByRole('link', { name: 'Watching' }).click();
	await expect(page.getByRole('heading', { name: 'Watching', level: 1 })).toBeVisible();
});

test('the private lists have no accessibility violations', async ({ page }) => {
	await connect(page, { initial: engagement({ is_favourite: true, average_stars: 4.5 }) });
	await page.goto('/favourites');
	await expect(page.getByRole('gridcell').first()).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
