/**
 * The near-duplicate review queue through a real browser (M4, §8.1).
 *
 * The Rust suites prove the hashing, the pair ordering, the dismissal rule and the scope filter. What lives
 * only here are four properties of the *screen*:
 *
 * - **Two pictures, big enough to judge.** The whole task is "are these the same thing", so the images are the
 *   largest element and the metadata sits under them.
 * - **The distance is explained, not printed.** "3 of 64 bits differ" means nothing to a person; the row says
 *   what the number implies.
 * - **The third button is honest about doing nothing.** Recording that two assets are duplicates cannot decide
 *   which one survives — that has rights consequences — so the screen says so beside the button rather than
 *   letting somebody assume.
 * - **An empty queue distinguishes its two meanings**: nothing found, or nothing you can see both halves of.
 */
import { expect, test } from './fixtures';
import { type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

type Pair = {
	id: string;
	left: Side;
	right: Side;
	hamming: number | null;
	cosine: number | null;
	relation: string | null;
};
type Side = {
	asset_id: string;
	filename: string;
	mime: string;
	bytes: number;
	thumbnail_url: string | null;
};

function side(name: string, overrides: Partial<Side> = {}): Side {
	return {
		asset_id: `${name}-1111-4111-8111-111111111111`.slice(0, 36),
		filename: `${name}.jpg`,
		mime: 'image/jpeg',
		bytes: 4_200_000,
		thumbnail_url: null,
		...overrides
	};
}

function pair(overrides: Partial<Pair> = {}): Pair {
	return {
		id: 'aaaaaaaa-1111-4111-8111-111111111111',
		left: side('IMG_0186'),
		right: side('IMG_0186(1)'),
		hamming: 0,
		cosine: null,
		relation: 'near_identical',
		...overrides
	};
}

async function connect(page: Page, options: { pairs?: Pair[] } = {}) {
	const recorder = { resolved: [] as { id: string; state: string }[] };

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		if (url.pathname === '/duplicates' && route.request().method() === 'GET') {
			return route.fulfill({ json: options.pairs ?? [pair()] });
		}
		if (url.pathname.startsWith('/duplicates/')) {
			const body = route.request().postDataJSON() as { state: string };
			recorder.resolved.push({
				id: url.pathname.split('/').pop() ?? '',
				state: body.state
			});
			return route.fulfill({ status: 204, body: '' });
		}
		if (url.pathname === '/me') {
			return route.fulfill({ json: { id: 'p-ada', name: 'Ada', email: 'a@x' } });
		}
		return route.fulfill({ json: {} });
	});

	return recorder;
}

test('the distance is explained rather than printed', async ({ page }) => {
	// "0 of 64 bits differ" means nothing to a person. The number is still shown, because somebody who knows
	// what it is will want it — but the sentence is what a reviewer reads.
	await connect(page, {
		pairs: [
			pair({ id: 'a1111111-1111-4111-8111-111111111111', hamming: 0 }),
			pair({ id: 'b1111111-1111-4111-8111-111111111111', hamming: 2 }),
			pair({ id: 'c1111111-1111-4111-8111-111111111111', hamming: 5, relation: 'variant' }),
			pair({ id: 'd1111111-1111-4111-8111-111111111111', hamming: 11, relation: 'variant' })
		]
	});
	await page.goto('/duplicates');

	await expect(page.getByText('pixel-identical after scaling')).toBeVisible();
	await expect(page.getByText('almost certainly the same picture')).toBeVisible();
	await expect(page.getByText('very likely the same picture')).toBeVisible();
	await expect(page.getByText('possibly a crop, recolour or re-edit')).toBeVisible();
	await expect(page.getByText('11 of 64 bits differ')).toBeVisible();

	// And the footnote says where the number comes from, including the threshold — so nobody wonders why a
	// duplicate they expected is absent.
	await expect(page.getByText('Hamming distance', { exact: false })).toContainText(
		'past twelve is not shown'
	);
});

