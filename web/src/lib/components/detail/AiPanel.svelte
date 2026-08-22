<!--
	What a model wrote on this asset (M5b·4, G2).

	## Why this is on the asset and not only in the review queue

	Because that is what marking *is*. Article 50's obligation is that somebody encountering the content can tell
	which of it a machine produced, and a marking visible only to whoever runs the review queue is a marking the
	people it exists for never see. So this reads `GET /assets/{id}/ai`, which is Read-gated: anybody who may see
	the asset may see that a model described it.

	## Graded, not a warning

	An authentic photograph with an AI-written description is not "AI-generated content", and saying so would be
	both wrong and commercially damaging — 0006's `disclosure_kind` grades exactly this distinction. So the wording
	names the field and the model, and says nothing about the picture.

	## Loaded when opened

	Same posture as the history panel: a detail view already makes several requests, and most people opening an
	asset want the picture and the fields. A closed disclosure costs nothing.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { ApiError, readAssetAi, type MachineField } from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	let fields = $state<MachineField[]>([]);
	let error = $state('');
	let loaded = $state(false);
	let open = $state(false);

	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		// The id rather than the prop object: the parent replaces the asset on every refresh, and comparing the
		// object would refetch on each one.
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			error = '';
			loaded = false;
			if (open) void load();
		});
	});

	function reveal(event: Event) {
		open = (event.currentTarget as HTMLDetailsElement).open;
		if (open && !loaded) void load();
	}

	async function load() {
		try {
			fields = await readAssetAi(assetId);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the disclosure.';
		} finally {
			loaded = true;
		}
	}

	function shown(value: unknown): string {
		if (typeof value === 'string') return value;
		return value === null || value === undefined ? '' : JSON.stringify(value);
	}
</script>

<details class="space-y-2" {open} ontoggle={reveal}>
	<summary class="cursor-pointer text-xs font-semibold tracking-wide text-muted uppercase">
		Written by AI
	</summary>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
		>
			{error}
		</p>
	{:else if !loaded}
		<p class="text-xs text-muted">Loading…</p>
	{:else if fields.length === 0}
		<!--
			Said rather than hidden. "No AI here" is an answer somebody may specifically be looking for, and an
			empty panel would leave them wondering whether the request failed.
		-->
		<p class="text-xs text-muted">
			Nothing on this asset was written by a model. The picture and every field are as people left
			them.
		</p>
	{:else}
		<ul class="space-y-1">
			{#each fields as field (field.key)}
				<li class="rounded-md border border-line px-2 py-1.5 text-xs">
					<div class="flex flex-wrap items-baseline gap-x-2">
						<span class="font-semibold tracking-wide text-muted uppercase">{field.key}</span>
						<span class="font-mono text-muted">{field.model}</span>
						{#if field.confidence !== null && field.confidence !== undefined}
							<span class="text-muted">claimed {Math.round(field.confidence * 100)}%</span>
						{/if}
						{#if field.reviewed}
							<span
								class="rounded bg-state-rights-allowed/18 px-1.5 py-0.5 text-state-rights-allowed-fg"
							>
								checked by a person
							</span>
						{:else}
							<span class="rounded bg-surface px-1.5 py-0.5">not checked yet</span>
						{/if}
					</div>
					<p class="mt-1">{shown(field.value)}</p>
				</li>
			{/each}
		</ul>
		<p class="text-xs text-muted">
			The image itself is untouched — this lists the descriptive text a model wrote. Editing a field
			removes its marking.
		</p>
	{/if}
</details>
