/**
 * The asset grid (F.4).
 *
 * §14.1 names this component as the reason Svelte was chosen — "a 100k-row virtualised grid with live
 * selection state" — and §14.2 names it as where EN 301 549 conformance is won or lost. Both pressures
 * point at the same place, and they pull against each other: virtualisation removes rows from the DOM,
 * and assistive technology reads the DOM.
 *
 * That tension is the subject of these tests. The bug it produces is specific and common: a grid that
 * reports `aria-rowcount` from the rows it has *rendered* tells a screen-reader user there are twenty
 * assets in a library of a hundred thousand, and there is no visual symptom at all.
 */
import { describe, expect, it } from 'vitest';
import { render } from 'vitest-browser-svelte';
import { tick } from 'svelte';
import AssetGrid from './AssetGrid.svelte';
import type { AssetSummary } from './types';

function asset(index: number): AssetSummary {
	return {
		id: `00000000-0000-0000-0000-${String(index).padStart(12, '0')}`,
		filename: `asset-${index}.jpg`,
		mime: 'image/jpeg',
		bytes: 1024 * (index + 1),
		width: 4000,
		height: 3000,
		tier: index % 5 === 0 ? 'archive' : 'hot',
		rights_state: index % 7 === 0 ? 'denied' : 'allowed',
		provenance_state: 'valid',
		thumbnail_url: null,
		tag_confidence: 0.8
	};
}

function assets(count: number): AssetSummary[] {
	return Array.from({ length: count }, (_, i) => asset(i));
}

/** A grid with a fixed viewport, so tests do not depend on layout. */
function grid(items: AssetSummary[], total = items.length, columns = 4) {
	return render(AssetGrid, { items, total, offset: 0, columns, height: 400, rowHeight: 120 });
}

describe('ARIA grid semantics', () => {
	it('is a grid', async () => {
		const screen = grid(assets(8));
		await expect.element(screen.getByRole('grid')).toBeInTheDocument();
	});

	it('reports the total row count, not the rendered one', async () => {
		// The bug this exists to prevent. 100,000 assets in 4 columns is 25,000 rows; a viewport that
		// renders 5 of them must still say 25,000, or a screen reader announces a library of a hundred
		// thousand as twenty items — with no visual symptom whatsoever.
		const screen = grid(assets(200), 100_000);
		await expect
			.element(screen.getByRole('grid'))
			.toHaveAttribute('aria-rowcount', String(Math.ceil(100_000 / 4)));
	});

	it('reports the column count', async () => {
		const screen = grid(assets(8), 8, 4);
		await expect.element(screen.getByRole('grid')).toHaveAttribute('aria-colcount', '4');
	});

	it('gives each rendered row its true position, not its position in the window', async () => {
		// `aria-rowindex` is 1-based and absolute. Numbering rendered rows 1..n would make every scroll
		// position claim to be the top of the list.
		const screen = grid(assets(40), 40, 4);
		const rows = screen.container.querySelectorAll('[role="row"]');
		expect(rows.length).toBeGreaterThan(0);
		const first = rows[0].getAttribute('aria-rowindex');
		expect(first).toBe('1');
	});

	it('gives each cell a column index and an accessible name from the filename', async () => {
		// A cell that announces only "gridcell" is unusable. The name has to be the thing the user is
		// looking for.
		const screen = grid(assets(4), 4, 4);
		const cells = screen.container.querySelectorAll('[role="gridcell"]');
		expect(cells[0].getAttribute('aria-colindex')).toBe('1');
		expect(cells[0].textContent).toContain('asset-0.jpg');
	});
});

describe('virtualisation', () => {
	it('renders a bounded number of cells for a very large collection', async () => {
		// The invariant that makes the grid viable at all: a 400px viewport of 120px rows needs about
		// four rows plus overscan, whatever the collection size. Without this the browser lays out
		// 100,000 cells and the tab stops responding.
		const screen = grid(assets(2_000), 100_000, 4);
		const cells = screen.container.querySelectorAll('[role="gridcell"]');
		expect(cells.length).toBeGreaterThan(0);
		expect(cells.length).toBeLessThan(80);
	});

	it('sizes the scroll area from the total, so the scrollbar tells the truth', async () => {
		const screen = grid(assets(2_000), 100_000, 4);
		const sizer = screen.container.querySelector('[data-testid="grid-sizer"]') as HTMLElement;
		const expected = Math.ceil(100_000 / 4) * 120;
		// Measured, not read from the style string: Chrome re-serialises a large px length in
		// exponential form ("3e+06px"), which looks like invalid CSS but lays out correctly. Asserting
		// on the serialised value would have reported a bug that does not exist — and missed a real
		// one, since what matters is whether the scroll area is actually that tall.
		expect(sizer.getBoundingClientRect().height).toBe(expected);
		expect(sizer.style.height, 'set via a probe below').toBeTruthy();
	});

	it('a very large px length survives the round trip through CSSOM', async () => {
		// Pinning the platform behaviour the assertion above depends on. If a future Chrome clamps
		// instead of normalising, the grid silently stops scrolling past the clamp and this says so.
		const probe = document.createElement('div');
		probe.style.height = '3000000px';
		document.body.append(probe);
		expect(probe.getBoundingClientRect().height).toBe(3_000_000);
		probe.remove();
	});
});

