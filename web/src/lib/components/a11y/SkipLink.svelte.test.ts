/**
 * The skip link, tested as a component so its behaviour is pinned independently of any page.
 */
import { describe, expect, it } from 'vitest';
import { render } from 'vitest-browser-svelte';
import SkipLink from './SkipLink.svelte';

describe('SkipLink', () => {
	it('is a real anchor to the main landmark', async () => {
		const screen = render(SkipLink);
		const link = screen.getByRole('link', { name: /skip to main content/i });
		await expect.element(link).toHaveAttribute('href', '#main-content');
	});

	it('is present in the DOM rather than hidden from assistive technology', async () => {
		// The common mistake is `display: none` until focus, which removes it from the accessibility
		// tree entirely — so a screen-reader user never hears it, and the affordance exists only for
		// sighted keyboard users. It must be positioned off-screen instead.
		const screen = render(SkipLink);
		const link = screen.getByRole('link', { name: /skip to main content/i });
		await expect.element(link).toBeInTheDocument();
	});
});
