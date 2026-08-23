<script lang="ts">
	/**
	 * One worklist, as a grid.
	 *
	 * The same `AssetGrid` the browse page uses, so keyboard navigation, virtualisation and the ARIA row
	 * counts are the ones already proven rather than a second implementation of them — and so a finding opens
	 * into something a person can act in.
	 *
	 * The order is oldest first, decided by the server: a worklist is a backlog, and the asset that has been
	 * waiting longest is the one to fix. Newest-first would show the same top rows to everybody who ever opens
	 * it while the old work sank.
	 */
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import AssetGrid from '$lib/components/grid/AssetGrid.svelte';
	import {
		ApiError,
		deliveryUrl,
		listWorklists,
		worklistPage,
		type AssetSummary,
		type Worklist
	} from '$lib/api/client';

	let items = $state<AssetSummary[]>([]);
	let total = $state(0);
	let list = $state<Worklist | null>(null);
	let loading = $state(true);
	let error = $state('');

	const key = $derived(page.params.key ?? '');

	onMount(async () => {
		try {
			// Both, because the label and the sentence live on the server beside the SQL that decides the
			// list — a heading hard-coded here would be a second copy that drifts.
			const [lists, first] = await Promise.all([listWorklists(), worklistPage(key)]);
			list = lists.find((one) => one.key === key) ?? null;
			items = first.items;
			total = first.total;
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 404
					? 'No such worklist.'
					: 'Could not read that worklist.';
		} finally {
			loading = false;
		}
	});
</script>

<svelte:head><title>{list?.label ?? 'Worklist'} · damrs</title></svelte:head>

<div class="space-y-4 p-8">
	<div>
		<a href={resolve('/worklists')} class="text-xs text-muted underline">All worklists</a>
		<h1 class="mt-1 text-2xl font-semibold tracking-tight">{list?.label ?? 'Worklist'}</h1>
		{#if list}
			<p class="mt-1 max-w-2xl text-sm text-muted">{list.explanation}</p>
		{/if}
	</div>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if items.length === 0}
		<p class="text-sm text-muted">Nothing on this list that you can see.</p>
	{:else}
		<p class="text-xs text-muted">
			{total}
			{total === 1 ? 'asset' : 'assets'} · longest waiting first
		</p>
		<AssetGrid {items} {total} columns={4} height={640} rowHeight={224} thumbnail={deliveryUrl} />
	{/if}
</div>
