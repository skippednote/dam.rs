<script lang="ts">
	/**
	 * The near-duplicate review queue.
	 *
	 * ## Two pictures, side by side, at a size you can judge
	 *
	 * The whole task is "are these the same thing". Everything else on the row — the filename, the size, the
	 * distance — is secondary to seeing them, so the images are as large as the row allows and the metadata sits
	 * under them rather than beside them.
	 *
	 * ## Three verdicts, and one of them is honest about doing nothing
	 *
	 * "Merged" records a decision and merges nothing. The server cannot decide which asset survives or what
	 * happens to the other's rights and references, so the button says what it does — a control that silently
	 * deleted one of two licensed deliverables would be the worst thing on this screen.
	 *
	 * ## The distance is explained, not just printed
	 *
	 * "3 of 64 bits differ" means nothing to a person. The row says what the number implies — nearly identical,
	 * or a variant worth looking at — and the sentence under the list says where the number comes from.
	 *
	 * ## An empty queue is the good outcome
	 *
	 * And it says which of the two things it means: nothing found, or nothing you can see both halves of.
	 */
	import { onMount } from 'svelte';
	import {
		ApiError,
		deliveryUrl,
		listDuplicates,
		resolveDuplicate,
		type DuplicateCandidate
	} from '$lib/api/client';
	import { session } from '$lib/api/session.svelte';

	let pairs = $state<DuplicateCandidate[]>([]);
	let loading = $state(true);
	let error = $state('');
	let notice = $state('');
	let busy = $state('');

	async function load() {
		try {
			pairs = await listDuplicates();
		} catch (caught) {
			error = caught instanceof ApiError ? caught.message : 'Could not read the queue.';
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		if (!session.connected) {
			loading = false;
			return;
		}
		void load();
	});

	async function decide(pair: DuplicateCandidate, state: 'confirmed' | 'dismissed' | 'merged') {
		busy = pair.id;
		error = '';
		try {
			await resolveDuplicate(pair.id, state);
			// Dropped locally rather than refetched: the queue can be long, and a reviewer working down it
			// should not have the list reorder under them after every verdict.
			pairs = pairs.filter((one) => one.id !== pair.id);
			notice =
				state === 'merged'
					? `Recorded as the same thing. Nothing was deleted — decide which copy to keep on the assets themselves.`
					: state === 'confirmed'
						? `Recorded as duplicates.`
						: `Dismissed. This pair will not come back.`;
		} catch (caught) {
			error = caught instanceof Error ? caught.message : 'Could not record that.';
		} finally {
			busy = '';
		}
	}

	/** What a Hamming distance means, in words a reviewer can act on. */
	function alikeness(hamming: number | null | undefined): string {
		if (hamming === null || hamming === undefined) return 'similarity unknown';
		if (hamming === 0) return 'pixel-identical after scaling';
		if (hamming <= 2) return 'almost certainly the same picture';
		if (hamming <= 6) return 'very likely the same picture';
		return 'possibly a crop, recolour or re-edit';
	}

	function size(bytes: number): string {
		const units = ['B', 'KiB', 'MiB', 'GiB'];
		let value = bytes;
		let unit = 0;
		while (value >= 1024 && unit < units.length - 1) {
			value /= 1024;
			unit += 1;
		}
		return `${value < 10 && unit > 0 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
	}
</script>

<svelte:head><title>Duplicates · damrs</title></svelte:head>

<div class="space-y-6 p-4">
	<header class="space-y-1">
		<h1 class="text-lg font-semibold tracking-tight">Possible duplicates</h1>
		<p class="max-w-2xl text-sm text-muted">
			Pairs that look alike, found by comparing perceptual hashes — so a re-export, a rescale or a
			re-compression of the same picture is caught even though the files differ. Identical files
			never reach here: the library stores those once.
		</p>
	</header>

	{#if error}
		<p role="alert" class="text-sm text-state-rights-denied-fg">{error}</p>
	{/if}
	<p role="status" aria-live="polite" class="sr-only">{notice}</p>
	{#if notice}
		<p class="text-xs text-muted">{notice}</p>
	{/if}

	{#if !session.connected}
		<p class="text-sm text-muted">Not connected.</p>
	{:else if loading}
		<p class="text-sm text-muted">Loading…</p>
	{:else if pairs.length === 0}
		<p class="max-w-2xl text-sm text-muted">
			Nothing to review. Either no near-duplicates were found, or none where you can see both sides
			— a pair is only shown when you have access to both of its assets.
		</p>
	{:else}
		<p class="text-xs text-muted">
			{pairs.length}
			{pairs.length === 1 ? 'pair' : 'pairs'} · most alike first
		</p>

		<ul class="space-y-4">
			{#each pairs as pair (pair.id)}
				<li class="space-y-3 rounded-md border border-line p-3">
					<div class="flex flex-wrap items-baseline gap-3">
						<span class="text-sm font-medium">{alikeness(pair.hamming)}</span>
						{#if pair.hamming !== null && pair.hamming !== undefined}
							<span class="text-xs text-muted tabular-nums">
								{pair.hamming} of 64 bits differ
							</span>
						{/if}
						{#if pair.relation}
							<span class="rounded border border-line px-1.5 py-0.5 text-xs">
								{pair.relation === 'near_identical' ? 'near identical' : pair.relation}
							</span>
						{/if}
					</div>

					<!-- The two pictures, as large as the row allows: the task is looking at them. -->
					<div class="grid grid-cols-2 gap-3">
						{#each [pair.left, pair.right] as side (side.asset_id)}
							<figure class="space-y-1">
								{#if side.thumbnail_url}
									<img
										src={deliveryUrl(side.thumbnail_url)}
										alt={side.filename}
										loading="lazy"
										class="h-40 w-full rounded border border-line object-contain"
									/>
								{:else}
									<div
										class="flex h-40 w-full items-center justify-center rounded border border-line text-xs text-muted"
									>
										no preview rendered
									</div>
								{/if}
								<figcaption class="space-y-0.5 text-xs">
									<p class="truncate font-medium">{side.filename}</p>
									<p class="text-muted">{side.mime} · {size(side.bytes)}</p>
								</figcaption>
							</figure>
						{/each}
					</div>

					<div class="flex flex-wrap items-center gap-2">
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === pair.id}
							onclick={() => decide(pair, 'dismissed')}
						>
							Not duplicates
						</button>
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === pair.id}
							onclick={() => decide(pair, 'confirmed')}
						>
							Duplicates, keep both
						</button>
						<button
							type="button"
							class="rounded-md border border-line px-2.5 py-1 text-xs hover:bg-raised disabled:opacity-50"
							disabled={busy === pair.id}
							onclick={() => decide(pair, 'merged')}
						>
							Duplicates, one should go
						</button>
						<!--
							Said plainly, because the button cannot do what its name suggests: deciding which of two
							licensed deliverables survives is not something this screen can get right on somebody's
							behalf. It records the judgement; a person acts on it.
						-->
						<p class="text-xs text-muted">
							Recording a verdict deletes nothing. Delete or archive the copy you do not want on the
							asset itself.
						</p>
					</div>
				</li>
			{/each}
		</ul>

		<p class="max-w-2xl text-xs text-muted">
			The number is a Hamming distance between two 64-bit perceptual hashes: how many bits differ. A
			re-encode moves one or two, a rescale two to four, and a heavy downscale of a fine pattern
			nine or ten. Anything past twelve is not shown.
		</p>
	{/if}
</div>
