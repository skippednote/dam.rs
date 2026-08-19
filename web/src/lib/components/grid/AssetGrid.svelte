<!--
	The asset grid: virtualised, keyboard-navigable, and an ARIA grid.

	Those three pull against each other, which is the whole difficulty. Virtualisation removes rows from
	the DOM; assistive technology reads the DOM. The reconciliation is that the *container* carries the
	truth about the collection (`aria-rowcount`, `aria-colcount`) while each rendered row carries its
	absolute position (`aria-rowindex`). A grid that numbered its rendered rows 1..n would claim every
	scroll position was the top of the list, and a grid that reported its rendered count as the row count
	would announce a hundred thousand assets as twenty — with no visual symptom at all.

	Keyboard behaviour follows the WAI-ARIA grid pattern: one tab stop (roving tabindex), arrows to move,
	Home/End within a row, ctrl+Home/End for the collection. Arrowing past an edge holds position rather
	than wrapping — in a grid, a wrap moves the eye the full width or height of the viewport, which
	disorients rather than helps.
-->
<script lang="ts">
	import { createVirtualizer } from '@tanstack/svelte-virtual';
	import { SvelteSet } from 'svelte/reactivity';
	import TierBadge from '$lib/components/state/TierBadge.svelte';
	import RightsBadge from '$lib/components/state/RightsBadge.svelte';
	import type { AssetSummary } from './types';

	let {
		items,
		/** Total matching assets, which is not `items.length` — see `aria-rowcount`. */
		total,
		/** Index of `items[0]` within the full result set. */
		offset = 0,
		columns = 4,
		height = 600,
		rowHeight = 160,
		onactivate,
		onselect,
		thumbnail: thumbnailProp
	}: {
		items: AssetSummary[];
		total: number;
		offset?: number;
		columns?: number;
		height?: number;
		rowHeight?: number;
		/** Enter, or a double click: the "open this" gesture. */
		onactivate?: (asset: AssetSummary) => void;
		/**
		 * A change of selection, with the asset that caused it and every id now selected.
		 *
		 * Reported because the selection lives in here — a `SvelteSet`, so mutation is cheap — and a detail
		 * panel or a bulk-operations bar outside the grid has no other way to read it. Without this the
		 * selection would have to be lifted into the parent, and every click would clone a set that a
		 * shift-range over 40,000 assets makes O(n).
		 */
		onselect?: (asset: AssetSummary, ids: string[]) => void;
		/**
		 * Resolves a thumbnail URL the server may have sent root-relative.
		 *
		 * Passed in rather than importing the session here, because this component is also the Drupal picker
		 * (§11.2) — the same Svelte app embedded in a Media Library — and that build resolves against a
		 * different base. A component that reached for a global would be a component that only works in one of
		 * them.
		 */
		thumbnail?: (url: string) => string;
	} = $props();

	const thumbnail = $derived(thumbnailProp ?? ((url: string) => url));

	let viewport = $state<HTMLDivElement | null>(null);
	/** Index within `items` of the cell holding the tab stop. */
	let focused = $state(0);
	// `SvelteSet`, not a plain Set in `$state`: mutation is reactive, so selecting does not clone the
	// whole set. Cloning is O(n) per click, and a shift-range selection over 40,000 assets does it on
	// every keystroke.
	const selected = new SvelteSet<string>();
	/** Where a shift+click range starts. */
	let anchor = $state(0);

	const totalRows = $derived(Math.ceil(total / columns));

	// The virtualiser is a Svelte store rather than a rune — @tanstack/svelte-virtual predates runes —
	// and it needs the scroll element, which does not exist until the div is bound. Creating it lazily
	// keeps `getScrollElement` from returning null on the first render.
	const virtualizer = $derived(
		viewport
			? createVirtualizer<HTMLDivElement, HTMLDivElement>({
					count: totalRows,
					getScrollElement: () => viewport,
					estimateSize: () => rowHeight,
					overscan: 2
				})
			: null
	);

	const virtualRows = $derived($virtualizer ? $virtualizer.getVirtualItems() : []);

	function cellIndex(row: number, column: number): number {
		return row * columns + column - offset;
	}

	function move(next: number) {
		focused = Math.min(Math.max(next, 0), items.length - 1);
		const row = Math.floor((focused + offset) / columns);
		// Scrolling the focused cell into view is what makes keyboard navigation work at all in a
		// virtualised grid: the next cell may not be rendered yet.
		$virtualizer?.scrollToIndex(row);
		queueMicrotask(() => {
			const target = viewport?.querySelector<HTMLElement>(`[data-cell-index="${focused}"]`);
			target?.focus();
		});
	}

	function onkeydown(event: KeyboardEvent) {
		const column = (focused + offset) % columns;
		let next: number;

		switch (event.key) {
			case 'ArrowRight':
				next = column === columns - 1 ? focused : focused + 1;
				break;
			case 'ArrowLeft':
				next = column === 0 ? focused : focused - 1;
				break;
			case 'ArrowDown':
				next = focused + columns;
				break;
			case 'ArrowUp':
				next = focused - columns;
				break;
			case 'Home':
				next = event.ctrlKey || event.metaKey ? 0 : focused - column;
				break;
			case 'End':
				next =
					event.ctrlKey || event.metaKey
						? items.length - 1
						: Math.min(focused + (columns - 1 - column), items.length - 1);
				break;
			case ' ':
			case 'Enter':
				event.preventDefault();
				select(focused, { toggle: event.key === ' ', range: event.shiftKey });
				if (event.key === 'Enter') onactivate?.(items[focused]);
				return;
			default:
				return;
		}

		if (next >= 0 && next < items.length) {
			event.preventDefault();
			move(next);
		} else {
			// Held at an edge. Consumed anyway, so the browser does not scroll the page underneath.
			event.preventDefault();
		}
	}

	function select(index: number, modifiers: { toggle: boolean; range: boolean }) {
		const id = items[index]?.id;
		if (!id) return;

		if (modifiers.range) {
			const [from, to] = anchor <= index ? [anchor, index] : [index, anchor];
			for (let i = from; i <= to; i += 1) {
				const item = items[i];
				if (item) selected.add(item.id);
			}
		} else if (modifiers.toggle) {
			if (selected.has(id)) selected.delete(id);
			else selected.add(id);
			anchor = index;
		} else {
			selected.clear();
			selected.add(id);
			anchor = index;
		}
		focused = index;
		// After the mutation, so a listener sees the selection as it now is rather than as it was.
		onselect?.(items[index], [...selected]);
	}

	/**
	 * Clears the selection from outside.
	 *
	 * Exported because the selection *lives* here — a `SvelteSet`, so mutation stays O(1) per click — and the
	 * bulk bar needs to reset it after an operation without the page owning the set. Reported through
	 * `onselect` like any other change, so listeners cannot drift from the truth.
	 */
	export function clearSelection() {
		selected.clear();
		const current = items[focused];
		if (current) onselect?.(current, []);
	}

	function onclick(event: MouseEvent, index: number) {
		select(index, {
			toggle: event.metaKey || event.ctrlKey,
			range: event.shiftKey
		});
	}

	const selectionMessage = $derived(
		selected.size === 0
			? `${total.toLocaleString()} assets`
			: `${selected.size.toLocaleString()} of ${total.toLocaleString()} assets selected`
	);
