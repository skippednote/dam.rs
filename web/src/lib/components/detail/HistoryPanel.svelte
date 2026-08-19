<!--
	Everything that has happened to one asset (Q.10).

	## The same sentence as the dashboard, on purpose

	A history line and a feed line say the same kind of thing, and the API returns the same shape for both. So the
	phrasing comes from `$lib/activity` rather than being written again here — one place to add a verb when a new
	event kind appears.

	## Loaded when opened, not with the asset

	A detail panel already makes several requests, and most people opening an asset want the picture and the fields —
	not an audit trail. So this is a disclosure: nothing is fetched until somebody opens it. It then *stays* open as
	they move between assets, because somebody who opened one history is usually comparing several, and re-opening it
	on every selection would be the panel forgetting what they just asked for.

	## The whole version group

	Somebody looking at March's cut needs to see that April's replaced it — that is the entry which explains all the
	others. The server decides this; the panel says so in a line beneath the list, because a person reading a history
	that mentions a filename they did not open deserves to know why it is there.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { ApiError, loadHistory, type ActivityEntry } from '$lib/api/client';
	import { phrase, when } from '$lib/activity';

	let { assetId, filename }: { assetId: string; filename?: string } = $props();

	let entries = $state<ActivityEntry[]>([]);
	let error = $state('');
	let loaded = $state(false);
	let open = $state(false);

	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		// Compare the id rather than merely reading the prop: the parent replaces the asset object on every refresh,
		// which re-runs this effect with an identical id and would otherwise refetch on each one.
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			// `loaded = false` gates the list below, which is what stops the previous asset's history showing under
			// the new one's name while the request is in flight.
			error = '';
			loaded = false;
			// Only if they are looking. A closed disclosure costs nothing.
			if (open) void load();
		});
	});

	/** First open, or an open while the current asset's history has not been fetched. */
	function reveal(event: Event) {
		open = (event.currentTarget as HTMLDetailsElement).open;
		if (open && !loaded) void load();
	}

	async function load() {
		try {
			entries = await loadHistory(assetId);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the history.';
		} finally {
			loaded = true;
		}
	}

	/** Whether any line is about a different row in the group — the only case where the note below is worth saying. */
	let mentionsSiblings = $derived(
		entries.some((entry) => entry.filename && filename && entry.filename !== filename)
	);
</script>

<details class="space-y-2" {open} ontoggle={reveal}>
	<summary class="cursor-pointer text-xs font-semibold tracking-wide text-muted uppercase">
		History
	</summary>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}

	{#if !loaded}
		<p class="text-xs text-muted">Loading…</p>
	{:else if error}
		<!--
			Nothing else. Driving this against the live server showed the failure and "nothing recorded yet" stacked
			on top of each other, which reads as "the request failed, and also there is no history" — a claim the
			panel is in no position to make. A failed read knows nothing about what happened to the asset.
		-->
	{:else if entries.length === 0}
		<!--
			Said rather than hidden. An asset imported before events were recorded has no history, and an empty panel
			would leave a reader wondering whether the request failed.
		-->
		<p class="text-xs text-muted">
			Nothing recorded yet. Uploads, edits, shares, comments and downloads appear here.
		</p>
	{:else}
		<ol class="space-y-1">
			{#each entries as entry (entry.id)}
				<li
					class="flex flex-wrap items-baseline gap-x-2 rounded-md border border-line px-2 py-1.5 text-xs"
				>
					<span class="min-w-0 flex-1">{phrase(entry)}</span>
					<!--
						The exact instant in the title, the rough one in the text. "3 h ago" is what somebody scanning
						wants; the timestamp is what somebody reconciling with another system wants.
					-->
					<time
						class="text-muted tabular-nums"
						datetime={entry.occurred_at}
						title={new Date(entry.occurred_at).toLocaleString()}
					>
						{when(entry.occurred_at)}
					</time>
				</li>
			{/each}
		</ol>
		{#if mentionsSiblings}
			<p class="text-xs text-muted">
				Includes every version of this asset, so a name you did not open may appear.
			</p>
		{/if}
	{/if}
</details>
