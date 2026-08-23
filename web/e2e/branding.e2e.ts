/**
 * Site branding through a real browser, against a mocked API (Q.20d).
 *
 * The API suite proves the fallback, the colour rule and the logo scope check. What lives only here are four
 * properties of the *screen*, two of which are fixes rather than preferences:
 *
 * - **The name field is empty when the name is a fallback.** Pre-filling it with the organisation's name would
 *   make the default look chosen, after which clearing it is indistinguishable from never setting it.
 * - **No flash of the vendor's name.** The header shows nothing until branding loads. The first version fell
 *   back to "damrs", which put the vendor's name into every page load of every customer's library — briefly,
 *   which is still the thing this feature exists to remove.
 * - **Saving updates the header without a reload**, because otherwise somebody saves a name and has to refresh
 *   to believe it.
 * - **The accent has a swatch**, because `#ff6600` is not a colour anybody can see — and the copy says where it
 *   actually applies, which is mostly portals rather than this application.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

/** The decorative mark beside the library name, which carries the accent in force. */
function mark(page: Page) {
	return page.locator('nav a span[aria-hidden]').first();
}

function branding(overrides: Record<string, unknown> = {}) {
	return {
		site_name: 'Acme Corporation',
		site_name_is_default: true,
		logo_asset_id: null,
		logo_url: null,
		accent: '#2563eb',
		support_email: null,
		...overrides
	};
}

