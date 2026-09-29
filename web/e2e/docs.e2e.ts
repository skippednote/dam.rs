import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

async function expectNoAxeViolations(page: Page) {
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map(
			(violation) =>
				`${violation.id} (${violation.impact}): ${violation.nodes.map((node) => node.target.join(' ')).join(', ')}`
		)
		.join('\n');
	expect(results.violations, `axe violations:\n${detail}`).toEqual([]);
}

test('the documentation has one landmark, persistent navigation and no axe violations', async ({
	page
}) => {
	await page.goto('/tour/docs');

	await expect(page.getByRole('heading', { level: 1 })).toHaveText('Get a local library running');
	await expect(page.getByRole('complementary', { name: 'Documentation navigation' })).toBeVisible();
	await expect(page.getByRole('complementary', { name: 'On this page' })).toBeVisible();
	await expect(page.getByRole('navigation', { name: 'Main' })).toHaveCount(0);
	await expect(page.locator('main')).toHaveCount(1);

	await expectNoAxeViolations(page);
});

test('documentation search filters the navigation without losing the current article', async ({
	page
}) => {
	await page.goto('/tour/docs');
	const sidebar = page.getByRole('complementary', { name: 'Documentation navigation' });

	await sidebar.getByRole('searchbox', { name: 'Search documentation' }).fill('audit');
	await expect(sidebar.getByRole('link', { name: 'Security model' })).toBeVisible();
	await expect(sidebar.getByRole('link', { name: 'Architecture' })).toHaveCount(0);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText('Get a local library running');

	await sidebar.getByRole('searchbox', { name: 'Search documentation' }).fill('no-such-topic');
	await expect(sidebar.getByText('No matching topic')).toBeVisible();
});

test('every documentation journey has a deep-linkable page', async ({ page }) => {
	for (const [path, heading] of [
		['/tour/docs/architecture', 'Architecture and invariants'],
		['/tour/docs/ingest-delivery', 'From upload to governed delivery'],
		['/tour/docs/api-mcp', 'Integrate through REST or MCP'],
		['/tour/docs/operations', 'Deploy and operate dam.rs'],
		['/tour/docs/security', 'Security and governance model'],
		['/tour/docs/contributing', 'Contribute with evidence']
	] as const) {
		await page.goto(path);
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(heading);
		await expect(page).toHaveTitle(/· dam\.rs$/);
		await expectNoAxeViolations(page);
	}
});

test('the product tour opens the full documentation instead of a teaser anchor', async ({
	page
}) => {
	await page.goto('/tour');
	await expect(page.getByRole('link', { name: 'Read the docs' })).toHaveAttribute(
		'href',
		'/tour/docs'
	);
	await expect(page.getByRole('link', { name: 'Open full guide' }).first()).toHaveAttribute(
		'href',
		'/tour/docs'
	);
});

test('mobile documentation fits and exposes its page navigation', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto('/tour/docs/operations');

	expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(390);
	await page.getByRole('button', { name: 'Toggle documentation navigation' }).click();
	const sidebar = page.getByRole('complementary', { name: 'Documentation navigation' });
	await expect(sidebar).toBeVisible();
	await expect(sidebar.getByRole('link', { name: 'Security model', exact: true })).toBeVisible();
});
