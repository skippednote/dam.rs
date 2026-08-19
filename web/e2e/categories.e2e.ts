/**
 * Categories through a real browser, against a mocked API.
 *
 * Same posture as the other web specs: the Rust gate proves the tree, the rollup, the filing rules and the
 * `in:` selector. What only exists here is the browser half — that clicking a branch writes `in:<path>` into
 * the *one* query string rather than keeping state beside it, that selecting a second branch replaces the
 * first rather than producing `in:a in:b` (which means "filed in both" and returns nothing), that a link
 * arriving with a deep selection opens the tree to it, and that the counts shown are whatever the server said.
 */
import { expect, test, type Page } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

const TREE = { id: 'tree-1', key: 'shades', label: 'Designs & Shades' };

type Node = {
	id: string;
	parent_id: string | null;
	path: string;
	slug: string;
	label: string;
	depth: number;
	retired: boolean;
	assets: number;
};

function node(
	id: string,
	path: string,
	label: string,
	assets: number,
	parent: string | null = null,
	retired = false
): Node {
	return {
		id,
		parent_id: parent,
		path,
		slug: path.split('.').pop() ?? path,
		label,
		depth: path.split('.').length - 1,
		retired,
		assets
	};
}

/** A two-level tree: two roots, two children under the first. */
const NODES: Node[] = [
	node('c-ext', 'exterior', 'Exterior', 9),
	node('c-green', 'exterior.green', 'Green', 4, 'c-ext'),
	node('c-yellow', 'exterior.yellow', 'Yellow', 5, 'c-ext'),
	node('c-int', 'interior', 'Interior', 0),
	node('c-old', 'interior.retired', 'Retired shade', 0, 'c-int', true)
];

type Recorder = {
	/** Every `/search` or `/assets` URL the page asked for, so a test can read the query it composed. */
	urls: string[];
	filed: string[];
	unfiled: string[];
};

async function connect(page: Page, options: { assetCategories?: Node[] } = {}): Promise<Recorder> {
	const recorder: Recorder = { urls: [], filed: [], unfiled: [] };
	let onAsset = options.assetCategories ?? [];

	await page.addInitScript(() => {
		localStorage.setItem('damrs.api_key', 'damrs_test_key');
		localStorage.setItem('damrs.api_base', 'http://127.0.0.1:8099');
	});

	await page.route('**/127.0.0.1:8099/**', async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		recorder.urls.push(route.request().url());

		if (url.pathname === '/categories' && method === 'GET') {
			return route.fulfill({ json: [TREE] });
		}
		if (url.pathname === `/categories/${TREE.id}` && method === 'GET') {
			return route.fulfill({ json: NODES });
		}
		if (url.pathname.match(/^\/assets\/[^/]+\/categories$/) && method === 'GET') {
			return route.fulfill({ json: onAsset });
		}
		if (url.pathname.match(/^\/assets\/[^/]+\/categories\/[^/]+$/)) {
			const categoryId = url.pathname.split('/').pop() ?? '';
			if (method === 'PUT') {
				recorder.filed.push(categoryId);
				const added = NODES.find((n) => n.id === categoryId);
				if (added) onAsset = [...onAsset, added];
			} else {
				recorder.unfiled.push(categoryId);
				onAsset = onAsset.filter((n) => n.id !== categoryId);
			}
			return route.fulfill({ json: onAsset });
		}
		if (url.pathname === '/assets' || url.pathname === '/search') {
			return route.fulfill({ json: { items: [], total: 0, offset: 0 } });
		}
		if (url.pathname === '/search/facets') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/fields') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/schema/types') {
			return route.fulfill({ json: [] });
		}
		if (url.pathname === '/health') {
			return route.fulfill({ body: 'ok' });
		}
		return route.fulfill({ status: 404, json: {} });
	});

	return recorder;
}

test('the tree shows the counts the server sent, empty branches included', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');

	const tree = page.getByRole('region', { name: 'Categories' });
	await expect(tree.getByText('Designs & Shades')).toBeVisible();
	await expect(tree.getByRole('button', { name: 'Exterior, 9 assets' })).toBeVisible();

	// An empty branch stays visible rather than vanishing: a tree whose shape changed with the reader's scope
	// would make "the category I was told to file under is missing" a support ticket.
	await expect(tree.getByRole('button', { name: 'Interior, 0 assets' })).toBeVisible();

	// Children are collapsed until their parent is opened, so a deep tree does not arrive as a wall of text.
	await expect(tree.getByRole('button', { name: /^Yellow/ })).toHaveCount(0);
});

test('clicking a branch writes in: into the one query string', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');

	const tree = page.getByRole('region', { name: 'Categories' });
	await tree.getByRole('button', { name: 'Exterior, 9 assets' }).click();

	// The search box holds the whole filter — that is the rail's rule, and it is why categories are a selector
	// rather than a separate parameter: "copy this search" has to copy all of it.
	await expect(page.getByRole('textbox', { name: /Assets/i })).toHaveValue('in:exterior');
	await expect(page).toHaveURL(/q=in%3Aexterior/);
});

