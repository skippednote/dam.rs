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
	import FilterRail from '$lib/components/filter/FilterRail.svelte';
	import DetailPanel from '$lib/components/detail/DetailPanel.svelte';
	import UploadQueue from '$lib/components/upload/UploadQueue.svelte';
	import {
		ApiError,
		deliveryUrl,
		getAsset,
		listAssets,
		loadFacets,
		loadFields,
		searchAssets,
		type AssetDetail,
		type AssetPage,
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
			ranked = trimmed.length > 0;
			const [assets, counted, defined] = await Promise.all([
				ranked ? searchAssets({ q: trimmed, limit: PAGE }) : listAssets({ limit: PAGE, order }),
				// Facets share the query, so the counts describe the same set as the results. A failure here
				// must not empty the grid: a rail is an affordance and the results are the page.
				loadFacets(trimmed).catch(() => [] as Facet[]),
				// Fetched rather than inferred from the facet keys. The editor needs each field's *shape* —
				// `multivalued` decides whether a value is a string or an array, and guessing sends a
				// comma-joined string to a field that takes an array.
				loadFields().catch(() => [] as FieldDefinition[])
			]);
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
	<div class="flex flex-wrap items-center gap-3 border-b border-state-neutral/40 px-4 py-3">
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
				class="min-w-0 flex-1 rounded-md border border-state-neutral bg-bg px-3 py-1.5 text-sm"
				bind:value={query}
				placeholder="brand:acme &quot;spring campaign&quot; year:>2024"
			/>
			<button
				type="submit"
				class="rounded-md bg-accent px-3 py-1.5 text-sm font-medium text-white"
				disabled={loading}
			>
				{loading ? 'Searching…' : 'Search'}
			</button>
		</form>

		{#if !ranked}
			<label class="sr-only" for="order">Sort order</label>
			<select
				id="order"
				class="rounded-md border border-state-neutral bg-bg px-2 py-1.5 text-sm"
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
			class="rounded-md border border-state-neutral px-3 py-1.5 text-sm"
			aria-expanded={showUpload}
			onclick={() => (showUpload = !showUpload)}
		>
			Upload
		</button>
	</div>

	{#if showUpload}
		<div class="border-b border-state-neutral/40 px-4 py-3">
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
			class="hidden w-56 shrink-0 overflow-y-auto border-r border-state-neutral/40 p-4 lg:block"
		>
			<FilterRail {facets} {query} onquery={railChanged} />
		</aside>

		<div class="min-w-0 flex-1 overflow-hidden p-4">
			{#if result}
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
					thumbnail={deliveryUrl}
					onselect={(asset) => open(asset.id)}
					onactivate={(asset) => open(asset.id)}
				/>
			{:else if !loading && !error}
				<p class="text-sm text-muted">Nothing here yet. Upload something, or check Settings.</p>
			{/if}
		</div>

		{#if selected}
			<aside aria-label="Selected asset" class="w-80 shrink-0 border-l border-state-neutral/40">
				<DetailPanel
					asset={selected}
					{fields}
					onchanged={(values) => {
						if (selected) selected = { ...selected, values };
					}}
					onclose={() => (selected = null)}
				/>
			</aside>
		{/if}
	</div>
</div>
