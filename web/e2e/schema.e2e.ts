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

type MetadataType = {
	id: string;
	key: string;
	label: string;
	applies_to: string[];
	is_default: boolean;
	field_keys: string[];
	assets: number;
};

type Recorder = {
	defined: Record<string, unknown>[];
	amended: { key: string; body: Record<string, unknown> }[];
	removed: string[];
	orders: string[][];
	/** Rail configurations, so a test can assert the whole ordered list travelled (Q.19). */
	rails: string[][];
	/** Type edits, so a test can assert the whole field list travelled rather than a delta. */
	typeEdits: { id: string; body: Record<string, unknown> }[];
	typesDefined: Record<string, unknown>[];
	typesRemoved: string[];
};

/**
 * Mounts the mocks. `behaviour` lets a case decide what the server says back, since half the point of this
 * page is how it renders a refusal.
 */
async function connect(
	page: Page,
	options: {
		fields?: Field[];
		types?: MetadataType[];
		define?: (body: Record<string, unknown>) => { status: number; json: unknown };
		amend?: (key: string, body: Record<string, unknown>) => { status: number; json: unknown };
		remove?: (key: string) => { status: number; json: unknown };
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = {
		defined: [],
		amended: [],
		removed: [],
		orders: [],
		rails: [],
		typeEdits: [],
		typesDefined: [],
		typesRemoved: []
	};
	let types: MetadataType[] = options.types ?? [];
	// The rail's candidates. Mutated by a PUT, so the screen's re-read sees what a server would have stored.
	let rail = [
		{ entry: 'field:brand', label: 'brand', kind: 'field', is_enabled: true },
		{ entry: 'field:campaign', label: 'campaign', kind: 'field', is_enabled: true },
		{
			entry: 'taxonomy:11111111-1111-4111-8111-111111111111',
			label: 'Materials',
			kind: 'taxonomy',
			is_enabled: true
		},
		{ entry: 'builtin:status', label: 'status', kind: 'builtin', is_enabled: true },
		{ entry: 'builtin:orientation', label: 'orientation', kind: 'builtin', is_enabled: true },
		{ entry: 'builtin:stars', label: 'stars', kind: 'builtin', is_enabled: true },
		{ entry: 'builtin:has', label: 'has', kind: 'builtin', is_enabled: true }
	];
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
		if (url.pathname === '/schema/facets' && method === 'GET') {
			// Q.19. Two fields, one vocabulary and the four built-ins — enough that ordering and switching off
			// are both visible.
			return route.fulfill({ json: rail });
		}
		if (url.pathname === '/schema/facets' && method === 'PUT') {
			const body = route.request().postDataJSON() as { enabled: string[] };
			recorder.rails.push(body.enabled);
			// The server's answer, not the client's guess: enabled entries in the order given, then everything
			// else marked off — which is what the screen re-reads after saving.
			const named = body.enabled
				.map((entry) => rail.find((one) => one.entry === entry))
				.filter((one): one is (typeof rail)[number] => one !== undefined)
				.map((one) => ({ ...one, is_enabled: true }));
			const rest = rail
				.filter((one) => !body.enabled.includes(one.entry))
				.map((one) => ({ ...one, is_enabled: false }));
			rail = [...named, ...rest];
			return route.fulfill({ status: 204, body: '' });
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
		if (url.pathname === '/schema/types' && method === 'GET') {
			return route.fulfill({ json: types });
		}
		if (url.pathname === '/schema/types' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.typesDefined.push(body);
			const created: MetadataType = {
				id: `type-${types.length + 1}`,
				key: String(body.key),
				label: String(body.label),
				applies_to: (body.applies_to as string[]) ?? [],
				is_default: Boolean(body.is_default),
				field_keys: (body.field_keys as string[]) ?? [],
				assets: 0
			};
			types = [...types, created];
			return route.fulfill({ status: 201, json: created });
		}
		if (url.pathname.startsWith('/schema/types/') && method === 'PATCH') {
			const id = url.pathname.split('/').pop() ?? '';
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.typeEdits.push({ id, body });
			types = types.map((type) => {
				if (type.id !== id) {
					// The default is exclusive, so claiming it here clears it everywhere else — the server does
					// the same, and a mock that did not would hide a UI showing two fallbacks.
					return body.is_default === true ? { ...type, is_default: false } : type;
				}
				return { ...type, ...body } as MetadataType;
			});
			return route.fulfill({ json: types.find((type) => type.id === id) });
		}
		if (url.pathname.startsWith('/schema/types/') && method === 'DELETE') {
			const id = url.pathname.split('/').pop() ?? '';
			recorder.typesRemoved.push(id);
			types = types.filter((type) => type.id !== id);
			return route.fulfill({ status: 204, body: '' });
		}
		// The page grew an upload-profiles section, and an unstubbed endpoint would surface as a second alert
		// on a page whose cases assert about the first one.
		if (url.pathname === '/upload-profiles') {
			return route.fulfill({ json: [] });
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
	await expect(
		page.getByRole('region', { name: 'Metadata fields' }).getByRole('alert')
	).toContainText('12 asset(s) already carry a value');
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
	//
	// Scoped to the fields region: the refine-search rail below it has move buttons too (Q.19), and an
	// accessible name matches case-insensitively — "Move brand down" there is the same name as "Move Brand
	// down" here.
	const fields = page.getByRole('region', { name: 'Metadata fields' });
	await fields.getByRole('button', { name: 'Move Brand down' }).click();
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

test('with no types defined, the page says every field applies', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');

	// The migration state, and the one an administrator most needs explaining: nothing is narrowed yet, and
	// the copy has to say what adding a type would do rather than showing an empty list.
	await expect(page.getByText(/No types yet/)).toBeVisible();
	await expect(page.getByText(/every asset shows every field/)).toBeVisible();
});

test('a type is created and its fields chosen from the vocabulary above', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	await page.getByRole('button', { name: 'Add a type' }).click();
	await page.getByLabel('Key').last().fill('video');
	await page.getByLabel('Label').last().fill('Video');
	await page.getByRole('checkbox', { name: 'video' }).check();
	await page.getByRole('button', { name: 'Add', exact: true }).click();

	expect(recorder.typesDefined).toEqual([
		{ key: 'video', label: 'Video', applies_to: ['video'], field_keys: [] }
	]);
	// A new type has no fields, so it opens straight into choosing them — and says why that matters.
	await expect(page.getByText(/Choose the fields it should show/)).toBeVisible();
	await expect(page.getByText(/shows no editable metadata at all/)).toBeVisible();

	// Adding a field sends the *whole* list, which is the endpoint's contract: a delta computed against a
	// stale copy would drop whatever the client had not seen.
	await page.getByRole('button', { name: '+ Brand' }).click();
	await expect.poll(() => recorder.typeEdits.at(-1)?.body).toEqual({ field_keys: ['brand'] });
	await page.getByRole('button', { name: '+ Campaign' }).click();
	await expect
		.poll(() => recorder.typeEdits.at(-1)?.body)
		.toEqual({ field_keys: ['brand', 'campaign'] });

	// And reordering sends the whole list too, reordered.
	await page.getByRole('button', { name: 'Move campaign up in Video' }).click();
	await expect
		.poll(() => recorder.typeEdits.at(-1)?.body)
		.toEqual({ field_keys: ['campaign', 'brand'] });
});

test('the fallback moves rather than being held by two types at once', async ({ page }) => {
	const recorder = await connect(page, {
		types: [
			{
				id: 'type-image',
				key: 'image',
				label: 'Image',
				applies_to: ['image'],
				is_default: true,
				field_keys: ['brand'],
				assets: 12
			},
			{
				id: 'type-video',
				key: 'video',
				label: 'Video',
				applies_to: ['video'],
				is_default: false,
				field_keys: [],
				assets: 0
			}
		]
	});
	await page.goto('/schema');

	// Only one row is labelled, and the labelled one has no "make fallback" button — offering it would invite
	// a click that does nothing.
	await expect(page.getByText('fallback', { exact: true })).toHaveCount(1);

	await page
		.getByRole('region', { name: 'Asset types' })
		.getByRole('button', { name: 'Make fallback' })
		.click();
	expect(recorder.typeEdits.at(-1)).toEqual({ id: 'type-video', body: { is_default: true } });
	await expect(page.getByText('fallback', { exact: true })).toHaveCount(1);
});

test('removing a type says how many assets re-form and that nothing is deleted', async ({
	page
}) => {
	const recorder = await connect(page, {
		types: [
			{
				id: 'type-image',
				key: 'image',
				label: 'Image',
				applies_to: ['image'],
				is_default: true,
				field_keys: ['brand'],
				assets: 12
			}
		]
	});
	await page.goto('/schema');

	const section = page.getByRole('region', { name: 'Asset types' });
	await expect(section.getByText('1 field · 12 assets')).toBeVisible();
	// Scoped to the section: "Remove" appears on every field row above as well, and an ambiguous locator
	// would be a test that passes or fails on DOM order.
	await section.getByRole('button', { name: 'Remove', exact: true }).click();

	// Both halves: how many are affected, and that being affected is not being damaged. A removal that only
	// said "12 assets use it" reads as a warning about data loss, which this is not.
	const confirmation = page.getByText(/12 asset\(s\) use it/);
	await expect(confirmation).toBeVisible();
	await expect(page.getByText(/fall back to the default form — nothing is deleted/)).toBeVisible();
	expect(recorder.typesRemoved, 'the first click only asks').toEqual([]);

	await section.getByRole('button', { name: 'Remove image' }).click();
	expect(recorder.typesRemoved).toEqual(['type-image']);
	await expect(page.getByRole('status')).toContainText('fall back to the default form');
});

test('the types section has no axe violations, including while editing', async ({ page }) => {
	await connect(page, {
		types: [
			{
				id: 'type-image',
				key: 'image',
				label: 'Image',
				applies_to: ['image'],
				is_default: true,
				field_keys: ['brand'],
				assets: 12
			}
		]
	});
	await page.goto('/schema');
	await expect(page.getByRole('heading', { name: 'Asset types' })).toBeVisible();

	let results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	let detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the types list:\n${detail}`).toEqual([]);

	await page
		.getByRole('region', { name: 'Asset types' })
		.getByRole('button', { name: 'Edit fields' })
		.click();
	await expect(page.getByText('Fields on this form, in the order they appear')).toBeVisible();
	results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations while editing a type:\n${detail}`).toEqual([]);
});

test('the refine-search rail is ordered and switched off by the tenant', async ({ page }) => {
	// Q.19. The rail *is* an order: a screen offering only on/off would leave the arrangement to whatever the
	// schema implied, which is the state this exists to fix.
	const recorder = await connect(page);
	await page.goto('/schema');

	const rail = page.getByRole('region', { name: 'Refine search' });
	await expect(rail.getByRole('heading', { name: 'Refine search' })).toBeVisible();
	// The built-ins are entries like any other, under their own words rather than their query selectors.
	await expect(rail.getByText('Rating')).toBeVisible();
	await expect(rail.getByText('Attachments')).toBeVisible();

	// Move the second field above the first, switch ratings off, and save.
	await rail.getByRole('button', { name: 'Move campaign up' }).click();
	// `click`, not `uncheck`: switching an entry off moves its row to the "Not shown" list, so the element
	// Playwright would re-assert on is a different one by then.
	await rail.getByRole('checkbox', { name: 'Show Rating' }).click();
	await rail.getByRole('button', { name: 'Save order' }).click();

	await expect(rail.getByRole('status')).toContainText('The rail follows this order');
	// The whole ordered list travelled, with the disabled entry absent from it rather than flagged.
	const sent = recorder.rails.at(-1) ?? [];
	expect(sent[0]).toBe('field:campaign');
	expect(sent[1]).toBe('field:brand');
	expect(sent).not.toContain('builtin:stars');

	// And what was switched off is still on screen, so it can be switched back on.
	await expect(rail.getByText('Not shown')).toBeVisible();
	await expect(rail.getByRole('checkbox', { name: 'Show Rating' })).toHaveCount(0);
});

test('the refine-search rail has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');
	await expect(page.getByRole('heading', { name: 'Refine search' })).toBeVisible();
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the rail:\n${detail}`).toEqual([]);
});
