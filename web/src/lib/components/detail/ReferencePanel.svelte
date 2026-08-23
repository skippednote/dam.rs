<!--
	Where this asset is used on connected sites (M3d·4, §11.4).

	## This is the panel somebody reads before a takedown

	"Delete" and "expire the licence" both have consequences on other people's websites, and this is the only
	place in the application that can say what they are. So it is always visible: "used nowhere" is a fact
	somebody about to pull an asset needs, and hiding an empty panel would make its absence indistinguishable
	from the feature not existing — the same argument the attachment panel makes about a missing release form.

	## The counts and the list answer different questions, and the panel says which

	The counts are what is **live**: linked, in use, on an active site, reported recently. The list is
	**everything**, including references a site stopped reporting. Showing only the counts would hide "one site
	went quiet three weeks ago", which is exactly the thing that makes a number untrustworthy — and showing only
	the list would make somebody add up columns.

	## `pages` is the site's own number and is labelled as such

	damrs cannot count pages on somebody else's website; it can only repeat what the site reported. Presenting
	that as a hard total beside two numbers damrs *does* know would be the kind of confident-sounding figure
	people make decisions on.

	## A quiet site is called out rather than quietly discounted

	A reference nobody has refreshed stops counting — that is what stops an abandoned integration holding a
	library in Standard forever — but the row stays and says so. An operator seeing "not reported for a while"
	knows to go and look at the site rather than concluding the asset is unused.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { ApiError, assetReferences, type ReferenceImpact } from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	let impact = $state<ReferenceImpact | null>(null);
	let error = $state('');
	let loaded = $state(false);
	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			error = '';
			// Gated on rather than cleared, for the reason the attachment panel documents: this line is what
			// prevents a flash of the previous asset's usage while the next request is in flight.
			loaded = false;
			void load();
		});
	});

	async function load() {
		try {
			impact = await assetReferences(assetId);
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 404
					? ''
					: caught instanceof ApiError
						? caught.message
						: 'Could not read where this is used.';
		} finally {
			loaded = true;
		}
	}

	const live = $derived((impact?.references ?? []).filter((row) => row.state === 'linked'));
	const dead = $derived((impact?.references ?? []).filter((row) => row.state !== 'linked'));

	function why(state: string): string {
		switch (state) {
			case 'orphaned':
				return 'the site no longer lists it';
			case 'expired':
				return 'the licence expired';
			case 'unpublished':
				return 'the asset was unpublished';
			default:
				return state;
		}
	}
</script>

<section class="space-y-2 border-t border-line p-3" aria-labelledby="used-on">
	<h3 id="used-on" class="text-sm font-semibold tracking-tight">Used on connected sites</h3>

	{#if error}
		<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
	{/if}

	{#if !loaded}
		<p class="text-xs text-muted">Loading…</p>
	{:else if !impact || impact.sites === 0}
		<p class="text-xs text-muted" data-testid="used-nowhere">
			Not in use on any connected site.
			{#if dead.length > 0}
				{dead.length}
				{dead.length === 1 ? 'reference' : 'references'} used to exist — see below.
			{:else}
				Deleting it or letting its licence lapse will not change any page.
			{/if}
		</p>
	{:else}
		<!--
			Three numbers, and the third is labelled differently on purpose. damrs knows how many sites and how
			many entities; it can only repeat what each site said about pages.
		-->
		<dl class="flex flex-wrap gap-x-5 gap-y-1" data-testid="impact">
			<div>
				<dt class="text-xs text-muted">Sites</dt>
				<dd class="text-lg font-semibold tabular-nums">{impact.sites}</dd>
			</div>
			<div>
				<dt class="text-xs text-muted">Entities</dt>
				<dd class="text-lg font-semibold tabular-nums">{impact.entities}</dd>
			</div>
			<div>
				<dt class="text-xs text-muted">Pages, as reported</dt>
				<dd class="text-lg font-semibold tabular-nums">{impact.pages.toLocaleString()}</dd>
			</div>
		</dl>
		<p class="max-w-prose text-xs text-muted">
			Pulling this asset, or letting its licence lapse, changes what those pages show. The page
			count is each site's own — damrs cannot see somebody else's website, only what it reports.
		</p>
	{/if}

	{#if live.length > 0}
		<ul class="space-y-1" data-testid="live-references">
			{#each live as row (row.connector_id + row.remote_entity_id)}
				<li class="flex flex-wrap items-baseline gap-x-2 text-xs">
					<span class="font-medium">{row.connector_label}</span>
					{#if row.remote_url}
						<!-- Somebody else's website, so `rel="external"`: it is not a route in this application,
						     and telling SvelteKit not to intercept it is both correct and what the linter wants. -->
						<a
							href={row.remote_url}
							rel="external noreferrer"
							class="underline decoration-line hover:decoration-fg"
						>
							{row.remote_entity_type}/{row.remote_entity_id}
						</a>
					{:else}
						<span class="text-muted">{row.remote_entity_type}/{row.remote_entity_id}</span>
					{/if}
					<span class="text-muted">
						{row.usage_count}
						{row.usage_count === 1 ? 'page' : 'pages'}
					</span>
					{#if row.version_drifted}
						<!-- A job to run, not a site to chase: the sync worker has not pushed the new version yet. -->
						<span class="text-state-rights-expiring-fg">showing an older version</span>
					{/if}
					{#if row.refresh_overdue}
						<!-- The other kind of stale, and the one that changes what the numbers above mean. -->
						<span
							class="text-state-rights-expiring-fg"
							data-testid="overdue-{row.remote_entity_id}"
						>
							not reported for a while — not counted above
						</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}

	{#if dead.length > 0}
		<details>
			<summary class="cursor-pointer text-xs text-muted">
				{dead.length} no longer in use
			</summary>
			<ul class="mt-1 space-y-1" data-testid="dead-references">
				{#each dead as row (row.connector_id + row.remote_entity_id)}
					<li class="flex flex-wrap items-baseline gap-x-2 text-xs text-muted">
						<span>{row.connector_label}</span>
						<span>{row.remote_entity_type}/{row.remote_entity_id}</span>
						<span>{why(row.state)}</span>
					</li>
				{/each}
			</ul>
		</details>
	{/if}
</section>
