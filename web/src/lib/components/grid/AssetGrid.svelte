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
	import { Archive, Broadcast, ImageBroken, Lock, Star } from 'phosphor-svelte';

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
		onfavourite,
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
		 * A request to toggle the asset's favourite state — from the star, or from `f` on the focused cell.
		 *
		 * Reported rather than performed: the grid is also the Drupal picker, and a component that called the
		 * engagement API would carry a dependency the picker has no use for. The page owns the write, and the
		 * page is what announces the outcome.
		 */
		onfavourite?: (asset: AssetSummary) => void;
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
			case 'f':
			case 'F':
				// Handled here rather than on the star itself, because the WAI-ARIA grid pattern puts key
				// handling on the container and the grid keeps exactly one tab stop. Without this the star
				// would be mouse-only — the same keyboard/mouse asymmetry the uploader's drop target had.
				//
				// Modifier-free only: ctrl+F and cmd+F are the browser's find, and stealing those would break a
				// far more important key than this one.
				if (event.ctrlKey || event.metaKey || event.altKey) return;
				event.preventDefault();
				if (items[focused]) onfavourite?.(items[focused]);
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

	/**
	 * What the live region announces.
	 *
	 * Pluralised on the *total*, because a one-asset library announced itself as "1 assets" — a string only a
	 * screen-reader user ever hears, which is exactly why nobody had noticed it. The selected count needs no
	 * such care: it is always followed by "of N", so it never sits against a noun.
	 */
	const selectionMessage = $derived(
		selected.size === 0
			? `${total.toLocaleString()} ${total === 1 ? 'asset' : 'assets'}`
			: `${selected.size.toLocaleString()} of ${total.toLocaleString()} ${
					total === 1 ? 'asset' : 'assets'
				} selected`
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
		class="overflow-auto"
	>
		<!-- Sized from the total, so the scrollbar reflects the collection rather than the window. -->
		<div data-testid="grid-sizer" style="height: {totalRows * rowHeight}px; position: relative;">
			{#each virtualRows as virtualRow (virtualRow.index)}
				<div
					role="row"
					aria-rowindex={virtualRow.index + 1}
					style="position: absolute; top: 0; left: 0; width: 100%; height: {rowHeight}px; transform: translateY({virtualRow.start}px);"
					class="flex gap-3 py-1"
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
								class="group flex min-w-0 flex-1 flex-col overflow-hidden rounded-md border border-line
								       bg-surface text-left text-xs transition-colors hover:border-state-neutral
								       aria-selected:border-accent aria-selected:ring-2 aria-selected:ring-accent"
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
										class="image-well min-h-0 w-full flex-1 object-cover"
									/>
								{:else}
									<!--
										A placeholder rather than an empty box, and it says *why*: between an upload
										finishing and the worker deriving it there is no thumbnail, and "processing" is
										the honest label for that. An empty cell reads as a broken image.
									-->
									<div
										class="image-well flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-[10px]
										       text-muted"
										data-testid="thumbnail-placeholder"
									>
										{#if asset.tier === 'archive'}
											<Archive size={22} aria-hidden="true" />
											<span>Original archived · preview pending</span>
										{:else}
											<ImageBroken size={22} aria-hidden="true" />
											<span>Preview processing</span>
										{/if}
									</div>
								{/if}
								<div class="space-y-1.5 px-2.5 py-2">
									<span class="flex min-w-0 items-center gap-1">
										<span data-testid="cell-filename" class="min-w-0 flex-1 truncate font-medium">
											{asset.filename}
										</span>
										<!--
										`tabindex="-1"`, so the grid keeps exactly one tab stop — the WAI-ARIA grid pattern
										allows a widget inside a cell and reaches it through the container's key handling,
										which is what `f` above is. It stays in the accessibility tree, so a screen reader
										browsing the cell finds and can operate it; it is simply not in the tab order.

										`stopPropagation`, because a click here is not a click on the cell: without it,
										favouriting would also change the selection and open the detail panel.
									-->
										<button
											type="button"
											tabindex="-1"
											aria-pressed={asset.is_favourite}
											aria-label={asset.is_favourite
												? `Remove ${asset.filename} from favourites`
												: `Add ${asset.filename} to favourites`}
											onclick={(event) => {
												event.stopPropagation();
												onfavourite?.(asset);
											}}
											class="shrink-0 rounded px-1 text-sm leading-none
										       {asset.is_favourite ? 'text-accent' : 'text-muted hover:text-fg'}"
										>
											<Star
												size={15}
												weight={asset.is_favourite ? 'fill' : 'regular'}
												aria-hidden="true"
											/>
										</button>
									</span>
									<span class="flex flex-wrap items-center gap-1">
										<TierBadge tier={asset.tier} />
										<RightsBadge state={asset.rights_state} />
										{#if asset.published_at}
											<!--
											Published (Q.14): this asset can appear on a page anybody can reach. Form
											rather than colour, like the tier badge — the colour channel belongs to
											rights, the only dimension with legal consequence — and a title carrying the
											instant, because "since when" is the question a public appearance raises.
										-->
											<span
												data-testid="published-badge"
												title={`Published ${new Date(asset.published_at).toLocaleString()}`}
												class="inline-flex items-center gap-1 rounded-md border border-state-neutral bg-surface px-2 py-0.5 text-xs text-fg"
											>
												<Broadcast size={12} aria-hidden="true" />
												Public
											</span>
										{/if}
										{#if asset.legal_hold}
											<!--
											Legal hold: this asset cannot be deleted and tiering will leave it alone.

											Here as well as on the detail panel because the bulk bar is two rows below,
											and somebody assembling a selection to delete should see which of it is
											frozen while they are choosing — not in the operation's result. Form rather
											than colour, like the published badge, since the colour channel belongs to
											rights.
										-->
											<span
												data-testid="hold-badge"
												title="Under legal hold — cannot be deleted"
												class="inline-flex items-center gap-1 rounded-md border border-state-neutral bg-surface px-2 py-0.5 text-xs text-fg"
											>
												<Lock size={12} aria-hidden="true" />
												Held
											</span>
										{/if}
										{#if asset.average_stars !== null && asset.average_stars !== undefined}
											<!--
											The library's average, as a number rather than five glyphs: a cell has no room for
											a star widget, and "4.2" is both smaller and more precise than four-and-a-bit
											stars drawn at this size.
										-->
											<span
												class="rounded bg-raised px-1 text-[10px] text-muted tabular-nums"
												title="Average rating"
											>
												<Star size={10} weight="fill" aria-hidden="true" />
												{asset.average_stars.toFixed(1)}
											</span>
										{/if}
									</span>
								</div>
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
