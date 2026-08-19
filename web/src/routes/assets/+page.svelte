<script lang="ts">
	/**
	 * The asset browser: search, facet rail, grid, detail panel, upload.
	 *
	 * ## One query string, and the URL owns it
	 *
	 * The box, the rail and the address bar hold the same value. That is what makes a search shareable and
	 * the back button work — and it removes the class of bug where a rail's selection and a box's text
	 * disagree about what is being shown.
	 *
	 * ## Search and list are different endpoints, and the difference is visible
	 *
	 * An empty query pages the library through `/assets`, which is ordered and exhaustive. A non-empty one
	 * goes to `/search`, which is ranked and bounded — relevance past a few hundred results is not
	 * meaningfully ordered, so the depth is capped server-side. Saying so beside the count is better than
	 * letting a user scroll into an invisible wall.
	 */
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import AssetGrid from '$lib/components/grid/AssetGrid.svelte';
	import CategoryTree from '$lib/components/filter/CategoryTree.svelte';
	import FilterRail from '$lib/components/filter/FilterRail.svelte';
	import DetailPanel from '$lib/components/detail/DetailPanel.svelte';
	import Lightbox from '$lib/components/detail/Lightbox.svelte';
	import UploadQueue from '$lib/components/upload/UploadQueue.svelte';
	import BulkBar from '$lib/components/bulk/BulkBar.svelte';
	import {
		ApiError,
		deliveryUrl,
		getAsset,
		listAssets,
		loadFacets,
		loadFields,
		searchAssets,
		setFavourite,
		type AssetDetail,
		type AssetPage,
		type AssetSummary,
		type Engagement,
		type Facet,
		type FieldDefinition,
		type SortOrder
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	/** Rows per request. The grid draws a window; this is comfortably more than one screen. */
	const PAGE = 60;

	let query = $state(page.url.searchParams.get('q') ?? '');
	let order = $state<SortOrder>('newest');
	let result = $state<AssetPage | null>(null);
	let facets = $state<Facet[]>([]);
	let fields = $state<FieldDefinition[]>([]);
	let selected = $state<AssetDetail | null>(null);
	let loading = $state(false);
	let error = $state('');
	let ranked = $state(false);
	let showUpload = $state(false);
	/** Whether the selected asset is open full-screen. Separate from `selected`, because the panel and the
	    lightbox show the same asset and closing one must not close the other. */
	let lightbox = $state(false);
	/** The grid's selection, as it reports it. Lives in the grid; this is the read model. */
	let selection = $state<string[]>([]);
	let grid = $state<{ clearSelection: () => void } | null>(null);
	/**
	 * What the last favourite toggle did, announced.
	 *
	 * The grid's star is not a tab stop, so a keyboard user reaches it with `f` while focus stays on the cell —
	 * which means nothing they are focused on changes state visibly to a screen reader. Without an announcement
	 * the key would be silent, and a silent toggle is indistinguishable from one that did nothing.
	 */
	let engagementNotice = $state('');

	/** Discards a stale response, so a fast second search cannot be overwritten by a slow first one. */
	let generation = 0;

	async function load() {
		if (!session.connected) {
			error = 'Not connected. Add an API key in Settings.';
			return;
		}
		const mine = ++generation;
		loading = true;
		error = '';
		try {
			const trimmed = query.trim();
			const searching = trimmed.length > 0;
			const [assets, counted, defined] = await Promise.all([
				searching ? searchAssets({ q: trimmed, limit: PAGE }) : listAssets({ limit: PAGE, order }),
				// Facets share the query, so the counts describe the same set as the results. A failure here
				// must not empty the grid: a rail is an affordance and the results are the page.
				loadFacets(trimmed).catch(() => [] as Facet[]),
				// Fetched rather than inferred from the facet keys. The editor needs each field's *shape* —
				// `multivalued` decides whether a value is a string or an array, and guessing sends a
				// comma-joined string to a field that takes an array.
				loadFields().catch(() => [] as FieldDefinition[])
			]);
			// Whether the results are relevance-ordered is the server's answer, not a guess from "did we search".
			// A category filter routes through SQL — the index cannot answer a membership join — and comes back
			// in recency order, so inferring `ranked` from the presence of a query would put "ranked by
			// relevance" over a list that is not.
			// `?? searching` because the field is optional on the wire: a server predating it omits it, and the
			// old inference — "we searched, so it is ranked" — is the best answer available in that case.
			ranked = assets.ranked ?? searching;
			if (mine !== generation) return;
			result = assets;
			facets = counted;
			if (defined.length > 0) fields = defined;
		} catch (caught) {
			if (mine !== generation) return;
			error = caught instanceof Error ? caught.message : 'Could not load assets.';
			if (caught instanceof ApiError && caught.query) {
				// The parser's own column, so the message points at the word rather than at the query.
				error = caught.query.at
					? `${caught.query.message} (at character ${caught.query.at})`
					: caught.query.message;
			}
		} finally {
			if (mine === generation) loading = false;
		}
	}

	async function open(id: string) {
		try {
			selected = await getAsset(id);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not open that asset.';
		}
	}

	/**
	 * Toggles a favourite from the grid, and updates the one row it changed.
	 *
	 * The row is patched in place rather than reloading the page: a reload would lose the scroll position of a
	 * virtualised grid and re-run the search, both to redraw one star. The server's answer is what is written,
	 * so the star cannot disagree with what was stored.
	 */
	async function favourite(asset: AssetSummary) {
		const wanted = !asset.is_favourite;
		try {
			const after = await setFavourite(asset.id, wanted);
			patchRow(asset.id, after);
			engagementNotice = after.is_favourite
				? `${asset.filename} added to your favourites.`
				: `${asset.filename} removed from your favourites.`;
		} catch (caught) {
			// Into the same region a success uses: a failure that only appeared in the page-level error banner
			// would be announced somewhere the user's focus is not.
			engagementNotice =
				caught instanceof Error
					? `Could not change ${asset.filename}: ${caught.message}`
					: `Could not change ${asset.filename}.`;
		}
	}

	/** Writes one asset's engagement into the loaded page and, if it is open, into the detail panel. */
	function patchRow(id: string, after: Engagement) {
		if (result) {
			result = {
				...result,
				items: result.items.map((item) =>
					item.id === id
						? { ...item, is_favourite: after.is_favourite, average_stars: after.average_stars }
						: item
				)
			};
		}
		// The panel too, when it is showing the same asset — otherwise the grid's star and the panel's toggle
		// would disagree until something else reloaded one of them.
		if (selected?.id === id) {
			selected = {
				...selected,
				engagement: after,
				is_favourite: after.is_favourite,
				average_stars: after.average_stars
			};
		}
	}

	/** Where the open asset sits in the loaded page, so the lightbox knows whether it can step. */
	const position = $derived(
		selected ? (result?.items.findIndex((item) => item.id === selected?.id) ?? -1) : -1
	);

	/**
	 * Steps the lightbox by `delta` within the loaded page.
	 *
	 * Bounded by what is loaded rather than by the whole result set: stepping past the last fetched row would
	 * need a fetch mid-gesture, and an arrow key that sometimes pauses for a network round trip feels broken.
	 * The controls are hidden at the edges instead, so the affordance matches what it can do.
	 */
	function step(delta: number) {
		const next = result?.items[position + delta];
		if (next) void open(next.id);
	}

	function submit(event: SubmitEvent) {
		event.preventDefault();
		show(query);
		void load();
	}

	function railChanged(next: string) {
		query = next;
		show(next);
		void load();
	}

	/**
	 * Puts the current query in the address bar.
	 *
	 * A `URL` built from the current one rather than a fresh query string: it preserves anything else that
	 * happens to be there, and it is what `resolve` cannot express — the route is fixed and only the search
	 * changes. `replaceState` keeps a search out of the history for every keystroke while still making the
	 * current one shareable, which is the whole reason the query lives in the URL at all.
	 */
	function show(next: string) {
		const url = new URL(page.url);
		const trimmed = next.trim();
		if (trimmed) url.searchParams.set('q', trimmed);
		else url.searchParams.delete('q');
		// `resolve` cannot express this: the route is fixed and only the search string changes, and the URL is
		// derived from `page.url`, which already carries whatever base path the app is served under. The rule
		// exists to catch a hand-written internal path that skips the base — not this.
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		void goto(url, { replaceState: true, keepFocus: true, noScroll: true });
	}

	onMount(load);
</script>

<div class="flex h-[calc(100vh-3rem)] flex-col">
	<div class="flex flex-wrap items-center gap-3 border-b border-line px-4 py-3">
		<!--
			A real `h1`, not a visually hidden one. A screen-reader user navigating by heading needs to land
			somewhere on this page, and an app screen with no `h1` is one that cannot be entered by heading at
			all — the affordance most such users reach for first.
		-->
		<h1 class="text-sm font-semibold tracking-tight">Assets</h1>
		<form class="flex flex-1 items-center gap-2" onsubmit={submit} role="search">
			<label class="sr-only" for="q">Search assets</label>
			<input
				id="q"
				class="min-w-0 flex-1 rounded-md border border-line bg-bg px-3 py-1.5 text-sm"
				bind:value={query}
				placeholder="brand:acme &quot;spring campaign&quot; year:>2024"
			/>
			<button
				type="submit"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-accent-fg"
				disabled={loading}
			>
				{loading ? 'Searching…' : 'Search'}
			</button>
		</form>

		{#if !ranked}
			<label class="sr-only" for="order">Sort order</label>
			<select
				id="order"
				class="rounded-md border border-line bg-bg px-2 py-1.5 text-sm"
				bind:value={order}
				onchange={load}
			>
				<option value="newest">Newest first</option>
				<option value="oldest">Oldest first</option>
				<option value="filename_asc">Filename A–Z</option>
				<option value="filename_desc">Filename Z–A</option>
				<option value="largest_first">Largest first</option>
			</select>
		{/if}

		<button
			type="button"
			class="rounded-md border border-line px-3 py-1.5 text-sm"
			aria-expanded={showUpload}
			onclick={() => (showUpload = !showUpload)}
		>
			Upload
		</button>
	</div>

	{#if showUpload}
		<div class="border-b border-line px-4 py-3">
			<UploadQueue onfinished={load} />
			<p class="mt-2 text-xs text-muted">
				An upload lands in staging and is finalised into an asset by the worker. Until the worker
				runs it will not appear in the grid.
			</p>
		</div>
	{/if}

	{#if error}
		<p
			role="alert"
			class="m-4 rounded-md bg-state-rights-denied/12 p-3 text-sm text-state-rights-denied-fg"
		>
			{error}
			{#if !session.connected}
				<a class="underline" href={resolve('/settings')}>Open Settings</a>
			{/if}
		</p>
	{/if}

	<div class="flex min-h-0 flex-1">
		<!--
			`aria-label` because there are two navigational regions on this page and a landmark with no name
			is announced as "complementary" with no clue which one.
		-->
		<aside
			aria-label="Filters"
			class="hidden w-56 shrink-0 overflow-y-auto border-r border-line p-4 lg:block"
		>
			<!-- The tree first: where an asset *lives* is the coarser question, and the facets narrow within
			     whatever it selects. Both write the same query string. -->
			<div class="space-y-6">
				<CategoryTree {query} onquery={railChanged} />
				<FilterRail {facets} {query} onquery={railChanged} />
			</div>
		</aside>

		<div class="min-w-0 flex-1 overflow-hidden p-4">
			{#if result}
				<!--
					The star's own announcement, separate from the result count above: they change for unrelated
					reasons, and one region holding both would re-announce the count every time somebody
					favourited something.
				-->
				<p role="status" aria-live="polite" class="sr-only">{engagementNotice}</p>
				<p class="mb-2 text-xs text-muted" aria-live="polite">
					{result.total}
					{result.total === 1 ? 'asset' : 'assets'}
					{#if ranked}
						· ranked by relevance, capped at the first 1,000
					{/if}
				</p>
				<!--
					The row height is mostly thumbnail. `thumb-256` is a 256px square (Cover fit), so a cell is
					sized to show one near its natural size rather than squeezing it into a strip — the first
					version gave it 32 pixels of a 96-pixel cell, which is a grid of captions with a smear on
					top.
				-->
				<AssetGrid
					items={result.items}
					total={result.total}
					offset={result.offset}
					columns={4}
					height={640}
					rowHeight={224}
					bind:this={grid}
					thumbnail={deliveryUrl}
					onselect={(asset, ids) => {
						selection = ids;
						void open(asset.id);
					}}
					onactivate={(asset) => {
						void open(asset.id).then(() => (lightbox = true));
					}}
					onfavourite={favourite}
				/>
				<BulkBar
					assetIds={selection}
					{fields}
					onfinished={load}
					onclear={() => {
						grid?.clearSelection();
						selection = [];
					}}
				/>
			{:else if !loading && !error}
				<p class="text-sm text-muted">Nothing here yet. Upload something, or check Settings.</p>
			{/if}
		</div>

		{#if selected && lightbox}
			<Lightbox
				asset={selected}
				hasPrevious={position > 0}
				hasNext={position >= 0 && position < (result?.items.length ?? 0) - 1}
				onclose={() => (lightbox = false)}
				onprevious={() => step(-1)}
				onnext={() => step(1)}
			/>
		{/if}

		{#if selected}
			<aside aria-label="Selected asset" class="w-80 shrink-0 border-l border-line">
				<DetailPanel
					asset={selected}
					{fields}
					onchanged={(values) => {
						if (selected) selected = { ...selected, values };
					}}
					onengagement={(after) => patchRow(after.asset_id, after)}
					onclose={() => (selected = null)}
				/>
			</aside>
		{/if}
	</div>
</div>
