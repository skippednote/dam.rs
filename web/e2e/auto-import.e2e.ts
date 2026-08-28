/**
 * Auto-import mappings through a real browser, against a mocked API.
 *
 * The Rust gate proves the mapping rules, the coercion and the HTTP contract. Three properties exist only here:
 *
 * - **The source is a picker built from the server's list.** A name typed from memory saves happily and then
 *   never fires, so a text box would be the bug. The options have to come from `/sources`.
 * - **A failing mapping list must not empty the picker.** The two reads are independent — one is this tenant's
 *   table, the other is a property of the extractor — and failing them together left the form open with an empty
 *   `required` select that could never be satisfied. This is the regression that check exists for.
 * - **The rows are grouped by target field in the server's order,** because "first match wins" is only legible
 *   if the screen shows which rule is tried first.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Mapping = {
	id: string;
	source: string;
	field_key: string;
	priority: number;
	overwrite: boolean;
	enabled: boolean;
};

function mapping(source: string, field_key: string, overrides: Partial<Mapping> = {}): Mapping {
	return {
		id: `m-${source}-${field_key}`,
		source,
		field_key,
		priority: 0,
		overwrite: false,
		enabled: true,
		...overrides
	};
}

/** What the real extractor advertises, trimmed to what these cases need. */
const SOURCES = ['exif.artist', 'exif.taken_at', 'xmp.creator', 'xmp.headline'];

const FIELDS = [
	{
		key: 'photographer',
		label: 'Photographer',
		kind: 'text',
		multivalued: false,
		required: false,
		read_only: false,
		ai_writable: false,
		facetable: false,
		searchable: true,
		search_alias: null,
		taxonomy_id: null,
		assets_with_values: 0
	},
	{
		key: 'ingested_by',
		label: 'Ingested by',
		kind: 'text',
		multivalued: false,
		required: false,
		// Read-only, so the picker must not offer it: the server would refuse the mapping, and a form that
		// offers a choice the server rejects is a form that teaches people to distrust it.
		read_only: true,
		ai_writable: false,
		facetable: false,
		searchable: false,
		search_alias: null,
		taxonomy_id: null,
		assets_with_values: 0
	}
];

type Recorder = {
	created: Record<string, unknown>[];
	amended: { id: string; body: Record<string, unknown> }[];
	removed: string[];
};

async function connect(
	page: Page,
	options: {
		mappings?: Mapping[];
		/** Lets a case make one of the two reads fail, which is the whole point of one of them. */
		listStatus?: number;
		sourcesStatus?: number;
		create?: (body: Record<string, unknown>) => { status: number; json: unknown };
	} = {}
): Promise<Recorder> {
	const recorder: Recorder = { created: [], amended: [], removed: [] };
	let mappings = options.mappings ?? [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		// Before the bare collection, since it is a prefix of this one.
		if (url.pathname === '/auto-import-mappings/sources') {
			if (options.sourcesStatus) {
				return route.fulfill({ status: options.sourcesStatus, json: { reason: 'no' } });
			}
			return route.fulfill({ json: SOURCES });
		}
		if (url.pathname === '/auto-import-mappings' && method === 'GET') {
			if (options.listStatus) {
				return route.fulfill({
					status: options.listStatus,
					json: { reason: 'the mapping table is not there' }
				});
			}
			return route.fulfill({ json: mappings });
		}
		if (url.pathname === '/auto-import-mappings' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.created.push(body);
			if (options.create) {
				const outcome = options.create(body);
				return route.fulfill({ status: outcome.status, json: outcome.json });
			}
			const created = mapping(String(body.source), String(body.field_key), {
				priority: Number(body.priority ?? 0),
				overwrite: Boolean(body.overwrite)
			});
			mappings = [...mappings, created];
			return route.fulfill({ status: 201, json: created });
		}
		if (url.pathname.startsWith('/auto-import-mappings/') && method === 'PATCH') {
			const id = url.pathname.split('/').pop() ?? '';
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.amended.push({ id, body });
			mappings = mappings.map((row) => (row.id === id ? { ...row, ...body } : row));
			return route.fulfill({ json: mappings.find((row) => row.id === id) });
		}
		if (url.pathname.startsWith('/auto-import-mappings/') && method === 'DELETE') {
			const id = url.pathname.split('/').pop() ?? '';
			recorder.removed.push(id);
			mappings = mappings.filter((row) => row.id !== id);
			return route.fulfill({ status: 204, body: '' });
		}

		if (url.pathname === '/schema/fields') {
			return route.fulfill({ json: FIELDS });
		}
		if (url.pathname === '/schema/types' || url.pathname === '/upload-profiles') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/categories' || url.pathname === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/assets' || url.pathname === '/search') {
			return route.fulfill({ json: { items: [], total: 0, offset: 0, ranked: false } });
		}
		if (url.pathname === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

function panel(page: Page) {
	return page.getByRole('region', { name: 'Auto-import from embedded metadata' });
}

test('the source picker offers what the server produces, and nothing else', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');

	const section = panel(page);
	await section.getByRole('button', { name: 'Add a mapping' }).click();

	// From `/sources`: a name this app invented would be a rule that saves and never fires.
	const offered = await section.getByLabel('In the file').locator('option').allTextContents();
	expect(offered).toEqual(['Choose a source', ...SOURCES]);

	// And the target list excludes the read-only field, which the server would refuse.
	const targets = await section.getByLabel('Fills').locator('option').allTextContents();
	expect(targets).toEqual(['Choose a field', 'Photographer']);
});

test('a mapping is created with the two defaults stated on screen', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	const section = panel(page);
	await section.getByRole('button', { name: 'Add a mapping' }).click();
	await section.getByLabel('In the file').selectOption('exif.artist');
	await section.getByLabel('Fills').selectOption('photographer');
	await section.getByRole('button', { name: 'Add', exact: true }).click();

	// The consequence, in the field's label rather than its key — the person reading it chose the label.
	await expect(section.getByRole('status')).toContainText('exif.artist now fills Photographer');
	expect(recorder.created).toEqual([
		{ source: 'exif.artist', field_key: 'photographer', priority: 0, overwrite: false }
	]);

	// `overwrite` off is the rule that protects a curated library, so its absence has to be visible.
	const row = section.locator('li li').first();
	await expect(row).toContainText('exif.artist');
	await expect(row.getByText('replaces')).toHaveCount(0);
});

