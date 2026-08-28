import { test as base, expect } from '@playwright/test';

/**
 * `test` and `expect`, with one change: `page.goto` waits for the page to stop fetching before it
 * returns.
 *
 * ## Why this exists
 *
 * Playwright's `click()` waits for an element to be *actionable* — attached, visible, stable, not
 * obscured. It cannot wait for a Svelte event handler to be attached, because that is not something the
 * DOM exposes. So a click that lands between first paint and hydration hits a button that looks
 * completely ready and does nothing at all. No request goes out, no panel opens, and the failure
 * surfaces later as `element(s) not found` on whatever the click was supposed to produce — thirty
 * seconds away from its cause, in an assertion that is not wrong.
 *
 * That was this suite's flakiness. Measured before fixing: three full runs lost 4, 3 and 1 of 410 tests
 * with **no test failing in more than one run**, which is the signature of a race rather than a bug.
 * `browse.e2e.ts` appeared in every list, and the reason turned out to be countable — 85 places in the
 * suite navigate and then immediately click or fill, and 30 of them are in that one file. The
 * distribution of failures followed the distribution of unsettled navigations.
 *
 * ## Why here rather than at the call sites
 *
 * 85 `await page.waitForLoadState(...)` lines would fix the 85 that exist and none of the ones written
 * next month. Overriding the fixture makes the guarantee a property of `goto` itself, so a test cannot
 * opt out by being written the obvious way.
 *
 * ## Why `networkidle`
 *
 * Playwright's own docs discourage it for general use, and they are right for a page talking to a real
 * network where "idle" may never arrive. It is the correct tool *here*: every one of these tests mocks
 * its HTTP boundary through `page.route`, so the responses are synthetic and immediate and idle is
 * reached in milliseconds. It also means what the tests need — the initial fetches have resolved, so
 * the component tree has the data it renders from, and hydration has had a turn.
 */
export const test = base.extend<Record<string, never>>({
	page: async ({ page }, use) => {
		const navigate = page.goto.bind(page);
		page.goto = async (url, options) => {
			const response = await navigate(url, options);
			// An explicit `waitUntil` is a test saying it knows which moment it wants, so settling on top of
			// it would be this fixture overriding the thing the test came for. A few cases exist and they
			// are the interesting ones: `branding.e2e.ts` asserts that a customer's header never flashes the
			// vendor's name *while branding is still loading*, and it can only observe that before the load
			// finishes. Settling unconditionally made that test watch the settled page and fail — correctly,
			// which is how this exception was found rather than assumed.
			if (options?.waitUntil === undefined) {
				// Not fatal on its own. A page that never goes idle is a test that will fail on its next
				// assertion with a message about the thing it was actually waiting for, which is more useful
				// than this helper throwing about load states.
				await page.waitForLoadState('networkidle').catch(() => {});
			}
			return response;
		};
		await use(page);
	}
});

export { expect };