async function connect(
	page: Page,
	options: { current?: Record<string, unknown>; refusal?: string; slow?: boolean } = {}
) {
	const recorder = { saved: [] as unknown[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;
		const method = route.request().method();

		if (path === '/branding' && method === 'GET') {
			if (options.slow) await new Promise((resolve) => setTimeout(resolve, 400));
			return route.fulfill({ json: options.current ?? branding() });
		}
		if (path === '/branding' && method === 'PUT') {
			if (options.refusal) {
				return route.fulfill({ status: 422, json: { reason: options.refusal } });
			}
			const body = route.request().postDataJSON() as Record<string, string | null>;
			recorder.saved.push(body);
			// The server normalises: lowercased accent, trimmed strings, and an empty name resolved back to the
			// organisation's. Echoing the request would hide all three.
			const name = (body.site_name ?? '').trim();
			return route.fulfill({
				json: branding({
					site_name: name === '' ? 'Acme Corporation' : name,
					site_name_is_default: name === '',
					accent: (body.accent ?? '').toLowerCase(),
					support_email: body.support_email,
					logo_asset_id: body.logo_asset_id
				})
			});
		}
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('an unset name shows as empty with the fallback as its placeholder', async ({ page }) => {
	await connect(page);
	await page.goto('/branding');

	const field = page.getByLabel('Library name');
	await expect(field).toHaveValue('');
	// The fallback is the placeholder, which is exactly what it is — and the note names it, so nobody has to
	// guess where the header's text is coming from.
	await expect(field).toHaveAttribute('placeholder', 'Acme Corporation');
	await expect(page.getByText("your organisation's name is used")).toContainText(
		'Acme Corporation'
	);
});

test('a name that was chosen is shown, and can be cleared back to the fallback', async ({
	page
}) => {
	const recorder = await connect(page, {
		current: branding({ site_name: 'Acme Picture Library', site_name_is_default: false })
	});
	await page.goto('/branding');

	await expect(page.getByLabel('Library name')).toHaveValue('Acme Picture Library');
	await expect(page.getByText('Clear it to fall back')).toBeVisible();

	await page.getByLabel('Library name').fill('');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(page.getByRole('status')).toContainText('Saved');
	// Back to the fallback, and the field empty again rather than pre-filled with what the server resolved.
	await expect(page.getByLabel('Library name')).toHaveValue('');
	await expect(page.getByText("your organisation's name is used")).toBeVisible();
	expect((recorder.saved[0] as Record<string, unknown>).site_name).toBe('');
});

test('the header takes the new name without a reload', async ({ page }) => {
	await connect(page);
	await page.goto('/branding');
	// The shell reads the same store, so the change lands where it matters — otherwise somebody saves a name
	// and has to refresh to believe it.
	await expect(page.locator('nav a').first()).toHaveText('Acme Corporation');

	await page.getByLabel('Library name').fill('Acme Picture Library');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(page.locator('nav a').first()).toHaveText('Acme Picture Library');
});

test('the header never shows the vendors name while branding loads', async ({ page }) => {
	// The regression this exists for. The first version fell back to "damrs", so every page load of every
	// customer's library flashed the vendor's name — the exact thing this feature removes, just briefly.
	// Driving the real page is what found it: "immediately: damrs | settled: Acme Picture Library".
	await connect(page, { slow: true });
	await page.goto('/branding');

	const home = page.locator('nav a').first();
	// Nothing *visible*, and specifically not the vendor's name. Asserted on the rendered text rather than on
	// `toHaveText('')`, because the link deliberately carries an `sr-only` name — removing the fallback
	// outright left it with no accessible name at all, which axe reported as `link-name (serious)` on every
	// screen in the application. A visual fix that breaks a screen reader is a worse bug than the one it fixed.
	await expect(home).not.toContainText('damrs');
	await expect(home.locator('span:not(.sr-only)')).toHaveCount(1);
	// The link is still there to click, because the decorative mark holds it open — and it is still named, for
	// anybody not looking at it.
	await expect(home).toBeVisible();
	await expect(mark(page)).toBeVisible();
	await expect(home).toHaveAccessibleName('Home');

	await expect(home).toContainText('Acme Corporation');
	await expect(home).toHaveAccessibleName('Acme Corporation');
});

test('the accent has a swatch, and the copy says where it applies', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/branding');

	// Two inputs on one value: a picker is how anybody chooses a colour, a text field is how somebody pastes a
	// hex out of a brand guideline.
	const picker = page.locator('input[type=color]');
	await expect(picker).toHaveValue('#2563eb');
	await picker.fill('#FF6600');
	await expect(page.locator('input[pattern]')).toHaveValue('#ff6600');

	// The header mark still shows the colour *in force*, not the draft — and that is the right way round.
	// A header that live-previewed an unsaved value would be telling you the library looks like something it
	// does not. The native picker is the draft preview; the mark is the state.
	//
	// Asserted on the computed colour rather than the style attribute: the browser rewrites an inline hex to
	// `rgb(...)` once it has been through a style recalculation, so matching the attribute passes before the
	// first update and fails after it — which is exactly the confusing half-pass this started as.
	await expect(mark(page)).toHaveCSS('background-color', 'rgb(37, 99, 235)');
	// And the honest scope: the app's own colours are left alone because their contrast pairs are tuned.
	await expect(page.getByText('It appears on your portals')).toContainText('tuned for contrast');

	await page.getByRole('button', { name: 'Save' }).click();
	await expect(page.getByRole('status')).toContainText('Saved');
	expect((recorder.saved[0] as Record<string, unknown>).accent).toBe('#ff6600');
	// And once saved, the mark is the new colour.
	await expect(mark(page)).toHaveCSS('background-color', 'rgb(255, 102, 0)');
});

test('a malformed colour never reaches the server', async ({ page }) => {
	// The `pattern` attribute means the browser refuses the submit, so the round trip does not happen at all.
	// Worth asserting rather than assuming: it is why the server's colour refusal is effectively unreachable
	// from this form, and the server check remains because the API is reachable without it.
	const recorder = await connect(page);
	await page.goto('/branding');
	await page.locator('input[pattern]').fill('#25e');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(page.getByRole('status')).toHaveText('');
	expect(recorder.saved).toHaveLength(0);
});

test('empty optional fields are sent as absent rather than as empty strings', async ({ page }) => {
	// A form that posts every field would otherwise store "" and put an empty mailto in a portal footer, and
	// send "" where the server expects a uuid.
	const recorder = await connect(page);
	await page.goto('/branding');
	await page.getByRole('button', { name: 'Save' }).click();
	await expect(page.getByRole('status')).toContainText('Saved');

	const sent = recorder.saved[0] as Record<string, unknown>;
	expect(sent.support_email).toBeNull();
	expect(sent.logo_asset_id).toBeNull();
});

test('a refusal shows the rule that failed', async ({ page }) => {
	// Through the logo rather than the colour, because that is the refusal a form can actually provoke: an id
	// that parses fine and belongs to an asset this caller cannot see. The server's sentence is shown as it
	// arrived — it names which rule failed, which is more than this page could work out.
	await connect(page, { refusal: 'that logo is not an asset you can see' });
	await page.goto('/branding');
	await page.getByLabel('Logo asset id').fill('11111111-1111-4111-8111-111111111111');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(page.getByRole('alert')).toContainText('not an asset you can see');
});

test('the logo field explains why it takes an id rather than an upload', async ({ page }) => {
	await connect(page);
	await page.goto('/branding');
	// A second upload path for a logo would be a second place for an unlicensed image to sit unnoticed, which
	// is the whole argument — so the screen makes it rather than looking like a missing feature.
	await expect(page.getByText('A logo is an asset')).toContainText('rights-checked');
});

test('the branding screen has no axe violations', async ({ page }) => {
	await connect(page, {
		current: branding({ site_name: 'Acme Picture Library', site_name_is_default: false })
	});
	await page.goto('/branding');
	await expect(page.getByLabel('Library name')).toHaveValue('Acme Picture Library');
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
