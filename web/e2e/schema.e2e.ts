/**
 * Schema administration, through a real browser, against a mocked API.
 *
 * Same posture as the other web specs: the Rust gate proves the refusals and the HTTP contract, so what
 * belongs here is the browser half — that the consequences the server computes actually reach the screen
 * (the newly-incomplete count, the stale-index warning, the value count in a removal confirmation), that a
 * refusal is shown in the server's own words rather than reduced to "something went wrong", that reordering
 * sends the *whole* list, and that none of it has an axe violation.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Field = {
	key: string;
	label: string;
	kind: string;
	multivalued: boolean;
	required: boolean;
	read_only: boolean;
	ai_writable: boolean;
	facetable: boolean;
	searchable: boolean;
	search_alias: string | null;
	taxonomy_id: string | null;
	assets_with_values: number;
};

function field(key: string, overrides: Partial<Field> = {}): Field {
	return {
		key,
		label: key[0].toUpperCase() + key.slice(1),
		kind: 'text',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: false,
		searchable: true,
		search_alias: null,
		taxonomy_id: null,
		assets_with_values: 0,
		...overrides
	};
}

type Recorder = {
	defined: Record<string, unknown>[];
	amended: { key: string; body: Record<string, unknown> }[];
	removed: string[];
	orders: string[][];
};

/**
 * Mounts the mocks. `behaviour` lets a case decide what the server says back, since half the point of this
 * page is how it renders a refusal.
 */
async function connect(
	page: Page,
	options: {
		fields?: Field[];
		define?: (body: Record<string, unknown>) => { status: number; json: unknown };
		amend?: (key: string, body: Record<string, unknown>) => { status: number; json: unknown };
		remove?: (key: string) => { status: number; json: unknown };
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = { defined: [], amended: [], removed: [], orders: [] };
	let fields = options.fields ?? [
		field('brand', { facetable: true, search_alias: 'bra', assets_with_values: 12 }),
		field('campaign', { assets_with_values: 0 })
	];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/schema/fields' && method === 'GET') {
			return route.fulfill({ json: fields });
		}
		if (url.pathname === '/schema/fields' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.defined.push(body);
			const reply = options.define?.(body);
			if (reply) return route.fulfill(reply);
			const created = field(String(body.key), { label: String(body.label) });
			fields = [...fields, created];
			return route.fulfill({ status: 201, json: created });
		}
		if (url.pathname === '/schema/fields/order' && method === 'PUT') {
			const body = route.request().postDataJSON() as { keys: string[] };
			recorder.orders.push(body.keys);
			fields = body.keys.map((key) => fields.find((f) => f.key === key) ?? field(key));
			return route.fulfill({ status: 204, body: '' });
		}
		if (url.pathname.startsWith('/schema/fields/') && method === 'PATCH') {
			const key = decodeURIComponent(url.pathname.split('/').pop() ?? '');
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.amended.push({ key, body });
			const reply = options.amend?.(key, body);
			if (reply) return route.fulfill(reply);
			fields = fields.map((f) => (f.key === key ? { ...f, ...body } : f));
			const updated = fields.find((f) => f.key === key) ?? field(key);
			return route.fulfill({
				json: { ...updated, reindex_required: false, assets_now_incomplete: 0 }
			});
		}
		if (url.pathname.startsWith('/schema/fields/') && method === 'DELETE') {
			const key = decodeURIComponent(url.pathname.split('/').pop() ?? '');
			recorder.removed.push(key);
			const reply = options.remove?.(key);
			const existing = fields.find((f) => f.key === key);
			fields = fields.filter((f) => f.key !== key);
			return route.fulfill(
				reply ?? {
					status: 200,
					json: {
						key,
						assets_with_values: existing?.assets_with_values ?? 0,
						reindex_required: true
					}
				}
			);
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the schema lists every field with how much data depends on it', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');

	const table = page.getByRole('table');
	// The label is what a person reads; the key is what they type into a search box. Both are on the row,
	// because a schema page whose rows only carry labels cannot be used to write a query.
	await expect(table.getByText('Brand', { exact: true })).toBeVisible();
	// The number that decides whether an edit is safe, on the row rather than behind another click.
	await expect(table.getByText('12')).toBeVisible();
	// The alias is shown as it is typed in a search box, colon included, because that is the only form of
	// it anybody uses.
	await expect(table.getByText('brand · bra:')).toBeVisible();
});

