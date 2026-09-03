import AxeBuilder from '@axe-core/playwright';
import { expect, test } from '@playwright/test';

const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

test('the product tour stays outside the operator shell and clears WCAG 2.1 AA', async ({
	page
}) => {
	await page.goto('/tour');

	await expect(page.getByRole('heading', { level: 1 })).toHaveText(
		'Find every asset. Prove you can use it.'
	);
	await expect(page.getByRole('navigation', { name: 'Product tour' })).toBeVisible();
	await expect(page.getByRole('navigation', { name: 'Main' })).toHaveCount(0);
	await expect(page.locator('main')).toHaveCount(1);

	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map(
			(violation) =>
				`${violation.id} (${violation.impact}): ${violation.nodes.map((node) => node.target.join(' ')).join(', ')}`
		)
		.join('\n');
	expect(results.violations, `axe violations:\n${detail}`).toEqual([]);
});

test('the delivery simulator fails closed and updates from live evidence', async ({ page }) => {
	await page.goto('/tour');

	await expect(page.getByRole('heading', { name: 'Distribution refused' })).toBeVisible();
	await expect(page.getByText('rights=unknown · decision=deny')).toBeVisible();

	await page.getByRole('button', { name: 'Cleared' }).click();
	await expect(page.getByRole('heading', { name: 'Distribution allowed' })).toBeVisible();
	await expect(page.getByText('rights=cleared · decision=allow')).toBeVisible();

	await page.getByRole('button', { name: 'Internal preview' }).click();
	await page.getByRole('button', { name: 'Expired' }).click();
	await expect(page.getByRole('heading', { name: 'Preview delivered' })).toBeVisible();
	await expect(page.getByText('rights verdict skipped by policy')).toBeVisible();
});

test('the product and documentation explorers reveal the selected content', async ({ page }) => {
	await page.goto('/tour');

	await page.getByRole('tab', { name: '02 Trust it' }).click();
	await expect(
		page.getByRole('heading', { name: 'Evidence travels with the asset.' })
	).toBeVisible();

	await page.getByRole('tab', { name: 'API', exact: true }).click();
	await expect(
		page.getByRole('heading', { name: 'One contract, generated clients' })
	).toBeVisible();
	await expect(page.getByText('143 paths and 190 operations')).toBeVisible();
});

test('the mobile tour fits the viewport and exposes its navigation', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto('/tour');

	const pageWidth = await page.evaluate(() => document.documentElement.scrollWidth);
	expect(pageWidth).toBe(390);

	await page.getByRole('button', { name: 'Toggle navigation' }).click();
	await expect(page.getByRole('navigation', { name: 'Product tour' })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Rights gate' })).toBeVisible();
});
