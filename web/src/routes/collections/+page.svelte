<script lang="ts">
	/**
	 * Collections: the sets a portal publishes, and the order they publish them in.
	 *
	 * ## The key is stated on the form, not derived from the label
	 *
	 * A slugified label would be friendlier for about a week — until somebody fixed a typo in "Sping 2026" and
	 * discovered that every portal built on it had either broken or silently repointed. The key is asked for
	 * once, shown on every row, and cannot be changed afterwards; the label is the thing anybody actually
	 * wanted to edit.
	 *
	 * ## Pinning is money, and says so
	 *
	 * `pin_hot` keeps every member's original in the hottest storage class for as long as it holds them, which
	 * is the difference between a live portal page and a broken image — and also a bill. It defaults off and
	 * the row explains what turning it on does, because a checkbox labelled "pin hot" tells nobody anything.
	 *
	 * ## Order is edited in place, one step at a time
	 *
	 * No drag and drop. Two buttons per row move an item up or down, which is keyboard-reachable, screen-reader
	 * announceable and exactly what the `position` endpoint does; a drag implementation would be a second
	 * ordering model layered over the server's, and reachable by mouse only.
	 *
	 * ## A gap in the numbering is deliberate
	 *
	 * The server returns only the members this caller can see, with their real positions. So a scoped curator
	 * may see positions 0, 2 and 3 — and the page says so rather than renumbering, because renumbering would
	 * be a quieter lie about what the collection holds.
	 */
	import { onMount } from 'svelte';
	import { resolve } from '$app/paths';
	import {
		addToCollection,
		amendCollection,
		ApiError,
		collectionItems,
		createCollection,
		deleteCollection,
		deliveryUrl,
		listCollections,
		moveInCollection,
		removeFromCollection,
		type Collection,
		type CollectionItem
	} from '$lib/api/client';

	let collections = $state<Collection[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');

	/** Which collection's members are open. One at a time: two open lists is two virtualised grids of nothing. */
	let open = $state('');
	let items = $state<CollectionItem[]>([]);
	let busy = $state('');

	/** The create form. */
	let newKey = $state('');
	let newLabel = $state('');
	let newDescription = $state('');
	let newPin = $state(false);
	let creating = $state(false);

	/** The row being edited, and its draft. */
	let editing = $state('');
	let draft = $state({ label: '', description: '', visibility: 'private', pin_hot: false });

	async function load() {
		error = '';
		try {
			collections = await listCollections();
		} catch (caught) {
			error =
				caught instanceof ApiError && caught.status === 403
					? 'Collections are administration. This key needs Manage.'
					: 'Could not read the collections.';
		} finally {
			loading = false;
		}
	}

	onMount(load);

	async function make(event: SubmitEvent) {
		event.preventDefault();
		creating = true;
		error = '';
		try {
			const made = await createCollection({
				key: newKey.trim(),
				label: newLabel.trim() || newKey.trim(),
				description: newDescription.trim() || undefined,
				pin_hot: newPin
			});
			collections = [...collections, made];
			notice = `${made.label} created. Its key is ${made.key}, and that is what a portal will reference.`;
			newKey = '';
			newLabel = '';
			newDescription = '';
			newPin = false;
		} catch (caught) {
			// The 409 body already says which key is taken, so it is shown as it stands rather than replaced
			// with a sentence that says less.
			error = caught instanceof Error ? caught.message : 'Could not create that collection.';
		} finally {
			creating = false;
		}
	}

	function edit(collection: Collection) {
		editing = collection.id;
		draft = {
			label: collection.label,
			description: collection.description ?? '',
			visibility: collection.visibility,
			pin_hot: collection.pin_hot
		};
	}

	async function save(collection: Collection) {
		busy = collection.id;
		error = '';
		try {
			const amended = await amendCollection(collection.id, {
				label: draft.label.trim() || collection.label,
				description: draft.description.trim() || undefined,
				visibility: draft.visibility,
				pin_hot: draft.pin_hot
			});
			collections = collections.map((one) => (one.id === amended.id ? amended : one));
			editing = '';
			notice = `${amended.label} saved.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not save that collection.';
		} finally {
			busy = '';
		}
	}

	async function drop(collection: Collection) {
		busy = collection.id;
		error = '';
		try {
			await deleteCollection(collection.id);
			collections = collections.filter((one) => one.id !== collection.id);
			if (open === collection.id) open = '';
			notice = `${collection.label} deleted. Its assets are untouched — a collection holds references.`;
		} catch (caught) {
			// A published collection comes back as a 409 naming the portals. That sentence is the whole
			// value of the guard, so it is what the operator sees.
			error = caught instanceof Error ? caught.message : 'Could not delete that collection.';
		} finally {
			busy = '';
		}
	}

	async function show(collection: Collection) {
		if (open === collection.id) {
			open = '';
			items = [];
			return;
		}
		open = collection.id;
		items = [];
		error = '';
		try {
			items = await collectionItems(collection.id);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not read the members.';
		}
	}

	/** Moves a member one step and takes the server's new order, not a locally guessed one. */
	async function move(collection: Collection, item: CollectionItem, by: number) {
		const index = items.findIndex((one) => one.asset_id === item.asset_id);
		const target = index + by;
		if (target < 0 || target >= items.length) return;
		busy = item.asset_id;
		try {
			// The neighbour's position, not `index + by`: with a scoped view the positions are the server's real
			// ones and may not be contiguous, so stepping by array index would jump over a member this caller
			// cannot see.
			items = await moveInCollection(collection.id, item.asset_id, items[target].position);
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not reorder.';
		} finally {
			busy = '';
		}
	}

	async function forget(collection: Collection, item: CollectionItem) {
		busy = item.asset_id;
		try {
			await removeFromCollection(collection.id, item.asset_id);
			// Refetched, not filtered locally. A removal closes the gap it left, so every position after it
			// changes — and dropping the row client-side leaves numbers that no longer match the server. The
			// browser suite caught exactly that: two removals left the list showing 0–7 and 9, and the
			// scoped-members banner below fired on a hole that did not exist.
			items = await collectionItems(collection.id);
			collections = collections.map((one) =>
				one.id === collection.id ? { ...one, item_count: items.length } : one
			);
			notice = `${item.filename} removed from ${collection.label}.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not remove that member.';
		} finally {
			busy = '';
		}
	}

	/**
	 * Whether the visible positions have a hole in them — see the component docs.
	 *
	 * Only ever true because the server withheld a member this caller cannot see: every mutation on this screen
	 * takes the server's own list back, so a hole here is never local staleness.
	 */
	const gapped = $derived(items.some((item, index) => item.position !== index));

	// Kept for the `addToCollection` import, which the bulk bar uses on the grid: this screen also accepts a
	// paste of ids, because the first thing anybody does with a fresh collection is fill it from a list they
	// already have.
	let pasted = $state('');
	async function addPasted(collection: Collection) {
		const ids = pasted
			.split(/[\s,]+/)
			.map((one) => one.trim())
			.filter(Boolean);
		if (ids.length === 0) return;
		busy = collection.id;
		error = '';
		try {
			const added = await addToCollection(collection.id, ids);
			notice =
				added.out_of_scope > 0
					? `Added ${added.added}. ${added.out_of_scope} were outside your scope or not assets.`
					: `Added ${added.added} to ${collection.label}.`;
			pasted = '';
			items = await collectionItems(collection.id);
			collections = await listCollections();
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not add those.';
		} finally {
			busy = '';
		}
	}
</script>

<svelte:head><title>Collections · damrs</title></svelte:head>

<div class="space-y-6 p-4">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Collections</h1>
		<p class="max-w-2xl text-sm text-muted">
			A collection is a set of assets in an order somebody chose. A portal publishes one, so this is
			where a public page starts. Select assets on the
			<a class="underline" href={resolve('/assets')}>Assets</a> screen to add them in bulk.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>
	{#if notice}
		<p class="text-xs text-muted">{notice}</p>
	{/if}

	<form class="flex flex-wrap items-end gap-3 rounded-md border border-line p-3" onsubmit={make}>
		<label class="space-y-1 text-xs">
			<span class="block text-muted">Key</span>
			<input
				bind:value={newKey}
				required
				pattern="[a-z0-9][a-z0-9\-]*"
				placeholder="spring-2026"
				class="w-40 rounded-md border border-line bg-surface px-2 py-1 font-mono text-sm"
			/>
		</label>
		<label class="space-y-1 text-xs">
			<span class="block text-muted">Label</span>
			<input
				bind:value={newLabel}
				placeholder="Spring 2026"
				class="w-48 rounded-md border border-line bg-surface px-2 py-1 text-sm"
			/>
		</label>
		<label class="space-y-1 text-xs">
			<span class="block text-muted">Description</span>
			<input
				bind:value={newDescription}
				class="w-64 rounded-md border border-line bg-surface px-2 py-1 text-sm"
			/>
		</label>
		<label class="flex items-center gap-2 text-xs">
			<input type="checkbox" bind:checked={newPin} />
			Keep originals instantly available
		</label>
		<button
			type="submit"
			disabled={creating}
			class="rounded-md border border-line px-3 py-1.5 text-sm hover:bg-raised disabled:opacity-50"
		>
			{creating ? 'Creating…' : 'Create'}
		</button>
		<p class="w-full text-xs text-muted">
			The key cannot be changed later — a portal references it. Pinning keeps every member’s
			original in the hottest storage class, which costs more and means a portal page never waits on
			a restore.
		</p>
	</form>

	{#if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if collections.length === 0}
		<p class="text-sm text-muted">
			No collections yet. Make one above, then select assets on the Assets screen and use “Add to
			collection”.
		</p>
	{/if}

	<ul class="space-y-3">
		{#each collections as collection (collection.id)}
			<li class="space-y-3 rounded-md border border-line p-3">
				<div class="flex flex-wrap items-baseline gap-3">
					<h2 class="text-sm font-semibold tracking-tight">{collection.label}</h2>
					<span class="font-mono text-xs text-muted">{collection.key}</span>
					<span class="text-xs text-muted">
						{collection.item_count}
						{collection.item_count === 1 ? 'asset' : 'assets'}
					</span>
					<span class="rounded border border-line px-1.5 py-0.5 text-xs"
						>{collection.visibility}</span
					>
					{#if collection.pin_hot}
						<span class="rounded border border-line px-1.5 py-0.5 text-xs">
							pinned — originals stay hot
						</span>
					{/if}
					{#if collection.description}
						<span class="text-xs text-muted">{collection.description}</span>
					{/if}

					<button
						type="button"
						class="ml-auto rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
						aria-expanded={open === collection.id}
						onclick={() => show(collection)}
					>
						{open === collection.id ? 'Hide members' : 'Members'}
					</button>
					<button
						type="button"
						class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised"
						onclick={() => edit(collection)}
					>
						Edit
					</button>
					<button
						type="button"
						class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
						disabled={busy === collection.id}
						onclick={() => drop(collection)}
					>
						Delete
					</button>
				</div>

				{#if editing === collection.id}
					<div class="flex flex-wrap items-end gap-3 border-t border-line pt-2">
						<label class="space-y-1 text-xs">
							<span class="block text-muted">Label</span>
							<input
								bind:value={draft.label}
								class="w-48 rounded-md border border-line bg-surface px-2 py-1 text-sm"
							/>
						</label>
						<label class="space-y-1 text-xs">
							<span class="block text-muted">Description</span>
							<input
								bind:value={draft.description}
								class="w-64 rounded-md border border-line bg-surface px-2 py-1 text-sm"
							/>
						</label>
						<label class="space-y-1 text-xs">
							<span class="block text-muted">Visibility</span>
							<select
								bind:value={draft.visibility}
								class="rounded-md border border-line bg-surface px-2 py-1 text-sm"
							>
								<option value="private">private</option>
								<option value="shared">shared</option>
								<option value="public">public</option>
							</select>
						</label>
						<label class="flex items-center gap-2 text-xs">
							<input type="checkbox" bind:checked={draft.pin_hot} />
							Keep originals instantly available
						</label>
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === collection.id}
							onclick={() => save(collection)}
						>
							Save
						</button>
						<button
							type="button"
							class="rounded-md px-2.5 py-1 text-xs text-muted underline"
							onclick={() => (editing = '')}
						>
							Cancel
						</button>
						<p class="w-full text-xs text-muted">
							The key stays <span class="font-mono">{collection.key}</span>. Everything else here
							can change.
						</p>
					</div>
				{/if}

				{#if open === collection.id}
					<div class="space-y-2 border-t border-line pt-2">
						{#if items.length === 0}
							<p class="text-xs text-muted">
								Nothing in it yet. Select assets on the Assets screen and use “Add to collection”,
								or paste ids below.
							</p>
						{:else}
							{#if gapped}
								<p class="text-xs text-muted">
									Some positions are not shown: this collection holds assets outside your scope. The
									numbers are the real ones.
								</p>
							{/if}
							<ol class="space-y-1">
								{#each items as item, index (item.asset_id)}
									<li class="flex items-center gap-3 text-xs">
										<span class="w-6 text-right text-muted tabular-nums">{item.position}</span>
										{#if item.thumbnail_url}
											<!-- 40px, decorative-adjacent: the filename beside it is the label, so alt
											     text repeating it would be read twice. -->
											<img
												src={deliveryUrl(item.thumbnail_url)}
												alt=""
												width="40"
												height="40"
												loading="lazy"
												class="h-10 w-10 rounded object-cover"
											/>
										{:else}
											<span
												class="flex h-10 w-10 items-center justify-center rounded border border-line font-mono text-[10px] text-muted"
											>
												{item.mime.split('/')[1]?.slice(0, 4) ?? '?'}
											</span>
										{/if}
										<span class="truncate">{item.filename}</span>
										<span class="ml-auto flex gap-1">
											<button
												type="button"
												class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
												disabled={index === 0 || busy === item.asset_id}
												aria-label="Move {item.filename} up"
												onclick={() => move(collection, item, -1)}
											>
												↑
											</button>
											<button
												type="button"
												class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
												disabled={index === items.length - 1 || busy === item.asset_id}
												aria-label="Move {item.filename} down"
												onclick={() => move(collection, item, 1)}
											>
												↓
											</button>
											<!--
												Named, like the two move buttons: a column of eight buttons all called
												"Remove" is a list a screen reader cannot tell apart, and it was also what
												made the browser suite unable to address one row.
											-->
											<button
												type="button"
												class="rounded-md border border-line px-2 py-0.5 hover:bg-raised disabled:opacity-40"
												disabled={busy === item.asset_id}
												aria-label="Remove {item.filename} from {collection.label}"
												onclick={() => forget(collection, item)}
											>
												Remove
											</button>
										</span>
									</li>
								{/each}
							</ol>
						{/if}

						<div class="flex flex-wrap items-end gap-2">
							<label class="space-y-1 text-xs">
								<span class="block text-muted">Add asset ids</span>
								<input
									bind:value={pasted}
									placeholder="one or more ids, separated by spaces or commas"
									class="w-96 rounded-md border border-line bg-surface px-2 py-1 font-mono text-xs"
								/>
							</label>
							<button
								type="button"
								class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
								disabled={busy === collection.id}
								onclick={() => addPasted(collection)}
							>
								Add
							</button>
						</div>
					</div>
				{/if}
			</li>
		{/each}
	</ul>
</div>
