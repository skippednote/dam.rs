<!--
	The paperwork that goes with an asset: a release, a licence, a contract.

	## Always visible, unlike the version panel

	A version panel on a single-version asset is noise, because every asset is version 1 of itself. An *empty*
	attachment list is different: "no release form on file" is a fact somebody clearing an asset for use needs to
	see, and hiding the panel would make its absence indistinguishable from the feature not existing.

	## Detaching says it is not deleting

	The document goes back to being an ordinary asset. Somebody correcting a mis-attachment should not have to
	wonder whether they just destroyed a signed release.

	## The kind is named, not inferred

	A PDF could be any of these, and the difference matters for a rights question. So the kind is chosen when
	attaching, and shown on every row.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import { ApiError, detachDocument, listAttachments, type AssetAttachment } from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	let documents = $state<AssetAttachment[]>([]);
	let error = $state('');
	let notice = $state('');
	let busy = $state(false);
	let loaded = $state(false);
	let confirmingRemoval = $state<string | null>(null);

	const KINDS: Record<string, string> = {
		release: 'Release',
		licence: 'Licence',
		contract: 'Contract',
		permit: 'Permit',
		other: 'Document'
	};

	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			// `loaded = false` is what prevents a flash of the previous asset's paperwork: the list below is gated on
			// it, so the rows are not rendered while the next request is in flight. Clearing `documents` as well was
			// redundant — mutation testing showed removing it changed nothing, because this line already hides them.
			// One mechanism, and a test that samples the window rather than its end state.
			error = '';
			notice = '';
			loaded = false;
			confirmingRemoval = null;
			void load();
		});
	});

	async function load() {
		try {
			documents = await listAttachments(assetId);
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the attachments.';
		} finally {
			loaded = true;
		}
	}

	function detach(document: AssetAttachment) {
		confirmingRemoval = null;
		busy = true;
		error = '';
		void (async () => {
			try {
				await detachDocument(assetId, document.asset_id);
				documents = documents.filter((row) => row.asset_id !== document.asset_id);
				notice = `${document.filename} detached. It is an ordinary asset again — nothing was deleted.`;
			} catch (caught) {
				error = caught instanceof ApiError ? caught.message : 'That could not be detached.';
			} finally {
				busy = false;
			}
		})();
	}

	function bytes(n: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB'];
		let value = n;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}
</script>

<section class="space-y-2" aria-label="Attached documents">
	<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">
		Documents{#if documents.length > 0}&nbsp;({documents.length}){/if}
	</h3>

	{#if error}
		<p
			role="alert"
			class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
		>
			{error}
		</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>

	{#if !loaded}
		<p class="text-xs text-muted">Loading…</p>
	{:else if documents.length === 0}
		<!--
			Said rather than hidden. "No release on file" is what somebody clearing an asset for use needs to know,
			and an absent panel would make that indistinguishable from the feature not existing.
		-->
		<p class="text-xs text-muted">
			No paperwork on file. Upload a release or licence as an asset, then attach it here.
		</p>
	{:else}
		<ul class="space-y-1">
			{#each documents as document (document.asset_id)}
				<li
					class="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-md border border-line px-2 py-1.5 text-xs"
				>
					<span class="rounded bg-raised px-1.5 py-0.5 font-medium">
						{KINDS[document.kind] ?? document.kind}
					</span>
					<span class="min-w-0 flex-1 truncate">{document.filename}</span>
					<span class="text-muted tabular-nums">{bytes(document.bytes)}</span>
					{#if document.uploaded_by}
						<span class="text-muted">{document.uploaded_by.name}</span>
					{/if}
					<button
						type="button"
						class="underline disabled:opacity-50"
						disabled={busy}
						onclick={() =>
							(confirmingRemoval =
								confirmingRemoval === document.asset_id ? null : document.asset_id)}
					>
						Detach
					</button>

					{#if confirmingRemoval === document.asset_id}
						<div class="w-full rounded-md bg-surface p-2 text-[11px]">
							<p>
								Detaching returns {document.filename} to the library as an ordinary asset. Nothing is
								deleted, and you can attach it again.
							</p>
							<span class="mt-1 flex items-center gap-2">
								<button
									type="button"
									class="rounded-md bg-accent px-2 py-0.5 font-medium text-accent-fg disabled:opacity-50"
									disabled={busy}
									onclick={() => detach(document)}
								>
									Detach
								</button>
								<button type="button" class="underline" onclick={() => (confirmingRemoval = null)}>
									Cancel
								</button>
							</span>
						</div>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
</section>
