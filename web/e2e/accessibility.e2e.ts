/**
 * The D10 gate: WCAG 2.1 AA is a release gate from the first UI commit, not a later audit.
 *
 * ARCHITECTURE §14.2 is explicit that automated scanning is "necessary, and nowhere near
 * sufficient" — it catches roughly 40% of issues. So this file does two things: it runs axe, and it
 * asserts the handful of structural properties axe cannot see but every keyboard and screen-reader
 * user depends on. A scan that passes on a page with no landmarks and no skip link is exactly the
 * false comfort §14.2 warns about.
 */
import { expect, test } from '@playwright/test';
import AxeBuilder from '@axe-core/playwright';

/** WCAG 2.1 A and AA — the operative benchmark, since 2.2 is not yet in the harmonised standard. */
const WCAG_21_AA = ['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa'];

test('the home page has no axe violations at WCAG 2.1 AA', async ({ page }) => {
	await page.goto('/');
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();

	// Printed rather than summarised: "3 violations" sends someone hunting, while the rule id and
	// the failing selector are actionable from the CI log alone.
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(' ')).join(', ')}`)
		.join('\n');
	expect(results.violations, `axe violations:\n${detail}`).toEqual([]);
});

test('a skip link is the first thing a keyboard user reaches', async ({ page }) => {
	// The most-used accessibility affordance in a dense app, and invisible to axe: it cannot tell
	// that a keyboard user has to traverse a filter rail before reaching the grid.
	await page.goto('/');
	await page.keyboard.press('Tab');

	const focused = page.locator(':focus');
	await expect(focused).toHaveAttribute('href', '#main-content');
	await expect(focused).toBeVisible();
});

test('the skip link moves focus to the main landmark', async ({ page }) => {
	// A skip link that scrolls without moving focus is worse than none: the next Tab continues from
	// the header, so a screen-reader user is told they have skipped and then finds they have not.
	await page.goto('/');
	await page.keyboard.press('Tab');
	await page.keyboard.press('Enter');
	await expect(page.locator(':focus')).toHaveAttribute('id', 'main-content');
});

test('the page has exactly one main landmark and one h1', async ({ page }) => {
	await page.goto('/');
	await expect(page.locator('main')).toHaveCount(1);
	await expect(page.locator('h1')).toHaveCount(1);
	await expect(page.locator('main')).toHaveAttribute('id', 'main-content');
});

test('the document declares its language', async ({ page }) => {
	// Without it a screen reader reads English content with the user's default voice — which for a
	// German user renders every asset title as gibberish.
	await page.goto('/');
	await expect(page.locator('html')).toHaveAttribute('lang', 'en');
});

test('the viewport allows zoom, which WCAG 1.4.4 requires', async ({ page }) => {
	// `user-scalable=no` or a `maximum-scale` below 2 fails 1.4.4 outright, and it is the kind of
	// thing a well-meaning mobile tweak adds months later.
	await page.goto('/');
	const viewport = await page.locator('meta[name="viewport"]').getAttribute('content');
	expect(viewport).not.toContain('user-scalable=no');
	expect(viewport).not.toMatch(/maximum-scale=1(\.0)?\b/);
});

test('every state of every dimension clears WCAG 2.1 AA contrast', async ({ page }) => {
	// The /style page renders every variant of all four dimensions, so this one scan covers the
	// contrast of every token pair. A tint plus a hue is exactly the combination that looks fine and
	// measures 3.9:1, and checking it by eye is how that ships.
	await page.goto('/style');
	const results = await new AxeBuilder({ page }).withTags(WCAG_21_AA).analyze();
	const detail = results.violations
		.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.failureSummary).join(' | ')}`)
		.join('\n');
	expect(results.violations, `axe violations on /style:\n${detail}`).toEqual([]);
});

test('the state reference page is navigable by heading structure', async ({ page }) => {
	// Screen-reader users navigate by heading, so a reference page with one flat list of badges and no
	// headings is unusable even though axe is happy with it.
	await page.goto('/style');
	await expect(page.locator('h1')).toHaveCount(1);
	await expect(page.locator('h2')).toHaveCount(4);
});