test('two sources for one field are grouped, in the order the server tries them', async ({
	page
}) => {
	await connect(page, {
		mappings: [
			// The order the server returns: `(field_key, priority)`. The screen must not re-sort it.
			mapping('xmp.creator', 'photographer', { priority: 0 }),
			mapping('exif.artist', 'photographer', { priority: 10, enabled: false })
		]
	});
	await page.goto('/schema');

	const section = panel(page);
	const group = section.locator('li').filter({ has: page.getByRole('heading', { level: 3 }) });
	await expect(group.getByRole('heading', { level: 3 })).toContainText('Photographer');
	await expect(group.getByRole('heading', { level: 3 })).toContainText('1 of 2 in use');

	const rows = group.locator('li');
	await expect(rows.nth(0)).toContainText('xmp.creator');
	await expect(rows.nth(1)).toContainText('exif.artist');
	// The disabled one says so, rather than looking identical to a rule that is doing something.
	await expect(rows.nth(1)).toContainText('off');
});

test('a failing mapping list does not empty the source picker', async ({ page }) => {
	// The regression: `Promise.all` made one bad read blank both, so the form opened with a `required` select
	// that had no options and no explanation.
	await connect(page, { listStatus: 500 });
	await page.goto('/schema');

	const section = panel(page);
	await expect(section.getByRole('alert')).toContainText('mapping table is not there');

	await section.getByRole('button', { name: 'Add a mapping' }).click();
	const offered = await section.getByLabel('In the file').locator('option').allTextContents();
	expect(offered).toEqual(['Choose a source', ...SOURCES]);
});

test('with no source list there is no form to fill in', async ({ page }) => {
	// The other direction: nothing to choose from means the button is unavailable and the page says why, rather
	// than offering a picker that cannot produce a valid rule.
	await connect(page, { sourcesStatus: 500 });
	await page.goto('/schema');

	const section = panel(page);
	await expect(section.getByRole('button', { name: 'Add a mapping' })).toBeDisabled();
	await expect(section).toContainText('could not be read');
});

test("a refusal is shown in the server's own words", async ({ page }) => {
	await connect(page, {
		create: () => ({
			status: 409,
			json: { reason: '`exif.artist` already maps to `photographer`' }
		})
	});
	await page.goto('/schema');

	const section = panel(page);
	await section.getByRole('button', { name: 'Add a mapping' }).click();
	await section.getByLabel('In the file').selectOption('exif.artist');
	await section.getByLabel('Fills').selectOption('photographer');
	await section.getByRole('button', { name: 'Add', exact: true }).click();

	await expect(section.getByRole('alert')).toContainText('already maps to');
});

test('removal says what it does and does not undo', async ({ page }) => {
	const recorder = await connect(page, {
		mappings: [mapping('exif.artist', 'photographer')]
	});
	await page.goto('/schema');

	const section = panel(page);
	await section.locator('li li').first().getByRole('button', { name: 'Remove' }).click();
	// The distinction that matters: this changes what happens next, it does not retract what already happened.
	await expect(section).toContainText('Values already imported stay on their assets');

	await section.getByRole('button', { name: 'Remove exif.artist' }).click();
	await expect(section.getByRole('status')).toContainText('no longer fills Photographer');
	expect(recorder.removed).toEqual(['m-exif.artist-photographer']);
});

test('switching overwrite on sends only that change', async ({ page }) => {
	const recorder = await connect(page, {
		mappings: [mapping('exif.artist', 'photographer')]
	});
	await page.goto('/schema');

	const section = panel(page);
	await section.locator('li li').first().getByLabel('Replace').check();
	await expect(section.getByRole('status')).toContainText('will now replace');
	// Only the switch that moved: sending `enabled` too would let a stale read turn a mapping off by accident.
	expect(recorder.amended).toEqual([
		{ id: 'm-exif.artist-photographer', body: { overwrite: true } }
	]);
	await expect(section.locator('li li').first().getByText('replaces')).toBeVisible();
});

test('the panel has no accessibility violations, populated or empty', async ({ page }) => {
	await connect(page, {
		mappings: [
			mapping('xmp.creator', 'photographer'),
			mapping('exif.artist', 'photographer', { priority: 10, enabled: false, overwrite: true })
		]
	});
	await page.goto('/schema');

	const section = panel(page);
	await expect(section.getByRole('heading', { level: 2 })).toBeVisible();
	const populated = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Auto-import from embedded metadata"]')
		.analyze();
	expect(populated.violations).toEqual([]);

	// The disabled row is dimmed by a ground tint rather than by opacity, which is what keeps its muted text
	// above AA — the mistake this repo has already made once.
	await section.getByRole('button', { name: 'Add a mapping' }).click();
	const withForm = await new AxeBuilder({ page })
		.withTags(WCAG_21_AA)
		.include('[aria-label="Auto-import from embedded metadata"]')
		.analyze();
	expect(withForm.violations).toEqual([]);
});
