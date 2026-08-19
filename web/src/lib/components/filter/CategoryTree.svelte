<!--
	The browse tree: where assets are filed, and how you navigate into it.

	## Navigating, not ticking

	The facets below this are checkboxes because "brand is Acme *or* Globex" is a normal thing to want. A
	category is different: `in:a in:b` means "filed in both", which almost always returns nothing while looking
	like the tree is broken. So selecting a category replaces the previous one, and selecting the current one
	again clears it — which is what clicking through a hierarchy means everywhere else.

	## Counts are this reader's own

	A group-scoped reader legitimately sees smaller numbers here than an administrator. That is §7: a count
	reporting the true total would disclose the size of the part of the library they cannot reach.

	## Empty branches stay visible

	A category with no assets *for this reader* still renders, greyed, rather than vanishing. A tree that hid
	its empty branches would change shape as the reader's scope changed, and "the category I was told to file
	under is missing" is a worse problem than "it is there and says zero".
-->
<script lang="ts">
	import {
		ApiError,
		listCategoryTrees,
		readCategoryTree,
		type CategoryNode
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';
	import { selectedValue, withOnlyTerm } from '$lib/search/query';

	let {
		query,
		onquery
	}: {
		query: string;
		onquery: (next: string) => void;
	} = $props();

	let nodes = $state<CategoryNode[]>([]);
	let treeLabel = $state('');
	let error = $state('');
	let loaded = $state(false);

	const selected = $derived(selectedValue(query, 'in'));

	/**
	 * Which branches are open.
	 *
	 * Derived from the selection rather than stored, so arriving on a link like `?q=in:exterior.yellow` shows
	 * the branch already expanded — a tree that opened collapsed would hide the very thing the URL named.
	 * Manual toggles are layered on top.
	 */
	let manuallyToggled = $state<Record<string, boolean>>({});

	function isOpen(node: CategoryNode): boolean {
		const manual = manuallyToggled[node.path];
		if (manual !== undefined) return manual;
		return selected !== null && (selected === node.path || selected.startsWith(`${node.path}.`));
	}

	/** A node is visible when every ancestor of it is open. */
	function isVisible(node: CategoryNode): boolean {
		if (node.depth === 0) return true;
		const segments = node.path.split('.');
		for (let cut = 1; cut < segments.length; cut += 1) {
			const ancestorPath = segments.slice(0, cut).join('.');
			const ancestor = nodes.find((candidate) => candidate.path === ancestorPath);
			if (!ancestor || !isOpen(ancestor)) return false;
		}
		return true;
	}

	function hasChildren(node: CategoryNode): boolean {
		return nodes.some((candidate) => candidate.parent_id === node.id);
	}

	function choose(node: CategoryNode) {
		// Selecting the current one clears it, which is how somebody gets back to the whole library without
		// hunting for a "clear" control.
		onquery(withOnlyTerm(query, 'in', selected === node.path ? null : node.path));
	}

	$effect(() => {
		void (async () => {
			try {
				const trees = await listCategoryTrees();
				// One tree is the normal case. Several is legal in the schema, and the first by label is a
				// deliberate simplification rather than an oversight — a tree *picker* is worth building when a
				// tenant actually has two, and guessing at the UI for it now would be guessing.
				const first = trees[0];
				if (!first) {
					loaded = true;
					return;
				}
				treeLabel = first.label;
				nodes = await readCategoryTree(first.id);
				loaded = true;
			} catch (caught) {
				error = caught instanceof ApiError ? caught.message : 'Could not load categories.';
				loaded = true;
			}
		})();
	});
</script>

<!--
	An unconnected app is *not* this component's story to tell. The page already carries one banner explaining
	it, and a second alert repeating it is noise a user cannot act on separately — an e2e case caught exactly
	that: two `role="alert"` nodes saying the same sentence. A real failure while connected is still reported,
	because that one is news.
-->
{#if error && session.connected}
	<p role="alert" class="text-xs text-state-rights-denied-fg">{error}</p>
{:else if loaded && nodes.length > 0}
	<section aria-label="Categories" class="space-y-1">
		<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">
			{treeLabel || 'Categories'}
		</h3>

		{#if selected}
			<button
				type="button"
				class="text-xs text-accent underline"
				onclick={() => onquery(withOnlyTerm(query, 'in', null))}
			>
				Everything
			</button>
		{/if}

		<!--
			A tree of buttons rather than links: this edits the query in place, and a link would imply a
			navigation that discards the rest of the search. `aria-expanded` is on the disclosure control only,
			so a leaf does not claim to be expandable.
		-->
		<ul class="space-y-0.5">
			{#each nodes.filter(isVisible) as node (node.id)}
				<li class="flex items-center gap-1" style={`padding-left: ${node.depth * 0.75}rem`}>
					{#if hasChildren(node)}
						<button
							type="button"
							class="w-4 shrink-0 text-xs text-muted"
							aria-expanded={isOpen(node)}
							aria-label={`${isOpen(node) ? 'Collapse' : 'Expand'} ${node.label}`}
							onclick={() => (manuallyToggled[node.path] = !isOpen(node))}
						>
							{isOpen(node) ? '▾' : '▸'}
						</button>
					{:else}
						<span class="w-4 shrink-0"></span>
					{/if}

					<!--
						An explicit label, because the visible text is "Exterior 9" and read aloud that is a
						riddle. It also keeps this button distinguishable from the disclosure control beside it,
						which otherwise shares the category's name.
					-->
					<button
						type="button"
						class="flex min-w-0 flex-1 items-baseline gap-1.5 text-left text-sm hover:underline
						       {selected === node.path ? 'font-semibold text-accent' : ''}
						       {node.assets === 0 ? 'text-muted' : ''}"
						aria-label={`${node.label}, ${node.assets} ${node.assets === 1 ? 'asset' : 'assets'}${
							node.retired ? ', retired' : ''
						}`}
						aria-current={selected === node.path ? 'true' : undefined}
						onclick={() => choose(node)}
					>
						<span class="truncate">{node.label}</span>
						<!-- The count is this reader's own; see the note at the top of the file. -->
						<span class="tabular shrink-0 text-xs text-muted">{node.assets.toLocaleString()}</span>
						{#if node.retired}
							<span class="shrink-0 text-xs text-muted">(retired)</span>
						{/if}
					</button>
				</li>
			{/each}
		</ul>
	</section>
{/if}