</script>

<!--
	The live region sits outside the grid and is always present. Rendering it only when something is
	selected means the first announcement is missed: a region added to the DOM at the same moment its
	content appears is not reliably announced.
-->
<div role="status" aria-live="polite" class="sr-only">{selectionMessage}</div>

{#if total === 0}
	<!--
		Not a grid. An empty result is a sentence, and `role="grid"` over prose is wrong twice: it fails
		`aria-required-children` (a grid must contain rows), and a screen reader announces "grid, 0 rows" and
		then reads text that is in no cell. `role="status"` is what this actually is — the answer to a search.

		Found by an e2e fixture that returned zero assets: every other fixture has results, so this state had
		never been axe-scanned.
	-->
	<p role="status" class="rounded-md border border-line p-8 text-center text-muted">
		No assets match this search.
	</p>
{:else}
	<div
		bind:this={viewport}
		role="grid"
		aria-label="Assets"
		aria-rowcount={totalRows}
		aria-colcount={columns}
		aria-multiselectable="true"
		tabindex="-1"
		{onkeydown}
		style="height: {height}px"
		class="overflow-auto rounded-md border border-line"
	>
		<!-- Sized from the total, so the scrollbar reflects the collection rather than the window. -->
		<div data-testid="grid-sizer" style="height: {totalRows * rowHeight}px; position: relative;">
			{#each virtualRows as virtualRow (virtualRow.index)}
				<div
					role="row"
					aria-rowindex={virtualRow.index + 1}
					style="position: absolute; top: 0; left: 0; width: 100%; height: {rowHeight}px; transform: translateY({virtualRow.start}px);"
					class="flex gap-2 p-2"
				>
					{#each Array.from({ length: columns }, (_, column) => column) as column (column)}
						{@const index = cellIndex(virtualRow.index, column)}
						{#if index >= 0 && index < items.length}
							{@const asset = items[index]}
							<!--
								Keyboard parity is real but lives on the grid container: the WAI-ARIA grid pattern
								puts key handling there and delegates, so Enter and Space on the focused cell are
								handled by `onkeydown` above. A per-cell handler would double-fire.

								The directive below has to be its own comment with nothing but the code in it —
								`svelte-ignore` treats every following token as another rule name, so an inline
								explanation becomes thirty invented rule names and eslint rejects each one.
							-->
							<!-- svelte-ignore a11y_click_events_have_key_events -->
							<div
								role="gridcell"
								aria-colindex={column + 1}
								aria-selected={selected.has(asset.id)}
								data-cell-index={index}
								tabindex={focused === index ? 0 : -1}
								onclick={(event) => onclick(event, index)}
								class="flex min-w-0 flex-1 flex-col gap-1 rounded-md border border-line
								       bg-surface p-2 text-left text-xs
								       aria-selected:ring-2 aria-selected:ring-accent"
							>
								<!--
									`loading="lazy"` because a virtualised grid still mounts a row's worth of images at a
									time and a scroll through 40,000 assets should not fetch 40,000 thumbnails.

									`alt=""` and `aria-hidden`, deliberately: the filename beneath is already the cell's
									accessible name, and a screen reader announcing "harbour.jpg, image, harbour.jpg" is
									worse than one that says it once. A thumbnail is decoration *for this cell* — the
									asset's real alt text is a metadata field, and it belongs where the image is used for
									its content rather than as a picker affordance.
								-->
								{#if asset.thumbnail_url}
									<img
										src={thumbnail(asset.thumbnail_url)}
										alt=""
										aria-hidden="true"
										loading="lazy"
										decoding="async"
										class="image-well min-h-0 w-full flex-1 rounded object-cover"
									/>
								{:else}
									<!--
										A placeholder rather than an empty box, and it says *why*: between an upload
										finishing and the worker deriving it there is no thumbnail, and "processing" is
										the honest label for that. An empty cell reads as a broken image.
									-->
									<div
										class="image-well flex min-h-0 flex-1 items-center justify-center rounded text-[10px]
										       text-muted"
										data-testid="thumbnail-placeholder"
									>
										{asset.tier === 'archive' ? 'archived' : 'processing'}
									</div>
								{/if}
								<span class="truncate font-medium">{asset.filename}</span>
								<span class="flex flex-wrap gap-1">
									<TierBadge tier={asset.tier} />
									<RightsBadge state={asset.rights_state} />
								</span>
							</div>
						{:else}
							<!-- A trailing gap on the last row. Presentational, so it is not a gridcell: an
							     empty cell would be announced as one and inflate the count. -->
							<div class="min-w-0 flex-1" aria-hidden="true"></div>
						{/if}
					{/each}
				</div>
			{/each}
		</div>
	</div>
{/if}