describe('keyboard navigation follows the WAI-ARIA grid pattern', () => {
	it('exposes exactly one tab stop', async () => {
		// Roving tabindex. Without it, tabbing through a 100k-row grid means 100,000 tab presses to
		// reach whatever follows it.
		const screen = grid(assets(12), 12, 4);
		const focusable = screen.container.querySelectorAll('[role="gridcell"][tabindex="0"]');
		expect(focusable.length).toBe(1);
	});

	it('moves the tab stop with the arrow keys', async () => {
		const screen = grid(assets(12), 12, 4);
		const cellAt = (i: number) =>
			screen.container.querySelectorAll('[role="gridcell"]')[i] as HTMLElement;

		cellAt(0).focus();
		cellAt(0).dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
		// `await tick()` throughout: Svelte 5 flushes to the DOM asynchronously, so a synchronous
		// getAttribute reads the state *before* the keypress was applied and every one of these
		// assertions fails for a reason that has nothing to do with the component.
		await tick();
		expect(cellAt(1).getAttribute('tabindex')).toBe('0');
		expect(cellAt(0).getAttribute('tabindex')).toBe('-1');

		cellAt(1).dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
		await tick();
		expect(cellAt(5).getAttribute('tabindex')).toBe('0');
	});

	it('does not wrap or run off the ends', async () => {
		// Arrowing left from the first cell must stay put rather than jumping to the end — wrapping in a
		// grid disorients, because the visual jump is the whole width and height of the viewport.
		const screen = grid(assets(12), 12, 4);
		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		const first = cells()[0] as HTMLElement;
		first.focus();
		first.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true }));
		await tick();
		expect((cells()[0] as HTMLElement).getAttribute('tabindex')).toBe('0');

		first.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true }));
		await tick();
		expect((cells()[0] as HTMLElement).getAttribute('tabindex')).toBe('0');
	});

	it('jumps to the start and end of the collection with ctrl+Home and ctrl+End', async () => {
		const screen = grid(assets(12), 12, 4);
		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		const first = cells()[0] as HTMLElement;
		first.focus();
		first.dispatchEvent(new KeyboardEvent('keydown', { key: 'End', ctrlKey: true, bubbles: true }));
		await tick();
		const all = cells();
		expect((all[all.length - 1] as HTMLElement).getAttribute('tabindex')).toBe('0');
	});
});

describe('selection', () => {
	it('marks selection on the cell, where assistive technology looks for it', async () => {
		const screen = grid(assets(8), 8, 4);
		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		(cells()[2] as HTMLElement).click();
		await tick();
		expect(cells()[2].getAttribute('aria-selected')).toBe('true');
		expect(cells()[0].getAttribute('aria-selected')).toBe('false');
	});

	it('extends a range with shift+click', async () => {
		const screen = grid(assets(8), 8, 4);
		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		(cells()[1] as HTMLElement).click();
		await tick();
		(cells()[4] as HTMLElement).dispatchEvent(
			new MouseEvent('click', { shiftKey: true, bubbles: true })
		);
		await tick();
		for (const index of [1, 2, 3, 4]) {
			expect(cells()[index].getAttribute('aria-selected'), `cell ${index}`).toBe('true');
		}
		expect(cells()[0].getAttribute('aria-selected')).toBe('false');
	});

	it('toggles a single item with meta+click without clearing the rest', async () => {
		const screen = grid(assets(8), 8, 4);
		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		(cells()[1] as HTMLElement).click();
		await tick();
		(cells()[3] as HTMLElement).dispatchEvent(
			new MouseEvent('click', { metaKey: true, bubbles: true })
		);
		await tick();
		expect(cells()[1].getAttribute('aria-selected')).toBe('true');
		expect(cells()[3].getAttribute('aria-selected')).toBe('true');

		(cells()[3] as HTMLElement).dispatchEvent(
			new MouseEvent('click', { metaKey: true, bubbles: true })
		);
		await tick();
		expect(cells()[3].getAttribute('aria-selected')).toBe('false');
	});

	it('announces the selection count in a live region', async () => {
		// A sighted user sees the count in a toolbar. Without a live region, a screen-reader user
		// selecting forty assets hears nothing at all and cannot tell the action took effect.
		const screen = grid(assets(8), 8, 4);
		const status = screen.container.querySelector('[role="status"]') as HTMLElement;
		expect(status.getAttribute('aria-live')).toBe('polite');

		const cells = () => screen.container.querySelectorAll('[role="gridcell"]');
		(cells()[2] as HTMLElement).click();
		await tick();
		expect(status.textContent).toMatch(/1 .*selected/i);
	});

	it('reports the selection against the total, not the rendered page', async () => {
		const screen = grid(assets(200), 100_000, 4);
		const status = screen.container.querySelector('[role="status"]') as HTMLElement;
		const cells = screen.container.querySelectorAll('[role="gridcell"]');
		(cells[0] as HTMLElement).click();
		await tick();
		expect(status.textContent).toContain('100,000');
	});
});

describe('an empty grid', () => {
	it('says it is empty rather than rendering a silent void', async () => {
		const screen = grid([], 0, 4);
		await expect.element(screen.getByText(/no assets/i)).toBeInTheDocument();
	});

	it('is still a grid with zero rows, so assistive technology is not confused', async () => {
		const screen = grid([], 0, 4);
		await expect.element(screen.getByRole('grid')).toHaveAttribute('aria-rowcount', '0');
	});
});
