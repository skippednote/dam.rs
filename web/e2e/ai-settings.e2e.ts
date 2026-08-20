/**
 * The AI configuration screen, through a real browser against a mocked API (M5a·4).
 *
 * The Rust suites prove the refusals, the sealing and the arithmetic. What belongs here is the browser half,
 * and on this screen that half is unusually load-bearing:
 *
 * - **A key must not survive the form.** The field is cleared whether the request succeeded or failed, and the
 *   page must never render the key it just sent — a hint is the whole disclosure.
 * - **The endpoint requirement is visible before submission**, because "openai-compatible needs a base url" is
 *   a 422 the user can be spared.
 * - **A verification result says which of three things happened** — it worked, the credential was rejected, or
 *   the model declined — and the third means the key is fine, which is the distinction that saves somebody
 *   re-issuing a working key.
 * - **No axe violations, in both themes.**
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Credential = {
	id: string;
	provider: string;
	label: string;
	base_url: string | null;
	hint: string;
	default_model: string;
	is_active: boolean;
	is_default: boolean;
	needs_resealing: boolean;
	created_at: string;
};

function credential(overrides: Partial<Credential> = {}): Credential {
	return {
		id: 'cred-1',
		provider: 'anthropic',
		label: "Ada's key",
		base_url: null,
		hint: '…9911',
		default_model: 'claude-opus-5',
		is_active: true,
		is_default: true,
		needs_resealing: false,
		created_at: '2026-08-20T10:00:00Z',
		...overrides
	};
}

type Recorder = {
	added: Record<string, unknown>[];
	rotated: { id: string; body: Record<string, unknown> }[];
	budgets: Record<string, unknown>[];
	verifies: string[];
};

async function connect(
	page: Page,
	options: {
		credentials?: Credential[];
		budget?: Record<string, unknown>;
		verify?: Record<string, unknown>;
		addReply?: { status: number; json: unknown };
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = { added: [], rotated: [], budgets: [], verifies: [] };
	let credentials = options.credentials ?? [];
	let budget = options.budget ?? {
		limit_cents: null,
		enforcement: 'soft',
		warn_at_fraction: 0.8,
		used_cents: 0,
		period_start: '2026-08-01',
		state: 'allowed'
	};

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/ai/credentials' && method === 'GET') {
			return route.fulfill({ json: credentials });
		}
		if (url.pathname === '/ai/credentials' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.added.push(body);
			if (options.addReply) return route.fulfill(options.addReply);
			const key = String(body.api_key ?? '');
			const created = credential({
				id: `cred-${credentials.length + 1}`,
				label: String(body.label),
				provider: String(body.provider),
				base_url: (body.base_url as string | null) ?? null,
				default_model: String(body.default_model),
				hint: `…${key.slice(-4)}`,
				is_default: body.make_default === true
			});
			credentials = [
				...credentials.map((row) =>
					body.make_default === true ? { ...row, is_default: false } : row
				),
				created
			];
			return route.fulfill({ status: 201, json: created });
		}
		if (url.pathname.endsWith('/key') && method === 'PUT') {
			const id = url.pathname.split('/')[3] ?? '';
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.rotated.push({ id, body });
			const key = String(body.api_key ?? '');
			credentials = credentials.map((row) =>
				row.id === id ? { ...row, hint: `…${key.slice(-4)}` } : row
			);
			return route.fulfill({ json: credentials.find((row) => row.id === id) });
		}
		if (url.pathname.endsWith('/default') && method === 'PATCH') {
			const id = url.pathname.split('/')[3] ?? '';
			credentials = credentials.map((row) => ({ ...row, is_default: row.id === id }));
			return route.fulfill({ json: credentials.find((row) => row.id === id) });
		}
		if (url.pathname.endsWith('/active') && method === 'PATCH') {
			const id = url.pathname.split('/')[3] ?? '';
			const body = route.request().postDataJSON() as { is_active: boolean };
			credentials = credentials.map((row) =>
				row.id === id
					? { ...row, is_active: body.is_active, is_default: row.is_default && body.is_active }
					: row
			);
			return route.fulfill({ json: credentials.find((row) => row.id === id) });
		}
		if (url.pathname.endsWith('/verify') && method === 'POST') {
			recorder.verifies.push(url.pathname.split('/')[3] ?? '');
			return route.fulfill({
				json: options.verify ?? {
					ok: true,
					model: 'claude-opus-5',
					detail: 'ready',
					worth_retrying: false
				}
			});
		}
		if (url.pathname === '/ai/budget' && method === 'GET') {
			return route.fulfill({ json: budget });
		}
		if (url.pathname === '/ai/budget' && method === 'PUT') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.budgets.push(body);
			budget = {
				...budget,
				limit_cents: body.limit_cents,
				enforcement: body.hard === true ? 'hard' : 'soft'
			};
			return route.fulfill({ json: budget });
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('a stored key never appears on the screen again', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/settings/ai');

	await page.getByLabel('Name').fill('Marketing key');
	await page.getByLabel('Provider key').fill('sk-test-not-a-credential-4242');
	await page.getByRole('button', { name: 'Store key' }).click();

	await expect(page.getByRole('status')).toContainText('ending …4242');
	expect(recorder.added).toHaveLength(1);
	expect(recorder.added[0]).toMatchObject({
		provider: 'anthropic',
		label: 'Marketing key',
		api_key: 'sk-test-not-a-credential-4242',
		// Empty means absent: an endpoint of "" would resolve to nothing.
		base_url: null
	});

	// The field is cleared, and the whole page contains no trace of the key beyond the hint.
	await expect(page.getByLabel('Provider key')).toHaveValue('');
	const body = (await page.locator('body').textContent()) ?? '';
	expect(body).not.toContain('sk-test-not-a-credential-4242');
	expect(body).toContain('…4242');
});

test('a failed store still clears the key', async ({ page }) => {
	// A key left in a field outlives the mistake that put it there — a screenshot, a shared screen, a step away
	// from the desk.
	await connect(page, {
		addReply: { status: 422, json: { message: 'that provider is not one this build speaks' } }
	});
	await page.goto('/settings/ai');
	await page.getByLabel('Name').fill('Wrong');
	await page.getByLabel('Provider key').fill('sk-test-doomed-0001');
	await page.getByRole('button', { name: 'Store key' }).click();

	await expect(page.getByRole('alert')).toContainText('not one this build speaks');
	await expect(page.getByLabel('Provider key')).toHaveValue('');
});

test('an openai-compatible credential asks for its endpoint before submission', async ({
	page
}) => {
	await connect(page);
	await page.goto('/settings/ai');

	const endpoint = page.getByLabel(/^Endpoint/);
	await expect(endpoint).not.toHaveAttribute('required', '');
	await expect(page.getByText('calls go to api.anthropic.com')).toBeVisible();

	await page.getByLabel('Wire format').selectOption('openai_compatible');
	await expect(endpoint).toHaveAttribute('required', '');
	await expect(endpoint).toHaveAttribute('placeholder', 'https://api.moonshot.ai/v1');
	await expect(page.getByText('Include the version segment')).toBeVisible();
});

test('a verification distinguishes a working key, a rejected one and a refusal', async ({
	page
}) => {
	const recorder = await connect(page, { credentials: [credential()] });
	await page.goto('/settings/ai');
	await page.getByRole('button', { name: 'Check it works' }).click();
	await expect(page.getByRole('status')).toContainText('Answered by claude-opus-5');
	expect(recorder.verifies).toEqual(['cred-1']);

	await connect(page, {
		credentials: [credential()],
		verify: {
			ok: false,
			model: null,
			detail: 'the provider rejected the credential',
			worth_retrying: false
		}
	});
	await page.goto('/settings/ai');
	await page.getByRole('button', { name: 'Check it works' }).click();
	await expect(page.getByRole('status')).toContainText('rejected the credential');
	await expect(page.getByRole('status')).not.toContainText('Worth another try');

	// The one that matters: the credential works and the model said no. Somebody told only "failed" would
	// re-issue a key that was never the problem.
	await connect(page, {
		credentials: [credential()],
		verify: {
			ok: false,
			model: null,
			detail: 'the credential works; the model declined this request (policy)',
			worth_retrying: false
		}
	});
	await page.goto('/settings/ai');
	await page.getByRole('button', { name: 'Check it works' }).click();
	await expect(page.getByRole('status')).toContainText('the credential works');
});

test('a rotation replaces one row and says which', async ({ page }) => {
	const recorder = await connect(page, {
		credentials: [credential(), credential({ id: 'cred-2', label: 'Kimi', is_default: false })]
	});
	await page.goto('/settings/ai');

	await page.getByRole('listitem').filter({ hasText: 'Kimi' }).getByText('Replace key').click();
	await page.getByLabel('New key for Kimi').fill('sk-test-rotated-7777');
	await page.getByRole('button', { name: 'Replace', exact: true }).click();

	await expect(page.getByRole('status')).toContainText('Replaced the key for Kimi');
	await expect(page.getByRole('status')).toContainText('…7777');
	expect(recorder.rotated).toEqual([{ id: 'cred-2', body: { api_key: 'sk-test-rotated-7777' } }]);
});

test('withdrawing the credential in use leaves nothing in use', async ({ page }) => {
	await connect(page, { credentials: [credential()] });
	await page.goto('/settings/ai');
	await expect(page.getByText('In use')).toBeVisible();

	await page.getByRole('button', { name: 'Withdraw' }).click();
	await expect(page.getByRole('status')).toContainText('withdrawn');
	await expect(page.getByText('Withdrawn', { exact: true })).toBeVisible();
	await expect(page.getByText('In use')).toBeHidden();
});

test('an unmetered tenant is told so, and a cap is set in dollars', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/settings/ai');
	await expect(page.getByText('no cap set')).toBeVisible();

	await page.getByLabel('Monthly limit (US dollars)').fill('250.50');
	await page.getByRole('button', { name: 'Save cap' }).click();
	await expect(page.getByRole('status')).toContainText('Spend cap saved');
	// Dollars at the edge, cents on the wire.
	expect(recorder.budgets).toEqual([{ limit_cents: 25050, hard: true }]);
	await expect(page.getByText('$0.00 used this month of $250.50.')).toBeVisible();
});

test('a tenant over a hard cap is told enrichment is refused', async ({ page }) => {
	await connect(page, {
		budget: {
			limit_cents: 10000,
			enforcement: 'hard',
			warn_at_fraction: 0.8,
			used_cents: 11000,
			period_start: '2026-08-01',
			state: 'refused'
		}
	});
	await page.goto('/settings/ai');
	await expect(page.getByText('$110.00 used this month of $100.00.')).toBeVisible();
	await expect(page.getByText('Over the cap: new enrichment is refused.')).toBeVisible();
});

test('a credential sealed under a retired key is flagged for re-sealing', async ({ page }) => {
	// The rotation worklist, and it is computed without opening anything — which is why the flag can be shown
	// at all.
	await connect(page, { credentials: [credential({ needs_resealing: true })] });
	await page.goto('/settings/ai');
	await expect(page.getByText('Needs re-sealing')).toBeVisible();
});

for (const theme of ['light', 'dark'] as const) {
	test(`the AI settings screen has no axe violations in ${theme}`, async ({ page }) => {
		await connect(page, {
			credentials: [credential(), credential({ id: 'cred-2', label: 'Kimi', is_default: false })],
			budget: {
				limit_cents: 10000,
				enforcement: 'hard',
				warn_at_fraction: 0.8,
				used_cents: 9000,
				period_start: '2026-08-01',
				state: 'warned'
			}
		});
		await page.emulateMedia({ colorScheme: theme });
		await page.goto('/settings/ai');
		await page.getByRole('button', { name: 'Check it works' }).first().click();
		await expect(page.getByRole('status').first()).toBeVisible();

		const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
		expect(results.violations).toEqual([]);
	});
}