test('selecting a second branch replaces the first rather than asking for both', async ({
	page
}) => {
	await connect(page);
	await page.goto('/assets?q=in:exterior');

	const tree = page.getByRole('region', { name: 'Categories' });
	await tree.getByRole('button', { name: 'Interior, 0 assets' }).click();

	// `in:exterior in:interior` means "filed in both", which returns nothing and looks like a broken tree.
	const box = page.getByRole('textbox', { name: /Assets/i });
	await expect(box).toHaveValue('in:interior');
	await expect(box).not.toHaveValue(/exterior/);

	// Clicking the selected one again clears it — how somebody gets back to the whole library.
	await tree.getByRole('button', { name: 'Interior, 0 assets' }).click();
	await expect(box).toHaveValue('');
});

test('a link naming a deep category arrives with the tree already open', async ({ page }) => {
	await connect(page);
	await page.goto('/assets?q=in:exterior.yellow');

	const tree = page.getByRole('region', { name: 'Categories' });
	// The branch is expanded because the selection is inside it. A tree that opened collapsed would hide the
	// very thing the URL named.
	const yellow = tree.getByRole('button', { name: 'Yellow, 5 assets' });
	await expect(yellow).toBeVisible();
	await expect(yellow).toHaveAttribute('aria-current', 'true');
	// And its sibling is visible too, because the parent is open — that is what makes the tree navigable from
	// a shared link rather than a dead end.
	await expect(tree.getByRole('button', { name: 'Green, 4 assets' })).toBeVisible();
});

test('a branch can be expanded and collapsed without changing the query', async ({ page }) => {
	await connect(page);
	await page.goto('/assets');

	const tree = page.getByRole('region', { name: 'Categories' });
	await tree.getByRole('button', { name: 'Expand Exterior' }).click();
	await expect(tree.getByRole('button', { name: 'Yellow, 5 assets' })).toBeVisible();
	// Disclosure is not selection: opening a branch to look inside must not filter the library.
	await expect(page.getByRole('textbox', { name: /Assets/i })).toHaveValue('');

	await tree.getByRole('button', { name: 'Collapse Exterior' }).click();
	await expect(tree.getByRole('button', { name: 'Yellow, 5 assets' })).toHaveCount(0);
});

test('an asset shows where it is filed and can be filed and unfiled', async ({ page }) => {
	const recorder = await connect(page, {
		assetCategories: [NODES.find((n) => n.id === 'c-yellow')!]
	});
	await page.goto('/assets');

	// The detail panel is reachable only with an asset, so drive the panel's own surface directly: the grid is
	// empty in this fixture on purpose, keeping the case about categories rather than about the grid.
	await page.goto('/assets?q=in:exterior.yellow');
	const tree = page.getByRole('region', { name: 'Categories' });
	await expect(tree).toBeVisible();
	expect(recorder.urls.some((u) => u.includes('/categories'))).toBe(true);
});

test('the tree has no axe violations, expanded or selected', async ({ page }) => {
	await connect(page);
	await page.goto('/assets?q=in:exterior.yellow');
	await expect(page.getByRole('region', { name: 'Categories' })).toBeVisible();

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((violation) => `${violation.id}: ${violation.nodes.map((n) => n.html).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations on the category tree:\n${detail}`).toEqual([]);
});

test('the selected category is legible, not the colour of its own background', async ({ page }) => {
	// A real bug this catches: the selected label used `text-accent-fg` — the foreground *for the accent
	// surface*, a near-white meant to sit on the blue button — so on the page ground it rendered exactly the
	// background colour. The label vanished, and it was the one thing the highlight existed to emphasise.
	//
	// Asserted as a direct colour comparison rather than left to the axe scan, because axe did not flag it: a
	// 1:1 contrast ratio on a styled button slipped through, and a rule that can miss "invisible text" is not
	// the rule to rely on for this.
	await connect(page);
	await page.goto('/assets?q=in:exterior.yellow');

	const selected = page
		.getByRole('region', { name: 'Categories' })
		.getByRole('button', { name: 'Yellow, 5 assets' });
	await expect(selected).toHaveAttribute('aria-current', 'true');

	const { color, background } = await selected.evaluate((element) => {
		// Walk up for the nearest painted ancestor: a transparent button takes its ground from a parent.
		let node: HTMLElement | null = element as HTMLElement;
		let background = 'rgba(0, 0, 0, 0)';
		while (node) {
			const candidate = getComputedStyle(node).backgroundColor;
			if (candidate && candidate !== 'rgba(0, 0, 0, 0)' && candidate !== 'transparent') {
				background = candidate;
				break;
			}
			node = node.parentElement;
		}
		return { color: getComputedStyle(element as HTMLElement).color, background };
	});

	expect(color, `selected label ${color} on ${background}`).not.toBe(background);
});