test('adding a field sends the key and kind, and says a key is permanent', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	await page.getByRole('button', { name: 'Add a field' }).click();
	// Said at the moment of choosing, not in documentation: the key cannot be changed afterwards, and this
	// is the only point where that is cheap to act on.
	await expect(page.getByText(/A key is permanent/)).toBeVisible();

	await page.getByLabel('Key').fill('stylist');
	await page.getByLabel('Label').fill('Stylist');
	await page.getByLabel('Kind').selectOption('text');
	await page.getByRole('button', { name: 'Add', exact: true }).click();

	await expect(page.getByRole('status')).toContainText('Added stylist');
	expect(recorder.defined).toEqual([
		{
			key: 'stylist',
			label: 'Stylist',
			kind: 'text',
			facetable: false,
			multivalued: false
		}
	]);
});

test('making a field required reports what it breaks and that search is stale', async ({
	page
}) => {
	await connect(page, {
		amend: (key, body) => ({
			status: 200,
			json: {
				...field(key, { required: Boolean(body.required) }),
				reindex_required: true,
				assets_now_incomplete: 412
			}
		})
	});
	await page.goto('/schema');

	await page
		.getByRole('row', { name: /brand/ })
		.getByRole('checkbox', { name: 'Required' })
		.check();

	// Both consequences, in the words that say what to do about them. A number alone ("412") would leave an
	// administrator to work out what it counted.
	const status = page.getByRole('status');
	await expect(status).toContainText('412 asset(s) have no value for it');
	await expect(status).toContainText('next metadata save will be refused');
	await expect(status).toContainText('stale until the index is rebuilt');
});

test('a kind locked by stored values is shown in the server’s own words', async ({ page }) => {
	await connect(page, {
		amend: () => ({
			status: 409,
			json: {
				reason:
					'`brand` cannot change kind: 12 asset(s) already carry a value stored under the current kind'
			}
		})
	});
	await page.goto('/schema');

	await page
		.getByRole('row', { name: /brand/ })
		.getByRole('checkbox', { name: 'Searchable' })
		.uncheck();

	// Verbatim, count included: this sentence is the whole reason the refusal is actionable, and no status
	// code the page could paraphrase carries the number.
	await expect(page.getByRole('alert')).toContainText('12 asset(s) already carry a value');
});

test('removing a field confirms with the count and promises the values survive', async ({
	page
}) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	const brand = page.getByRole('row', { name: /Brand/ });
	await brand.getByRole('button', { name: 'Remove' }).click();

	// The confirmation carries the count *and* the recoverability, because "remove brand" and "remove
	// brand, which 12 assets use, reversibly" are different decisions. It lives in its own full-width row:
	// in the actions cell, a sentence with a real count reflowed every column and pushed these two buttons
	// off the edge of the table — found by driving the real tenant, where the counts are not "12".
	const confirmation = page.getByRole('row', { name: /Remove brand\?/ });
	await expect(confirmation).toContainText('12 asset(s) use it');
	await expect(confirmation).toContainText('come back if you re-add it');
	expect(recorder.removed, 'clicking Remove must not remove anything yet').toEqual([]);

	// Both controls are reachable, which is the property the layout bug broke.
	await expect(confirmation.getByRole('button', { name: 'Cancel' })).toBeVisible();
	const confirm = confirmation.getByRole('button', { name: 'Remove brand' });
	await expect(confirm).toBeInViewport();
	await confirm.click();
	await expect(page.getByRole('status')).toContainText('The values on 12 asset(s) are kept');
	expect(recorder.removed).toEqual(['brand']);
});

test('reordering sends the whole list, not the pair that moved', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	// The server refuses a partial list — display order is a total order — so the client has to send all of
	// it. Sending only the moved keys is the mistake this asserts against.
	await page.getByRole('button', { name: 'Move Brand down' }).click();
	await expect(page.getByRole('status')).toContainText('Field order saved');
	expect(recorder.orders).toEqual([['campaign', 'brand']]);

	// And the list on screen followed the server's new order.
	const firstRowKey = page.getByRole('row').nth(1);
	await expect(firstRowKey).toContainText('campaign');
});

test('the schema page has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');
	await expect(page.getByRole('table')).toBeVisible();

	let results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	let detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on /schema:\n${detail}`).toEqual([]);

	// The add form is a separate state with its own labels and its own contrast.
	await page.getByRole('button', { name: 'Add a field' }).click();
	await expect(page.getByLabel('Key')).toBeVisible();
	results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations with the add form open:\n${detail}`).toEqual([]);
});
