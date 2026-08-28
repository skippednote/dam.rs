/**
 * Connected sites through a real browser (M3d, §11).
 *
 * The Rust suites prove the secret lifecycle and every delivery bound. What lives only here are properties of
 * the *screen*, and all of them are about consequence:
 *
 * - **Two credentials, shown once, in a panel that stays.** Neither can be read back, so this is the only place
 *   either exists. A toast is the wrong shape for something somebody has to paste into another system.
 * - **Rotation is two buttons, not a checkbox with a default.** A scheduled rotation and a leak want opposite
 *   answers, and a default is wrong half the time in the direction that matters.
 * - **What a site may have is written in words.** Two unlabelled ticks marked "original" and "restore" is the
 *   version of this form where somebody grants master access to a public website by accident.
 * - **Revoking says it is permanent and offers pausing instead.** The secret is already out there.
 * - **A grace window is stated as a state, not as an event.** "Rotated on Tuesday" is not actionable; "the old
 *   secret still works, so this site keeps rendering until it deploys" is.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Connector = {
	id: string;
	kind: string;
	label: string;
	site_url: string;
	remote_version: string | null;
	status: string;
	may_render: boolean;
	allow_original: boolean;
	allow_restore: boolean;
	all_asset_groups: boolean;
	asset_group_ids: string[];
	last_seen_at: string | null;
	last_error: string | null;
	secret_rotated_at: string | null;
	previous_secret_live: boolean;
	created_at: string;
};

function site(overrides: Partial<Connector> = {}): Connector {
	return {
		id: 'aaaaaaaa-1111-4111-8111-111111111111',
		kind: 'drupal',
		label: 'Marketing site',
		site_url: 'https://www.example.com',
		remote_version: 'drupal 11.1 / damrs_dam 1.0.0',
		status: 'active',
		may_render: true,
		allow_original: false,
		allow_restore: false,
		all_asset_groups: false,
		asset_group_ids: ['b1111111-1111-4111-8111-111111111111'],
		last_seen_at: '2026-08-24T09:00:00Z',
		last_error: null,
		secret_rotated_at: null,
		previous_secret_live: false,
		created_at: '2026-08-20T09:00:00Z',
		...overrides
	};
}

const WARNING =
	'The API key and signing secret are shown only here. The key is stored as a hash and the secret is ' +
	'encrypted at rest, so neither can be read back — a lost one is replaced, not recovered.';

async function connect(page: Page, options: { sites?: Connector[]; refuse?: number } = {}) {
	const recorder = {
		registered: [] as Record<string, unknown>[],
		rotated: [] as { id: string; keep_previous: boolean }[],
		statuses: [] as { id: string; status: string }[]
	};

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/connectors' && method === 'GET') {
			if (options.refuse) {
				return route.fulfill({ status: options.refuse, json: { reason: 'refused' } });
			}
			return route.fulfill({ json: options.sites ?? [site()] });
		}
		if (url.pathname === '/connectors' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.registered.push(body);
			return route.fulfill({
				status: 201,
				json: {
					connector: site({
						label: String(body.label),
						site_url: String(body.site_url),
						allow_original: Boolean(body.allow_original),
						allow_restore: Boolean(body.allow_restore),
						all_asset_groups: Boolean(body.all_asset_groups)
					}),
					api_key: 'damrs_abcdef0123456789',
					signing_secret: 'damrs_fedcba9876543210',
					warning: WARNING
				}
			});
		}
		if (url.pathname.endsWith('/rotate')) {
			const body = route.request().postDataJSON() as { keep_previous: boolean };
			recorder.rotated.push({ id: url.pathname.split('/')[2] ?? '', ...body });
			return route.fulfill({
				json: {
					connector: site({
						secret_rotated_at: '2026-08-24T10:00:00Z',
						previous_secret_live: body.keep_previous
					}),
					// Empty: a secret rotation leaves the API key alone, and the blank is what says so.
					api_key: '',
					signing_secret: 'damrs_0000rotated0000',
					warning: WARNING
				}
			});
		}
		if (url.pathname.endsWith('/status')) {
			const body = route.request().postDataJSON() as { status: string };
			recorder.statuses.push({ id: url.pathname.split('/')[2] ?? '', ...body });
			return route.fulfill({ json: site({ status: body.status, may_render: false }) });
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the page says what a connected site is, and why it matters', async ({ page }) => {
	// Not decoration. "Reference, not copy" is the reason the whole feature exists, and an operator deciding
	// what to grant a website needs to know that an expiring licence takes effect there too.
	await connect(page);
	await page.goto('/connectors');
	await expect(page.getByText('never keeps a copy of the file', { exact: false })).toContainText(
		'the image stops rendering there'
	);
});

test('both credentials appear once, in a panel that stays until dismissed', async ({ page }) => {
	const recorder = await connect(page, { sites: [] });
	await page.goto('/connectors');

	await page.getByTestId('connect').click();
	await page.getByTestId('label').fill('Marketing site');
	await page.getByTestId('site-url').fill('https://www.example.com');
	await page.getByTestId('submit').click();

	const panel = page.getByTestId('issued');
	await expect(panel).toBeVisible();
	await expect(panel.getByTestId('issued-key')).toHaveValue('damrs_abcdef0123456789');
	await expect(panel.getByTestId('issued-secret')).toHaveValue('damrs_fedcba9876543210');
	// The server's own sentence, not a paraphrase: it is the one that says neither can be recovered.
	await expect(panel).toContainText('shown only here');
	await expect(panel).toContainText('replaced, not recovered');

	// It survives until acknowledged — this is something somebody has to paste into another system.
	await page.getByRole('button', { name: 'I have copied both' }).click();
	await expect(panel).toHaveCount(0);

	expect(recorder.registered).toEqual([
		{
			kind: 'drupal',
			label: 'Marketing site',
			site_url: 'https://www.example.com',
			all_asset_groups: false,
			allow_original: false,
			allow_restore: false
		}
	]);
});

test('the form says what each grant costs rather than showing two ticks', async ({ page }) => {
	// The version of this form with unlabelled checkboxes is the one where somebody gives a public website
	// master access by accident.
	await connect(page, { sites: [] });
	await page.goto('/connectors');
	await page.getByTestId('connect').click();

	await expect(
		page.getByText('can leak the deliverable a customer paid for', { exact: false })
	).toBeVisible();
	await expect(
		page.getByText('a retrieval nobody asked for and somebody pays for', { exact: false })
	).toBeVisible();
	await expect(page.getByText('cannot publish what it should not', { exact: false })).toBeVisible();

	// And it will not submit half a registration.
	await expect(page.getByTestId('submit')).toBeDisabled();
	await page.getByTestId('label').fill('X');
	await expect(page.getByTestId('submit')).toBeDisabled();
	await page.getByTestId('site-url').fill('https://x.example.com');
	await expect(page.getByTestId('submit')).toBeEnabled();
});

test('rotation is two buttons because the two situations want opposite answers', async ({
	page
}) => {
	const recorder = await connect(page);
	await page.goto('/connectors');
	const id = 'aaaaaaaa-1111-4111-8111-111111111111';

	await page.getByTestId(`rotate-${id}`).click();
	await expect(page.getByRole('status')).toContainText('keeps working for a week');
	await expect(page.getByTestId('issued-secret')).toHaveValue('damrs_0000rotated0000');
	// A secret rotation leaves the API key alone, so there is no key field to copy.
	await expect(page.getByTestId('issued-key')).toHaveCount(0);

	await page.getByTestId(`rotate-now-${id}`).click();
	await expect(page.getByRole('status')).toContainText('will render nothing until it deploys');

	expect(recorder.rotated).toEqual([
		{ id, keep_previous: true },
		{ id, keep_previous: false }
	]);
});

test('a live grace window is stated as a state somebody can act on', async ({ page }) => {
	// "Rotated on Tuesday" is not actionable. "The previous secret still works, so this site keeps rendering
	// until it deploys" is.
	await connect(page, {
		sites: [site({ secret_rotated_at: '2026-08-24T10:00:00Z', previous_secret_live: true })]
	});
	await page.goto('/connectors');
	await expect(page.getByTestId('grace-aaaaaaaa-1111-4111-8111-111111111111')).toContainText(
		'until it deploys the new one'
	);
});

test('revoking says it is permanent and offers pausing instead', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/connectors');
	const id = 'aaaaaaaa-1111-4111-8111-111111111111';

	await page.getByTestId(`revoke-${id}`).click();
	// Inline, not a confirm(), which blocks the automation driving this page.
	await expect(page.getByText('Revoking is permanent', { exact: false })).toContainText(
		'pause it instead if you might want it later'
	);
	await page.getByTestId(`revoke-confirm-${id}`).click();
	await expect(page.getByRole('status')).toContainText('its URLs stopped working');

	expect(recorder.statuses).toEqual([{ id, status: 'revoked' }]);
});

test('pausing is the reversible one and says what it did', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/connectors');
	const id = 'aaaaaaaa-1111-4111-8111-111111111111';

	await page.getByTestId(`pause-${id}`).click();
	await expect(page.getByRole('status')).toContainText('images stopped rendering');
	expect(recorder.statuses).toEqual([{ id, status: 'paused' }]);
});

test('a site with no groups says it will show nothing rather than looking configured', async ({
	page
}) => {
	await connect(page, { sites: [site({ asset_group_ids: [], all_asset_groups: false })] });
	await page.goto('/connectors');
	await expect(page.getByText('sees nothing yet — grant it an asset group')).toBeVisible();
});

test('an error state says the images are still rendering', async ({ page }) => {
	// The distinction that matters: something went wrong, and the site's home page is still fine. Conflating
	// them would have an operator scrambling over a failed webhook.
	await connect(page, {
		sites: [site({ status: 'error', may_render: true, last_error: 'webhook returned 500' })]
	});
	await page.goto('/connectors');
	await expect(page.getByTestId('status-aaaaaaaa-1111-4111-8111-111111111111')).toHaveText('error');
	await expect(page.getByText('Last problem: webhook returned 500')).toBeVisible();
	await expect(page.getByText('its images are not rendering')).toHaveCount(0);
});

test('a paused site says its images are not rendering', async ({ page }) => {
	await connect(page, { sites: [site({ status: 'paused', may_render: false })] });
	await page.goto('/connectors');
	await expect(page.getByText('its images are not rendering')).toBeVisible();
});

test('a revoked site offers nothing to do', async ({ page }) => {
	await connect(page, { sites: [site({ status: 'revoked', may_render: false })] });
	await page.goto('/connectors');
	const id = 'aaaaaaaa-1111-4111-8111-111111111111';
	// Terminal, so there are no controls at all rather than ones that fail on click.
	await expect(page.getByTestId(`rotate-${id}`)).toHaveCount(0);
	await expect(page.getByTestId(`pause-${id}`)).toHaveCount(0);
	await expect(page.getByTestId(`revoke-${id}`)).toHaveCount(0);
});

test('an empty list explains what connecting a site is for', async ({ page }) => {
	await connect(page, { sites: [] });
	await page.goto('/connectors');
	await expect(page.getByText('Nothing is connected', { exact: false })).toContainText(
		'without copying the files'
	);
});

test('a caller without Manage is told which permission is missing', async ({ page }) => {
	await connect(page, { refuse: 403 });
	await page.goto('/connectors');
	await expect(page.getByRole('alert')).toContainText('does not hold Manage');
});

test('the connected-sites screen has no axe violations', async ({ page }) => {
	await connect(page, {
		sites: [
			site(),
			site({
				id: 'b1111111-1111-4111-8111-111111111111',
				label: 'Campaign microsite',
				status: 'paused',
				may_render: false,
				all_asset_groups: true,
				allow_original: true,
				previous_secret_live: true,
				secret_rotated_at: '2026-08-24T10:00:00Z',
				last_error: 'webhook returned 500'
			})
		]
	});
	await page.goto('/connectors');
	await expect(page.getByTestId('sites').locator('li')).toHaveCount(2);
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);

	// With the form and the credentials panel open, which is where the interactive controls are.
	await page.getByTestId('connect').click();
	await expect(page.getByTestId('kind')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
