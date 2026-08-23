<!--
	Taking a copy (Q.11d).

	The detail panel has said "the original is available" since F.3, with nothing to press. This is the press, plus
	the named formats a tenant offers.

	## The description is the interface

	A list of sizes makes somebody guess. Every format carries a sentence written by whoever configured it, and that
	sentence is why the conversions table exists rather than a hard-coded set of dimensions. So it is shown, not
	tucked behind a tooltip.

	## Preparing is a state, not an error

	The first person to ask for a format waits while it is rendered. The server says so — 202 with
	`status: 'rendering'` — and this polls until the URL arrives. Showing "failed" or a dead button would be wrong
	twice: nothing failed, and the thing they asked for is on its way.

	## The use is declared, and a default is not a declaration

	Channel and territory decide the rights answer, so the panel asks before it takes a copy — and only counts an
	answer as one when the person supplied both. Leaving them alone downloads under the server's default and the
	ledger records that nobody was asked, which is the distinction the whole capture exists for. The choice
	persists while the panel is open, because somebody taking six assets for one campaign should say so once.

	## The refusal is the server's own words

	Rights are evaluated when the URL is minted, so a download can be refused for a reason the person can act on: a
	licence that does not cover this use, a format that needs a permission. The reason is displayed as sent rather
	than replaced with "could not download", which is the version that wastes an afternoon.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		ApiError,
		loadDownloadOptions,
		loadUsageOptions,
		requestDownload,
		type DownloadOptions,
		type UsageOptions
	} from '$lib/api/client';

	let { assetId }: { assetId: string } = $props();

	let options = $state<DownloadOptions | null>(null);
	let vocabulary = $state<UsageOptions | null>(null);
	/** The declared use. Empty means "not answered", which is what makes the record honest. */
	let channel = $state('');
	let territory = $state('');
	let error = $state('');
	let notice = $state('');
	let loaded = $state(false);
	/** The format currently being fetched or rendered, so one button at a time says what is happening. */
	let busy = $state<string | null>(null);

	let shownFor: string | null = null;

	$effect(() => {
		const id = assetId;
		// The id, not the prop object: the parent replaces the asset on every refresh, and refetching then would
		// make opening one asset cost several requests.
		if (id === shownFor) return;
		shownFor = id;
		untrack(() => {
			error = '';
			notice = '';
			busy = null;
			loaded = false;
			void load();
		});
	});

	async function load() {
		try {
			// Both, and `allSettled` rather than `all`: a tenant with no licences has no vocabulary to offer, and
			// that must not remove the download button. The lesson is old here — one failing read used to empty a
			// whole panel.
			const [formats, uses] = await Promise.allSettled([
				loadDownloadOptions(assetId),
				loadUsageOptions()
			]);
			if (uses.status === 'fulfilled') vocabulary = uses.value;
			if (formats.status === 'rejected') throw formats.reason;
			options = formats.value;
		} catch (caught) {
			// A reader without download scope gets a 403 here, which is not a fault: they may look and not take.
			// The panel says nothing at all in that case rather than showing an error about a thing they were
			// never offered.
			if (caught instanceof ApiError && caught.status === 403) {
				options = null;
			} else {
				error =
					caught instanceof ApiError ? caught.message : 'Could not read the download formats.';
			}
		} finally {
			loaded = true;
		}
	}

	/** How long to keep asking while a format is rendered, and how often. */
	const POLL_MS = 1500;
	const POLL_ATTEMPTS = 20;

	function choose(format: string, label: string) {
		busy = format;
		error = '';
		notice = '';
		void (async () => {
			try {
				for (let attempt = 0; attempt < POLL_ATTEMPTS; attempt += 1) {
					const issued = await requestDownload(assetId, format, declaredUse());
					if (issued.url) {
						// A navigation, not a fetch: the URL redirects to the object store, and following it in
						// script would pull the bytes through the page for no reason.
						window.location.href = issued.url;
						notice = `${label} is downloading.`;
						return;
					}
					notice = `${label} is being prepared…`;
					await new Promise((resolve) => setTimeout(resolve, POLL_MS));
				}
				// Still not ready. Not an error either: a large source takes a while, and the render continues
				// whether or not this panel is still watching.
				notice = `${label} is still being prepared. Try again in a moment.`;
			} catch (caught) {
				// The server's own sentence — a rights verdict with its codes, or the permission a format needs.
				error = caught instanceof ApiError ? caught.message : 'That download was refused.';
			} finally {
				busy = null;
			}
		})();
	}

	function sizeOf(format: { max_width: number; max_height: number }): string {
		return `${format.max_width} × ${format.max_height}`;
	}

	/**
	 * The use to send, or nothing.
	 *
	 * Both fields or neither: half an answer would be recorded as a full declaration by a server that only sees
	 * what arrived, and the record would then claim more than the person was asked.
	 */
	function declaredUse(): { channel: string; territory: string } | undefined {
		return channel && territory ? { channel, territory } : undefined;
	}
