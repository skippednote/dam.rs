<!--
	One of the caller's private lists — favourites or watches — as a place you can go.

	## Shared, because the two differ only in copy and endpoint

	Two near-identical routes is how one of them acquires a fix the other does not. What genuinely differs is the
	sentence explaining what the list *is*, and that is a prop.

	## The order is the point

	The server returns these in the order the person added them, which is not an order any other endpoint can
	produce — `/search?q=is:favourite` gives upload recency instead. So this page never re-sorts, and it says which
	order it is showing, because a list whose order is meaningful and unexplained looks arbitrary.

	## Removing from here removes from the list, and the row goes

	On the assets grid a star toggles and the row stays. Here the row *is* its membership, so removing it takes the
	row away — and that is worth announcing, because a row vanishing under the cursor is otherwise indistinguishable
	from a page reloading.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import AssetGrid from './AssetGrid.svelte';
	import {
		ApiError,
		deliveryUrl,
		listFavourites,
		listWatches,
		setFavourite,
		setWatch,
		type AssetSummary
	} from '$lib/api/client';

	let {
		kind,
		title,
		explanation,
		empty
	}: {
		kind: 'favourites' | 'watches';
		title: string;
		/** What the list is, in one sentence. The only real difference between the two routes. */
		explanation: string;
		/** What to say when it is empty, which is where a person needs the most help. */
		empty: string;
	} = $props();

	let items = $state<AssetSummary[]>([]);
	let total = $state(0);
	let error = $state('');
	let loading = $state(true);
	/** What the last removal did, announced — see the component docs on the vanishing row. */
	let notice = $state('');

	async function load() {
		error = '';
		loading = true;
		try {
			const page = kind === 'favourites' ? await listFavourites() : await listWatches();
			// Never re-sorted: the server's order is the order the person built the list in, and that is the
			// only place that order exists.
			items = page.items;
			total = page.total;
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : `Could not load your ${kind}.`;
		} finally {
			loading = false;
		}
	}

	/**
	 * Removes one asset from this list.
	 *
	 * The row is dropped locally rather than reloading: a reload would refetch every row to remove one, and would
	 * lose the scroll position of a virtualised grid while doing it.
	 */
	async function remove(asset: AssetSummary) {
		try {
			if (kind === 'favourites') await setFavourite(asset.id, false);
			else await setWatch(asset.id, false);
			items = items.filter((item) => item.id !== asset.id);
			total = Math.max(0, total - 1);
			notice = `${asset.filename} removed from your ${kind}.`;
		} catch (caught) {
			notice =
				caught instanceof ApiError
					? `Could not remove ${asset.filename}: ${caught.message}`
					: `Could not remove ${asset.filename}.`;
		}
	}

	onMount(load);
</script>

<div class="space-y-4 p-8">
	<div>
		<h1 class="text-2xl font-semibold tracking-tight">{title}</h1>
		<p class="mt-1 max-w-2xl text-sm text-muted">{explanation}</p>
	</div>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-3 text-sm text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}

	<!-- Its own region, so removing a row does not re-announce the count and vice versa. -->
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>

	{#if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if items.length === 0}
		<p class="text-sm text-muted">{empty}</p>
	{:else}
		<p class="text-xs text-muted">
			{total}
			{total === 1 ? 'asset' : 'assets'} · most recently added first
		</p>
		<!--
			The same grid the browse page uses, so keyboard behaviour, virtualisation and the ARIA row counts are
			the ones already proven rather than a second implementation of them. `f` and the star both remove from
			this list, because on this page that is what un-favouriting means.
		-->
		<AssetGrid
			{items}
			{total}
			columns={4}
			height={640}
			rowHeight={224}
			thumbnail={deliveryUrl}
			onfavourite={remove}
		/>
	{/if}
</div>
