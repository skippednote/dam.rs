/**
 * Upload profiles through a real browser, against a mocked API.
 *
 * The Rust gate proves the validation, the merge and the HTTP contract. What only exists here is the browser
 * half, and one property that exists *only* in the browser: `require_complete` is a client-side rule by design —
 * the server deliberately will not refuse a finished upload over it, because by then the bytes are staged and
 * refusing would strand them. So if the uploader does not apply it, nothing does.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Profile = {
	id: string;
	key: string;
	label: string;
	metadata_type_id: string | null;
	defaults: Record<string, unknown>;
	require_complete: boolean;
	ai_tags_enabled: boolean;
	is_default: boolean;
};

function profile(key: string, overrides: Partial<Profile> = {}): Profile {
	return {
		id: `p-${key}`,
		key,
		label: key[0].toUpperCase() + key.slice(1),
		metadata_type_id: null,
		defaults: {},
		require_complete: false,
		ai_tags_enabled: true,
		is_default: false,
		...overrides
	};
}

const FIELDS = [
	{
		key: 'credit',
		label: 'Credit',
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
		key: 'ingested_at',
		label: 'Ingested at',
		kind: 'datetime',
		multivalued: false,
		required: false,
		// Read-only: a profile must not be offered it, because the validator would refuse the write.
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
	/** `Upload-Metadata` headers the uploader sent, so a test can read what profile it named. */
	uploadMetadata: string[];
};