</script>

<!--
	Rendered when there is *either* a list or something to say. The first version gated the whole section
	on `options`, which made the error banner inside it unreachable: a failed options request left
	`options` null, so the panel silently showed nothing at all. Mutation testing found it — the mutation
	that turned a reader's 403 into an error survived, because the error had nowhere to appear.
-->
{#if loaded && (options || error)}
	<section class="space-y-2" aria-label="Download">
		<h3 class="text-xs font-semibold tracking-wide text-muted uppercase">Download</h3>

		{#if error}
			<p
				role="alert"
				class="rounded-md bg-state-rights-denied/18 p-2 text-xs text-state-rights-denied-fg"
			>
				{error}
			</p>
		{/if}
		<p role="status" aria-live="polite" class="text-xs text-muted">{notice}</p>

		{#if options}
			{#if vocabulary && (vocabulary.channels.length > 0 || vocabulary.territories.length > 0)}
				<!--
					Asked, not enforced. Leaving these alone downloads under the server's default and the record
					says nobody was asked — which is the honest outcome and better than a required field somebody
					fills in at random to get past it.
				-->
				<div class="grid grid-cols-2 gap-2">
					<label class="text-xs text-muted">
						What for
						<select
							bind:value={channel}
							class="mt-0.5 w-full rounded-md border border-line bg-raised px-1.5 py-1 text-xs"
						>
							<option value="">Not stated ({vocabulary.default_channel})</option>
							{#each vocabulary.channels as option (option)}
								<option value={option}>{option}</option>
							{/each}
						</select>
					</label>
					<label class="text-xs text-muted">
						Where
						<select
							bind:value={territory}
							class="mt-0.5 w-full rounded-md border border-line bg-raised px-1.5 py-1 text-xs"
						>
							<option value="">Not stated ({vocabulary.default_territory})</option>
							{#each vocabulary.territories as option (option)}
								<option value={option}>{option}</option>
							{/each}
						</select>
					</label>
				</div>
				<p class="text-xs text-muted">
					{#if channel && territory}
						Recorded against this asset as {channel} in {territory}.
					{:else}
						Stating the use records it against the asset and is what the licence is checked against.
					{/if}
				</p>
			{/if}

			<ul class="space-y-1">
				{#if options.original_available}
					<li>
						<button
							type="button"
							class="w-full rounded-md bg-accent px-3 py-2 text-left text-xs text-accent-fg disabled:opacity-50"
							disabled={busy !== null}
							onclick={() => choose('original', 'The original file')}
						>
							<span class="font-medium">Original file</span>
							<span class="block">
								Exactly what was uploaded. Rights are checked when the link is made, so this may
								still be refused.
							</span>
						</button>
					</li>
				{:else}
					<!--
						Said rather than shown disabled. The bytes are in cold storage and need a restore, which is a
						different action from downloading — a dimmed button would invite pressing it.
					-->
					<li class="rounded-md border border-line px-2 py-1.5 text-xs text-muted">
						The original is archived and needs a restore before it can be downloaded.
					</li>
				{/if}

				{#each options.conversions as format (format.id)}
					<li class="rounded-md border border-line px-2 py-1.5">
						<button
							type="button"
							class="w-full text-left text-xs disabled:opacity-50"
							disabled={busy !== null}
							onclick={() => choose(format.key, format.label)}
						>
							<span class="font-medium">{format.label}</span>
							<span class="text-muted">
								&nbsp;· {format.format.toUpperCase()} · up to {sizeOf(format)}
							</span>
							<!-- The sentence somebody wrote for exactly this moment. -->
							<span class="block text-muted">{format.description}</span>
						</button>
					</li>
				{/each}
			</ul>

			{#if options.conversions.length === 0}
				<!--
					Named by class, so the answer is "nobody has set formats up for videos" rather than a blank space
					that reads as a fault.
				-->
				<p class="text-xs text-muted">
					No prepared formats for {options.media_class} assets yet.
				</p>
			{/if}
		{/if}
	</section>
{/if}
