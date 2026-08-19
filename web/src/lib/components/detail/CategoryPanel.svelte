<!--
	Where this asset is filed, and how to change it.

	## Chips, deepest first

	A category is a path, and the useful thing to read is the leaf — "Yellow" tells you more than "Exterior".
	The server orders deepest-first for that reason, and the chip shows the full path underneath so a reader can
	tell two "Yellow"s apart when they live in different branches.

	## Filing is a picker over the tree, not a text box

	A free-text field would invite typing a category that does not exist, and the honest response to that is a
	refusal — so the control offers what exists instead. Retired categories are excluded: they cannot take new
	assets, and offering one would produce a refusal the user could not have predicted.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		ApiError,
		fileInCategory,
		listCategoryTrees,
		readAssetCategories,
		readCategoryTree,
		unfileFromCategory,
		type CategoryNode
	} from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	let filed = $state<CategoryNode[]>([]);
	let available = $state<CategoryNode[]>([]);
	let error = $state('');
	let busy = $state(false);
	let picking = $state(false);
	let chosen = $state('');

	/** Everything not already on this asset, and not retired. */
	const offerable = $derived(
		available.filter((node) => !node.retired && !filed.some((existing) => existing.id === node.id))
	);

	/**
	 * Reloads for the selected asset.
	 *
	 * `assetId` is the only tracked read — everything else the body writes it also reads, and tracking those
	 * would make each write re-run the effect. The same trap the bulk bar hit.
	 */
	$effect(() => {
		const id = assetId;
		untrack(() => {
			error = '';
			picking = false;
			chosen = '';
			void (async () => {
				try {
					const [onAsset, trees] = await Promise.all([
						readAssetCategories(id),
						listCategoryTrees()
					]);
					filed = onAsset;
					const first = trees[0];
					available = first ? await readCategoryTree(first.id) : [];
				} catch (caught) {
					error = caught instanceof ApiError ? caught.message : 'Could not read categories.';
				}
			})();
		});
	});

	async function run(work: () => Promise<CategoryNode[]>) {
		busy = true;
		error = '';
		try {
			// The endpoints answer with the asset's categories afterwards, so the chips redraw from the server's
			// answer rather than from an optimistic guess that could disagree with it.
			filed = await work();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'That change could not be made.';
		} finally {
			busy = false;
		}
	}

	function add(event: SubmitEvent) {
		event.preventDefault();
		if (!chosen) return;
		const id = chosen;
		void run(async () => {
			const next = await fileInCategory(assetId, id);
			picking = false;
			chosen = '';
			return next;
		});
	}
</script>

<!-- Hidden entirely when the tenant has no categories: an empty "Categories" heading reads as a loading
     state that never finishes, and there is nothing to do here until somebody builds a tree. -->
{#if available.length > 0 || filed.length > 0}
	<div class="space-y-2">
		<div class="flex items-center justify-between gap-2">
			<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">Categories</h3>
			{#if offerable.length > 0}
				<button
					type="button"
					class="text-xs underline"
					onclick={() => (picking = !picking)}
					aria-expanded={picking}
				>
					{picking ? 'Cancel' : 'File…'}
				</button>
			{/if}
		</div>

		{#if filed.length === 0}
			<p class="text-sm text-muted">Not filed anywhere.</p>
		{:else}
			<ul class="flex flex-wrap gap-1.5">
				{#each filed as node (node.id)}
					<li class="flex items-center gap-1 rounded-md bg-surface px-2 py-1">
						<span class="text-sm">{node.label}</span>
						<!-- The full path, because two branches can legitimately hold a "Yellow". -->
						<span class="font-mono text-xs text-muted">{node.path}</span>
						<button
							type="button"
							class="text-xs text-state-rights-denied-fg"
							aria-label={`Remove from ${node.label}`}
							disabled={busy}
							onclick={() => run(() => unfileFromCategory(assetId, node.id))}
						>
							×
						</button>
					</li>
				{/each}
			</ul>
		{/if}

		{#if picking}
			<form class="flex items-center gap-2" onsubmit={add}>
				<label class="sr-only" for="category-picker">Category</label>
				<select
					id="category-picker"
					class="min-w-0 flex-1 rounded-md border border-line bg-bg px-2 py-1 text-sm"
					bind:value={chosen}
				>
					<option value="" disabled>Choose a category…</option>
					{#each offerable as node (node.id)}
						<!-- Indented by depth so the hierarchy is legible in a flat list; the path is the label a
						     reader can act on when two leaves share a name. -->
						<option value={node.id}>
							{' '.repeat(node.depth * 2)}{node.label}
						</option>
					{/each}
				</select>
				<button
					type="submit"
					class="rounded-md bg-accent px-2.5 py-1 text-sm font-medium text-accent-fg disabled:opacity-50"
					disabled={busy || !chosen}
				>
					File
				</button>
			</form>
		{/if}

		{#if error}
			<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
		{/if}
	</div>
{/if}
