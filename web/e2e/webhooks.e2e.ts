/**
 * Webhooks through a real browser, against a mocked API (Q.20c).
 *
 * The db suite proves the outbox, `dam-connect` proves the signature over a real socket, and the API suite
 * proves the SSRF guard. What lives only here are four properties of the *screen*:
 *
 * - **The secret is shown once, in something that does not disappear.** A receiver cannot verify a delivery
 *   without it and it is never returned again, so a toast that fades would mean deleting the subscription to
 *   get another.
 * - **A disabled endpoint is the loudest thing on the page**, because until somebody notices it, nothing is
 *   being delivered at all — and it carries the system's own reason, which says whether enabling will help.
 * - **Retry appears only on an abandoned delivery.** The server refuses to revive one still in flight, since
 *   that would break per-asset ordering, and a button that 404s is worse than no button.
 * - **The receiver's own words are on screen.** A row saying "failed" with nothing else is a support ticket.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const HOOK = '88888888-8888-4888-8888-888888888888';

type Delivery = {
	id: string;
	event_kind: string;
	asset_id: string | null;
	state: string;
	attempts: number;
	response_status: number | null;
	last_error: string | null;
	created_at: string;
	delivered_at: string | null;
	next_attempt_at: string;
};

function delivery(overrides: Partial<Delivery> = {}): Delivery {
	return {
		id: '99999999-9999-4999-8999-999999999999',
		event_kind: 'asset.published',
		asset_id: '11111111-1111-4111-8111-111111111111',
		state: 'delivered',
		attempts: 1,
		response_status: 204,
		last_error: null,
		created_at: '2026-08-23T09:00:00Z',
		delivered_at: '2026-08-23T09:00:01Z',
		next_attempt_at: '2026-08-23T09:00:00Z',
		...overrides
	};
}

function hook(overrides: Record<string, unknown> = {}) {
	return {
		id: HOOK,
		url: 'https://example.com/damrs/hook',
		event_kinds: [],
		active: true,
		disabled_reason: null,
		consecutive_failures: 0,
		created_at: '2026-08-20T09:00:00Z',
		...overrides
	};
}

async function connect(
	page: Page,
	options: {
		hooks?: Record<string, unknown>[];
		deliveries?: Delivery[];
		createRefusal?: string;
	} = {}
) {
	const recorder = { created: [] as unknown[], retried: 0, enabled: 0 };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const path = new URL(route.request().url()).pathname;
		const method = route.request().method();

		if (path === '/webhooks' && method === 'GET') {
			return route.fulfill({ json: options.hooks ?? [hook()] });
		}
		if (path === '/webhooks' && method === 'POST') {
			if (options.createRefusal) {
				return route.fulfill({ status: 422, json: { reason: options.createRefusal } });
			}
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.created.push(body);
			return route.fulfill({
				status: 201,
				json: {
					...hook({ url: body.url, event_kinds: body.event_kinds }),
					secret: 'damrs_whsec_a_very_long_generated_signing_key',
					signature_note:
						'Each delivery carries X-Damrs-Signature as "v1=<hex>", the HMAC-SHA256 of ' +
						'"<X-Damrs-Timestamp>.<body>" under this secret. Reject a timestamp older than a few ' +
						'minutes: the timestamp is what stops a captured delivery being replayed.'
				}
			});
		}
		if (path.endsWith('/enable')) {
			recorder.enabled += 1;
			return route.fulfill({ json: hook({ active: true }) });
		}
		if (path.endsWith('/retry')) {
			recorder.retried += 1;
			return route.fulfill({ status: 202, body: '' });
		}
		if (path.endsWith('/deliveries')) {
			return route.fulfill({ json: options.deliveries ?? [delivery()] });
		}
		if (method === 'DELETE') return route.fulfill({ status: 204, body: '' });
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('the signing key is shown once, in a panel that stays', async ({ page }) => {
	const recorder = await connect(page, { hooks: [] });
	await page.goto('/webhooks');

	await page.getByLabel('Endpoint URL').fill('https://example.com/damrs/hook');
	await page.getByRole('button', { name: 'Register' }).click();

	// Present, selectable, and accompanied by the recipe — the server sends the sentence so the screen and the
	// signing code cannot drift apart.
	await expect(page.getByText('Save this signing key')).toBeVisible();
	await expect(page.getByText('damrs_whsec_a_very_long_generated_signing_key')).toBeVisible();
	await expect(page.getByText('HMAC-SHA256', { exact: false })).toContainText('replayed');

	// It goes only when acknowledged. A toast would mean deleting the subscription to get another.
	await page.getByRole('button', { name: 'I have saved it' }).click();
	await expect(page.getByText('Save this signing key')).toHaveCount(0);

	// No kinds ticked means all of them, which is what the server stores.
	expect(recorder.created).toEqual([{ url: 'https://example.com/damrs/hook', event_kinds: [] }]);
});

test('a chosen subset of events is sent as chosen', async ({ page }) => {
	const recorder = await connect(page, { hooks: [] });
	await page.goto('/webhooks');

	await page.getByLabel('Endpoint URL').fill('https://example.com/hook');
	await page.getByRole('checkbox').first().check();
	await page.getByRole('button', { name: 'Register' }).click();
	await expect(page.getByText('Save this signing key')).toBeVisible();

	const sent = recorder.created[0] as Record<string, unknown>;
	expect(sent.event_kinds).toEqual(['asset.published']);
});

test('a refused URL shows the rule it broke', async ({ page }) => {
	// The server's sentence names which of the rules failed — https, credentials, or a private address — and
	// that is more useful than anything this page could say instead.
	await connect(page, {
		hooks: [],
		createRefusal:
			'169.254.169.254 is a private or link-local address, and this server will not post to one even in development: that range carries the cloud metadata service'
	});
	await page.goto('/webhooks');
	await page.getByLabel('Endpoint URL').fill('https://169.254.169.254/hook');
	await page.getByRole('button', { name: 'Register' }).click();

	await expect(page.getByRole('alert')).toContainText('cloud metadata service');
});

test('a disabled endpoint says so, and why, and offers to enable it', async ({ page }) => {
	const recorder = await connect(page, {
		hooks: [
			hook({
				active: false,
				consecutive_failures: 5,
				disabled_reason:
					'disabled automatically after 5 deliveries were abandoned; the last error was: could not connect'
			})
		]
	});
	await page.goto('/webhooks');

	// The consequence, not the state: until somebody acts, nothing is being delivered at all.
	await expect(page.getByText('disabled — nothing is being delivered')).toBeVisible();
	await expect(page.getByText('disabled automatically after 5')).toContainText('could not connect');
	await expect(page.getByText('5 abandoned in a row')).toBeVisible();

	await page.getByRole('button', { name: 'Enable' }).click();
	await expect(page.getByRole('status')).toContainText('enabled');
	expect(recorder.enabled).toBe(1);
});

test('an active endpoint offers no enable button', async ({ page }) => {
	await connect(page);
	await page.goto('/webhooks');
	await expect(page.getByText('active')).toBeVisible();
	await expect(page.getByRole('button', { name: 'Enable' })).toHaveCount(0);
});

test('retry appears only on an abandoned delivery, with the reason it failed', async ({ page }) => {
	const recorder = await connect(page, {
		deliveries: [
			delivery({
				id: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
				state: 'dead',
				attempts: 8,
				response_status: null,
				last_error: 'no answer within 10s',
				delivered_at: null
			}),
			delivery({
				id: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb',
				state: 'failed',
				attempts: 2,
				response_status: 503,
				last_error: 'server error: deploying',
				delivered_at: null
			}),
			delivery()
		]
	});
	await page.goto('/webhooks');
	await page.getByRole('button', { name: 'Deliveries' }).click();

	// States in words an operator can act on, not the raw column.
	await expect(page.getByText('abandoned')).toBeVisible();
	await expect(page.getByText('failed, will retry')).toBeVisible();
	await expect(page.getByText('accepted')).toBeVisible();

	// The receiver's own words, and the network's. A row saying "failed" without them is a support ticket.
	await expect(page.getByText('no answer within 10s')).toBeVisible();
	await expect(page.getByText('server error: deploying')).toBeVisible();
	// A timeout shows no status, because absent and zero are different diagnoses.
	await expect(page.getByText('HTTP 503')).toBeVisible();
	await expect(page.getByText('HTTP 0')).toHaveCount(0);

	// One button, on the abandoned one only: the server refuses to revive anything still in flight, because
	// that would break the per-asset ordering — and a button that 404s is worse than no button.
	await expect(page.getByRole('button', { name: 'Retry' })).toHaveCount(1);
	await page.getByRole('button', { name: 'Retry' }).click();
	await expect(page.getByRole('status')).toContainText('another round of attempts');
	expect(recorder.retried).toBe(1);
});

test('an empty page says what that means', async ({ page }) => {
	await connect(page, { hooks: [] });
	await page.goto('/webhooks');

	// Not just "none": the useful part is that the events are still recorded per asset, so nothing is lost by
	// having no endpoint.
	await expect(page.getByText('No endpoints registered', { exact: false })).toContainText(
		"still recorded on each asset's history"
	);
});

test('the page explains that events carry ids rather than bytes', async ({ page }) => {
	// §11's premise, on the screen where somebody decides what to build against it: a receiver reads the asset
	// through the API with its own credential, which is what makes withdrawing rights take effect downstream.
	await connect(page);
	await page.goto('/webhooks');
	await expect(page.getByText('Events carry ids, never bytes', { exact: false })).toContainText(
		'withdrawing rights takes effect downstream'
	);
});

test('the webhooks screen has no axe violations', async ({ page }) => {
	await connect(page, {
		hooks: [
			hook(),
			hook({
				id: '77777777-7777-4777-8777-777777777777',
				url: 'https://other.example/hook',
				active: false,
				disabled_reason: 'disabled automatically after 5 deliveries were abandoned'
			})
		]
	});
	await page.goto('/webhooks');
	await page.getByRole('button', { name: 'Deliveries' }).first().click();
	await expect(page.getByText('accepted')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
