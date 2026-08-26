/**
 * The state badges, rendered.
 *
 * The vocabulary tests pin the channel assignment; these pin what assistive technology receives —
 * which is a different question and the one that is easier to get wrong. A badge can have a perfect
 * colour token and still announce itself as "black circle".
 */
import { describe, expect, it } from 'vitest';
import { render } from 'vitest-browser-svelte';
import TierBadge from './TierBadge.svelte';
import RightsBadge from './RightsBadge.svelte';
import ProvenanceBadge from './ProvenanceBadge.svelte';
import ConfidenceBar from './ConfidenceBar.svelte';
import { PROVENANCE_STATES, RIGHTS_STATES, TIERS } from './vocabulary';

describe('TierBadge', () => {
	it('announces the tier in words', async () => {
		const screen = await render(TierBadge, { tier: 'archive' });
		await expect.element(screen.getByText('Archived')).toBeInTheDocument();
	});

	it('hides the decorative glyph from assistive technology', async () => {
		// Without aria-hidden a screen reader reads "dotted circle Archived", and the glyph is pure
		// duplication of the label.
		const screen = await render(TierBadge, { tier: 'archive' });
		const glyph = screen.getByTestId('tier-glyph');
		await expect.element(glyph).toHaveAttribute('aria-hidden', 'true');
	});

	// `it.each` rather than a loop inside one test: `render` mounts into a shared container, so
	// rendering repeatedly in a single test leaves several copies in the DOM and every locator matches
	// more than one element — which surfaces as a confusing timeout rather than a duplicate-match
	// error. A test per state also names the failing state.
	it.each(TIERS)('renders the %s tier', async (tier) => {
		const screen = await render(TierBadge, { tier });
		await expect.element(screen.getByTestId('tier-badge')).toHaveAttribute('data-tier', tier);
	});

	it.each([
		['archive', 'true'],
		['restoring', 'true'],
		['hot', 'false'],
		['cool', 'false'],
		['restored', 'false']
	] as const)(
		'exposes needs-restore=%s for %s so the grid can label the download action',
		async (tier, expected) => {
			const screen = await render(TierBadge, { tier });
			await expect
				.element(screen.getByTestId('tier-badge'))
				.toHaveAttribute('data-needs-restore', expected);
		}
	);
});

describe('RightsBadge', () => {
	it('announces the rights state in words', async () => {
		const screen = await render(RightsBadge, { state: 'denied' });
		await expect.element(screen.getByText('Not licensed')).toBeInTheDocument();
	});

	it.each(RIGHTS_STATES)('renders the %s state', async (state) => {
		const screen = await render(RightsBadge, { state });
		await expect.element(screen.getByTestId('rights-badge')).toHaveAttribute('data-rights', state);
	});

	// The grid dims the download action off this attribute. Unknown blocks — the schema's AI gate says
	// unevaluated rights are not permission.
	it.each([
		['denied', 'true'],
		['unknown', 'true'],
		['allowed', 'false'],
		['expiring', 'false']
	] as const)('marks %s as blocks-distribution=%s', async (state, expected) => {
		const screen = await render(RightsBadge, { state });
		await expect
			.element(screen.getByTestId('rights-badge'))
			.toHaveAttribute('data-blocks-distribution', expected);
	});
});

describe('ProvenanceBadge', () => {
	it.each(PROVENANCE_STATES)('renders the %s state with a label', async (state) => {
		const screen = await render(ProvenanceBadge, { state });
		await expect
			.element(screen.getByTestId('provenance-badge'))
			.toHaveAttribute('data-provenance', state);
	});
});

describe('ConfidenceBar', () => {
	it('exposes meter semantics with the value and its bounds', async () => {
		// A div with a width is invisible to a screen reader. `role="meter"` plus the value attributes
		// is what turns the bar into something announceable.
		const screen = await render(ConfidenceBar, { value: 0.42 });
		const meter = screen.getByRole('meter');
		await expect.element(meter).toHaveAttribute('aria-valuenow', '42');
		await expect.element(meter).toHaveAttribute('aria-valuemin', '0');
		await expect.element(meter).toHaveAttribute('aria-valuemax', '100');
	});

	it('carries the value as text as well as length', async () => {
		const screen = await render(ConfidenceBar, { value: 0.91 });
		await expect.element(screen.getByText('91%')).toBeInTheDocument();
	});

	it('says a missing score is missing rather than drawing an empty bar', async () => {
		// An empty bar would claim the model was certain the tag was wrong. A null score usually means
		// a human applied the tag and nothing scored it.
		const screen = await render(ConfidenceBar, { value: null });
		await expect.element(screen.getByText(/not scored/i)).toBeInTheDocument();
		expect(screen.container.querySelector('[role="meter"]')).toBeNull();
	});

	it('clamps a value outside the range instead of overflowing', async () => {
		const screen = await render(ConfidenceBar, { value: 1.8 });
		await expect.element(screen.getByRole('meter')).toHaveAttribute('aria-valuenow', '100');
	});
});