test('both pictures are shown, with their own facts underneath', async ({ page }) => {
	await connect(page, {
		pairs: [
			pair({
				left: side('harbour', { thumbnail_url: '/d/abc', bytes: 4_200_000 }),
				right: side('harbour-copy', { thumbnail_url: '/d/def', bytes: 1_100_000 })
			})
		]
	});
	await page.goto('/duplicates');

	// Two images, because the task is looking at them.
	await expect(page.locator('figure img')).toHaveCount(2);
	await expect(page.getByText('harbour.jpg')).toBeVisible();
	await expect(page.getByText('harbour-copy.jpg')).toBeVisible();
	// The sizes differ, which is often the whole tell for a re-export.
	await expect(page.getByText('4.0 MiB')).toBeVisible();
	await expect(page.getByText('1.0 MiB')).toBeVisible();
});

test('a side with no rendered preview says so instead of showing a broken image', async ({
	page
}) => {
	await connect(page, { pairs: [pair()] });
	await page.goto('/duplicates');

	await expect(page.locator('figure img')).toHaveCount(0);
	await expect(page.getByText('no preview rendered')).toHaveCount(2);
});

test('the three verdicts are recorded, and the destructive-sounding one says it is not', async ({
	page
}) => {
	const recorder = await connect(page, {
		pairs: [
			pair({ id: 'a1111111-1111-4111-8111-111111111111' }),
			pair({ id: 'b1111111-1111-4111-8111-111111111111' }),
			pair({ id: 'c1111111-1111-4111-8111-111111111111' })
		]
	});
	await page.goto('/duplicates');

	// Said beside the buttons, because "one should go" reads like it will do something. Deciding which of two
	// licensed deliverables survives has rights consequences this screen cannot weigh.
	await expect(page.getByText('Recording a verdict deletes nothing').first()).toBeVisible();

	await page.getByRole('button', { name: 'Not duplicates' }).first().click();
	await expect(page.getByRole('status')).toContainText('will not come back');

	await page.getByRole('button', { name: 'Duplicates, keep both' }).first().click();
	await expect(page.getByRole('status')).toContainText('Recorded as duplicates');

	await page.getByRole('button', { name: 'Duplicates, one should go' }).first().click();
	await expect(page.getByRole('status')).toContainText('Nothing was deleted');

	expect(recorder.resolved.map((one) => one.state)).toEqual(['dismissed', 'confirmed', 'merged']);
});

test('a resolved pair leaves the list without reordering the rest', async ({ page }) => {
	// Dropped locally rather than refetched: a reviewer working down a long queue should not have it reorder
	// under them after every verdict.
	await connect(page, {
		pairs: [
			pair({ id: 'a1111111-1111-4111-8111-111111111111', left: side('first') }),
			pair({ id: 'b1111111-1111-4111-8111-111111111111', left: side('second') }),
			pair({ id: 'c1111111-1111-4111-8111-111111111111', left: side('third') })
		]
	});
	await page.goto('/duplicates');
	await expect(page.getByText('3 pairs · most alike first')).toBeVisible();

	await page.getByRole('button', { name: 'Not duplicates' }).nth(1).click();
	await expect(page.getByText('second.jpg')).toHaveCount(0);
	await expect(page.getByText('first.jpg')).toBeVisible();
	await expect(page.getByText('third.jpg')).toBeVisible();
});

test('an empty queue distinguishes its two meanings', async ({ page }) => {
	// "Nothing found" and "nothing you can see both halves of" are different facts, and a reviewer who was told
	// there were duplicates needs to know which one they are looking at.
	await connect(page, { pairs: [] });
	await page.goto('/duplicates');

	await expect(page.getByText('Nothing to review', { exact: false })).toContainText(
		'none where you can see both sides'
	);
});

test('the page explains that identical files never reach the queue', async ({ page }) => {
	// Otherwise the obvious question is why the two copies somebody knows about are absent.
	await connect(page);
	await page.goto('/duplicates');
	await expect(page.getByText('Identical files never reach here', { exact: false })).toContainText(
		'stores those once'
	);
});

test('the duplicates screen has no axe violations', async ({ page }) => {
	await connect(page, {
		pairs: [
			pair({
				left: side('a', { thumbnail_url: '/d/abc' }),
				right: side('b', { thumbnail_url: '/d/def' })
			}),
			pair({ id: 'b1111111-1111-4111-8111-111111111111', hamming: 9, relation: 'variant' })
		]
	});
	await page.goto('/duplicates');
	await expect(page.getByText('2 pairs · most alike first')).toBeVisible();
	expect((await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze()).violations).toEqual([]);
});