async function connect(page: Page, options: { profiles?: Profile[] } = {}): Promise<Recorder> {
	const recorder: Recorder = { created: [], amended: [], removed: [], uploadMetadata: [] };
	let profiles = options.profiles ?? [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();

		if (url.pathname === '/upload-profiles' && method === 'GET') {
			return route.fulfill({ json: profiles });
		}
		if (url.pathname === '/upload-profiles' && method === 'POST') {
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.created.push(body);
			const created = profile(String(body.key), { label: String(body.label) });
			profiles = [...profiles, created];
			return route.fulfill({ status: 201, json: created });
		}
		if (url.pathname.startsWith('/upload-profiles/') && method === 'PATCH') {
			const id = url.pathname.split('/').pop() ?? '';
			const body = route.request().postDataJSON() as Record<string, unknown>;
			recorder.amended.push({ id, body });
			profiles = profiles.map((row) => {
				if (row.id !== id) {
					// The default is exclusive, and the server moves it — a mock that did not would let the UI
					// show two defaults and call it correct.
					return body.is_default === true ? { ...row, is_default: false } : row;
				}
				return { ...row, ...body } as Profile;
			});
			return route.fulfill({ json: profiles.find((row) => row.id === id) });
		}
		if (url.pathname.startsWith('/upload-profiles/') && method === 'DELETE') {
			const id = url.pathname.split('/').pop() ?? '';
			recorder.removed.push(id);
			profiles = profiles.filter((row) => row.id !== id);
			return route.fulfill({ status: 204, body: '' });
		}
		if (url.pathname === '/schema/fields') {
			return route.fulfill({ json: FIELDS });
		}
		if (url.pathname === '/schema/types') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/categories') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/uploads' && method === 'POST') {
			recorder.uploadMetadata.push(route.request().headers()['upload-metadata'] ?? '(none)');
			return route.fulfill({
				status: 201,
				headers: { location: '/uploads/abc123', 'tus-resumable': '1.0.0' },
				body: ''
			});
		}
		if (url.pathname === '/assets' || url.pathname === '/search') {
			return route.fulfill({ json: { items: [], total: 0, offset: 0, ranked: false } });
		}
		if (url.pathname === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('with no profiles, the page says what an upload gets anyway', async ({ page }) => {
	await connect(page);
	await page.goto('/schema');

	// The migration state, and the one worth explaining: nothing is configured, and uploads still behave a
	// particular way. A bare "no profiles" would leave an administrator guessing what that way is.
	const section = page.getByRole('region', { name: 'Upload profiles' });
	await expect(section.getByText(/No profiles yet/)).toBeVisible();
	await expect(section.getByText(/automatic tagging on/)).toBeVisible();
});

test('a profile is created and opens straight into saying what it means', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/schema');

	const section = page.getByRole('region', { name: 'Upload profiles' });
	await section.getByRole('button', { name: 'Add a profile' }).click();
	await section.getByLabel('Key').fill('press');
	await section.getByLabel('Label').fill('Press delivery');
	await section.getByRole('button', { name: 'Add', exact: true }).click();

	expect(recorder.created).toEqual([{ key: 'press', label: 'Press delivery' }]);
	// A profile with no defaults and no form is just a name, so the editor opens: the next thing to do is
	// always to say what it means.
	await expect(section.getByText('Insist on required metadata')).toBeVisible();
});

test('only fields a profile may write are offered as defaults', async ({ page }) => {
	await connect(page, { profiles: [profile('press')] });
	await page.goto('/schema');

	const section = page.getByRole('region', { name: 'Upload profiles' });
	await section.getByRole('button', { name: 'Edit' }).click();

	// `credit` is writable, `ingested_at` is read-only — maintained by the system, and the validator would
	// refuse the write. Offering it would invite a refusal the person could not have predicted.
	await expect(section.getByLabel('Credit')).toBeVisible();
	await expect(section.getByLabel('Ingested at')).toHaveCount(0);
});

test('a default is saved, and an empty one is dropped rather than sent as blank', async ({
	page
}) => {
	const recorder = await connect(page, { profiles: [profile('press')] });
	await page.goto('/schema');

	const section = page.getByRole('region', { name: 'Upload profiles' });
	await section.getByRole('button', { name: 'Edit' }).click();
	await section.getByLabel('Credit').fill('Acme Press Office');
	await section.getByRole('button', { name: 'Save defaults' }).click();

	expect(recorder.amended.at(-1)?.body).toEqual({
		defaults: { credit: 'Acme Press Office' }
	});

	// Cleared, not blanked: "" is a value the validator accepts for a text field, so sending it would silently
	// default every asset from this intake to empty.
	await section.getByLabel('Credit').fill('   ');
	await section.getByRole('button', { name: 'Save defaults' }).click();
	expect(recorder.amended.at(-1)?.body).toEqual({ defaults: {} });
});

test('the default profile is exclusive', async ({ page }) => {
	const recorder = await connect(page, {
		profiles: [profile('press', { is_default: true }), profile('studio')]
	});
	await page.goto('/schema');

	const section = page.getByRole('region', { name: 'Upload profiles' });
	await expect(section.getByText('default', { exact: true })).toHaveCount(1);
	// The one holding it has no "make default" button: offering it would invite a click that does nothing.
	await section.getByRole('button', { name: 'Make default' }).click();
	expect(recorder.amended.at(-1)).toEqual({ id: 'p-studio', body: { is_default: true } });
	await expect(section.getByText('default', { exact: true })).toHaveCount(1);
});

test('the uploader names the chosen profile, by key', async ({ page }) => {
	const recorder = await connect(page, {
		profiles: [profile('press', { is_default: true }), profile('studio')]
	});
	// The uploader lives behind the Upload toggle on the assets page rather than its own route.
	await page.goto('/assets');
	await page.getByRole('button', { name: 'Upload' }).click();

	// The default is preselected, because that is what the server would apply anyway — showing "none" while
	// the server silently used a profile would misdescribe what happens.
	await expect(page.getByLabel('Uploading as')).toHaveValue('press');

	await page.getByLabel('Uploading as').selectOption('studio');
	await page
		.getByRole('button', { name: /choose files/i })
		.or(page.locator('input[type=file]'))
		.first();
	await page.locator('input[type=file]').setInputFiles({
		name: 'a.jpg',
		mimeType: 'image/jpeg',
		buffer: Buffer.from([0xff, 0xd8, 0xff, 0xd9])
	});

	await expect.poll(() => recorder.uploadMetadata.length).toBeGreaterThan(0);
	const metadata = recorder.uploadMetadata[0];
	// Base64 of "studio" — the *key*, not the id, matching the server: a client that knows its intake by name
	// should not have to look up a uuid.
	expect(metadata).toContain(`profile ${Buffer.from('studio').toString('base64')}`);
});

test('a profile requiring metadata blocks the picker until acknowledged', async ({ page }) => {
	await connect(page, {
		profiles: [profile('strict', { is_default: true, require_complete: true })]
	});
	await page.goto('/assets');
	await page.getByRole('button', { name: 'Upload' }).click();

	// This is the property that exists *only* here: the server will not refuse a finished upload over
	// `require_complete`, so if the uploader does not apply the rule, nothing does.
	const chooser = page.locator('input[type=file]');
	await expect(chooser).toBeDisabled();
	await expect(page.getByText(/Acknowledge the metadata requirement/)).toBeVisible();

	await page.getByRole('checkbox', { name: /requires complete metadata/ }).check();
	await expect(chooser).toBeEnabled();
});

test('the profile sections have no axe violations', async ({ page }) => {
	await connect(page, {
		profiles: [
			profile('press', { is_default: true, require_complete: true, ai_tags_enabled: false })
		]
	});

	await page.goto('/schema');
	const section = page.getByRole('region', { name: 'Upload profiles' });
	await section.getByRole('button', { name: 'Edit' }).click();
	await expect(section.getByText('Insist on required metadata')).toBeVisible();

	let results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	let detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the profile editor:\n${detail}`).toEqual([]);

	await page.goto('/assets');
	await page.getByRole('button', { name: 'Upload' }).click();
	await expect(page.getByLabel('Uploading as')).toBeVisible();
	results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the uploader:\n${detail}`).toEqual([]);
});
