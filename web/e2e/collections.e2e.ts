/**
 * Collections through a real browser, against a mocked API (Q.14b).
 *
 * The Rust gates prove ordering, the pin union, the scope filter and the portal guard. What exists only here
 * are five properties of the *interface*:
 *
 * - **The key is asked for once and shown forever.** A portal references it, so the form states that it
 *   cannot change and the edit panel repeats which key is staying.
 * - **Pinning is described as what it costs.** "pin_hot" tells nobody anything; "keeps every member's original
 *   in the hottest storage class" is the sentence somebody can make a decision with.
 * - **A removal takes the server's order back.** Removing closes the gap it left, so every later position
 *   changes. The first version of this screen filtered locally and left stale numbers behind — which then
 *   tripped the scoped-members banner on a hole that did not exist. This is that regression, written down.
 * - **A real hole says why.** When the server withholds a member the caller cannot see, the numbering has a
 *   gap and the screen explains it rather than renumbering over it.
 * - **A refused delete is shown as it arrived.** The 409 names how many portals publish the collection, and
 *   that sentence is the whole value of the guard.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];
const PRESS = '11111111-1111-4111-8111-111111111111';

type Item = { asset_id: string; position: number; filename: string; mime: string };

function collection(overrides: Record<string, unknown> = {}) {
	return {
		id: PRESS,
		key: 'press-kit',
		label: 'Acme press kit',
		description: 'Everything a journalist needs.',
		visibility: 'shared',
		pin_hot: false,
		item_count: 3,
		...overrides
	};
}

function items(count: number): Item[] {
	return Array.from({ length: count }, (_, index) => ({
		asset_id: `2222222${index}-2222-4222-8222-222222222222`,
		position: index,
		filename: `harbour-${index}.jpg`,
		mime: 'image/jpeg',
		thumbnail_url: null
	}));
}

async function connect(
	page: Page,
	options: {
		collections?: Record<string, unknown>[];
		members?: Item[];
		/** What DELETE answers with. Absent means 204. */
		deleteRefusal?: { status: number; reason: string };
	} = {}
) {
	const recorder = { deletes: 0, patches: [] as unknown[], reads: 0 };
	let members = options.members ?? items(3);

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const path = url.pathname;
		const method = route.request().method();

		if (path === '/collections' && method === 'GET') {
			return route.fulfill({ json: options.collections ?? [collection()] });
		}
		if (path === '/collections' && method === 'POST') {
			const body = JSON.parse(route.request().postData() ?? '{}');
			return route.fulfill({
				status: 201,
				json: collection({
					id: '33333333-3333-4333-8333-333333333333',
					key: body.key,
					label: body.label,
					visibility: 'private',
					pin_hot: body.pin_hot ?? false,
					item_count: 0
				})
			});
		}
		if (path.endsWith('/items') && method === 'GET') {
			recorder.reads += 1;
			return route.fulfill({ json: members });
		}
		if (path.includes('/items/') && method === 'DELETE') {
			// What the server does: the row goes and the gap closes, so every later position moves down.
			const gone = path.split('/items/')[1];
			members = members
				.filter((item) => item.asset_id !== gone)
				.map((item, index) => ({ ...item, position: index }));
			return route.fulfill({ status: 204, body: '' });
		}
		if (path.match(/\/collections\/[^/]+$/) && method === 'PATCH') {
			const body = JSON.parse(route.request().postData() ?? '{}');
			recorder.patches.push(body);
			// The key is echoed unchanged whatever the client sent, which is the server's actual behaviour.
			return route.fulfill({ json: collection({ ...body, key: 'press-kit' }) });
		}
		if (path.match(/\/collections\/[^/]+$/) && method === 'DELETE') {
			recorder.deletes += 1;
			if (options.deleteRefusal) {
				return route.fulfill({
					status: options.deleteRefusal.status,
					json: { reason: options.deleteRefusal.reason }
				});
			}
			return route.fulfill({ status: 204, body: '' });
		}
		if (path === '/me') return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('the key is stated as unchangeable, on the form and again when editing', async ({ page }) => {
	await connect(page);
	await page.goto('/collections');

	await expect(page.getByText('The key cannot be changed later')).toContainText(
		'a portal references it'
	);
	// And the reason pinning exists, in money-and-latency terms rather than as a field name.
	await expect(page.getByText('Pinning keeps every member’s original')).toContainText(
		'never waits on a restore'
	);

	await page.getByRole('button', { name: 'Edit' }).click();
	await expect(page.getByText('The key stays press-kit')).toBeVisible();
});

test('a created collection reports its key, because that is what a portal will use', async ({
	page
}) => {
	await connect(page);
	await page.goto('/collections');

	await page.getByLabel('Key').fill('spring-2026');
	await page.getByRole('button', { name: 'Create' }).click();

	await expect(page.getByRole('status')).toContainText('Its key is spring-2026');
	// Created private: a collection is somebody's working set until they say otherwise.
	await expect(page.getByText('private')).toBeVisible();
});

test('amending sends the label and never a key', async ({ page }) => {
	const recorder = await connect(page);
	await page.goto('/collections');

	await page.getByRole('button', { name: 'Edit' }).click();
	await page.getByLabel('Label').last().fill('Press kit, renamed');
	await page.getByRole('button', { name: 'Save' }).click();

	await expect(page.getByRole('status')).toContainText('saved');
	expect(recorder.patches).toHaveLength(1);
	const sent = recorder.patches[0] as Record<string, unknown>;
	expect(sent.label).toBe('Press kit, renamed');
	expect(sent).not.toHaveProperty('key');
});

test('removing a member refetches, so the positions are never stale', async ({ page }) => {
	// The regression this file exists for. Removing position 0 renumbers 1→0 and 2→1 on the server; a screen
	// that filtered locally would keep showing 1 and 2 and then claim, from the gap at 0, that the collection
	// holds assets outside the caller's scope.
	const recorder = await connect(page);
	await page.goto('/collections');
	await page.getByRole('button', { name: 'Members' }).click();
	await expect(page.getByText('harbour-0.jpg')).toBeVisible();

	const before = recorder.reads;
	await page.getByRole('button', { name: 'Remove harbour-0.jpg from Acme press kit' }).click();

	await expect(page.getByRole('status')).toContainText('harbour-0.jpg removed');
	expect(recorder.reads).toBe(before + 1);
	// The row, not any mention of it: the notice line names the file it just removed.
	await expect(page.locator('ol > li').filter({ hasText: 'harbour-0.jpg' })).toHaveCount(0);
	// Dense again, and no scope claim: the two survivors are 0 and 1.
	await expect(page.getByText('this collection holds assets outside your scope')).toHaveCount(0);
	const numbers = await page
		.getByRole('listitem')
		.getByText(/^[0-9]+$/)
		.allTextContents();
	expect(numbers).toEqual(['0', '1']);
});

test('a genuine gap in the positions is explained rather than renumbered', async ({ page }) => {
	// What a scoped curator sees: the server returned members at 0 and 2 because it withheld the one at 1.
	await connect(page, {
		members: [
			{ ...items(1)[0], position: 0 },
			{ ...items(3)[2], position: 2 }
		]
	});
	await page.goto('/collections');
	await page.getByRole('button', { name: 'Members' }).click();

	await expect(page.getByText('this collection holds assets outside your scope')).toContainText(
		'The numbers are the real ones'
	);
});

test('a delete refused by a portal shows the refusal it arrived with', async ({ page }) => {
	await connect(page, {
		deleteRefusal: {
			status: 409,
			reason: '2 portal(s) publish this collection; delete or repoint them first'
		}
	});
	await page.goto('/collections');
	await page.getByRole('button', { name: 'Delete' }).click();

	// Named and counted, with the fix stated: a guard nobody can act on is a guard that generates a ticket.
	await expect(page.getByRole('alert')).toContainText('2 portal(s) publish this collection');
	// And the row is still there, because the refusal is a guard and not a half-delete.
	await expect(page.getByText('Acme press kit')).toBeVisible();
});

test('an empty library says where a portal starts', async ({ page }) => {
	await connect(page, { collections: [] });
	await page.goto('/collections');

	await expect(page.getByText('No collections yet', { exact: false })).toContainText(
		'Add to collection'
	);
});

test('the collections screen has no axe violations', async ({ page }) => {
	await connect(page);
	await page.goto('/collections');
	await page.getByRole('button', { name: 'Members' }).click();
	await expect(page.getByText('harbour-0.jpg')).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	expect(results.violations).toEqual([]);
});
